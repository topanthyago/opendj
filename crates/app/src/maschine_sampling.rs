//! Prepare deck captures on a worker; bind the result on the UI thread.
use std::io::Write;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use opendj_engine::machine_capture::{self, SliceMode, Stem};
use opendj_engine::maschine::{MaschineHandle, ZoneFrom, ZoneSpec};
use opendj_engine::track::Audio;
use sha2::{Digest, Sha256};

pub struct Request {
    pub project_generation: u64,
    pub audio: Arc<Audio>,
    pub start_frame: usize,
    pub bpm: f64,
    pub bars: u32,
    pub stem: Stem,
    pub group: usize,
    pub sound: usize,
    /// None assigns the entire capture to one sound; Some fills the group.
    pub slice: Option<SliceMode>,
}

pub struct Prepared {
    project_generation: u64,
    path: PathBuf,
    ranges: Vec<(usize, usize)>,
    group: usize,
    sound: usize,
    waveform: Vec<f32>,
    preserve_zone: bool,
    source_sample: Option<(usize, String)>,
}

struct PendingCapture {
    receiver: mpsc::Receiver<Result<Prepared, String>>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    live: Option<(Arc<Mutex<machine_capture::CaptureHandle>>, u64)>,
}

impl PendingCapture {
    fn cancel(self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some((live, generation)) = self.live {
            let mut capture = live.lock().unwrap_or_else(|error| error.into_inner());
            capture.cancel_generation(generation);
        }
    }
}

/// Call from the UI thread. Poll the returned receiver without blocking, then
/// call `Prepared::bind` on the UI thread (plug-ins may require that thread).
pub fn start(
    request: Request,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> mpsc::Receiver<Result<Prepared, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(prepare(request, &cancelled));
    });
    rx
}

fn prepare(
    request: Request,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Prepared, String> {
    if request.group >= 8 || request.sound >= 16 {
        return Err("invalid target group or sound".into());
    }
    if !request.bpm.is_finite()
        || !(20.0..=400.0).contains(&request.bpm)
        || !(1..=64).contains(&request.bars)
    {
        return Err("invalid tempo or bar count".into());
    }
    if request.slice.is_some() && request.sound != 0 {
        return Err("16 slices require starting at sound 1".into());
    }
    if request.stem != Stem::Mix && request.audio.stems().is_none() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        while !request.audio.is_complete() {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return Err("Capture cancelled".into());
            }
            if std::time::Instant::now() >= deadline {
                return Err("Audio decoding timed out".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        opendj_engine::separation::separate_fast(&request.audio, request.start_frame, || true)?;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let frames = loop {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err("Capture cancelled".into());
        }
        match machine_capture::capture(
            &request.audio,
            request.stem,
            request.start_frame,
            request.bpm,
            request.bars,
        ) {
            Ok(frames) => break frames,
            Err(e)
                if e.contains("still processing")
                    && std::time::Instant::now() < deadline
                    && request
                        .audio
                        .stems()
                        .is_some_and(|s| !s.current().is_complete()) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    };
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err("Capture cancelled".into());
    }
    prepare_frames(
        frames,
        request.audio.sample_rate,
        request.group,
        request.sound,
        request.slice,
        request.project_generation,
    )
}

fn prepare_frames(
    frames: Vec<[f32; 2]>,
    rate: u32,
    group: usize,
    sound: usize,
    slice: Option<SliceMode>,
    project_generation: u64,
) -> Result<Prepared, String> {
    if frames.is_empty() {
        return Err("sample is empty".into());
    }
    let ranges = match &slice {
        Some(mode) => machine_capture::slices(&frames, mode)?
            .windows(2)
            .map(|w| (w[0], w[1] - 1))
            .collect(),
        None => vec![(0, frames.len() - 1)],
    };
    let waveform = (0..512)
        .map(|i| {
            let a = i * frames.len() / 512;
            let b = ((i + 1) * frames.len() / 512).max(a + 1).min(frames.len());
            frames[a..b]
                .iter()
                .fold(0.0f32, |peak, f| peak.max(f[0].abs()).max(f[1].abs()))
        })
        .collect();
    let bytes = wav(&frames, rate)?;
    let hash = Sha256::digest(&bytes);
    let name: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    let root = opendj_engine::paths::config_base()
        .ok_or("no configuration directory")?
        .join(opendj_engine::paths::app_folder_name())
        .join("maschine/samples");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let path = root.join(format!("{name}.wav"));
    // Link a complete temporary file atomically. Never overwrite a user file.
    if !path.exists() {
        let mut tmp = None;
        for _ in 0..10 {
            let p = root.join(format!(
                ".{name}-{}-{}.tmp",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&p)
            {
                Ok(mut file) => {
                    let result = file.write_all(&bytes).and_then(|_| file.sync_all());
                    if let Err(e) = result {
                        let _ = std::fs::remove_file(&p);
                        return Err(e.to_string());
                    }
                    tmp = Some(p);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        let tmp = tmp.ok_or("could not create temporary sample")?;
        let link = std::fs::hard_link(&tmp, &path);
        let _ = std::fs::remove_file(&tmp);
        if let Err(e) = link {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e.to_string());
            }
        }
    }
    if std::fs::read(&path).map_err(|e| e.to_string())? != bytes {
        return Err("sample cache hash collision or corrupt file".into());
    }
    Ok(Prepared {
        project_generation,
        path,
        ranges,
        group,
        sound,
        waveform,
        preserve_zone: false,
        source_sample: None,
    })
}

impl Prepared {
    pub fn bind(
        self,
        handle: &MaschineHandle,
    ) -> Result<mpsc::Receiver<Result<bool, String>>, String> {
        if handle.project_generation() != self.project_generation {
            return Err("Project changed while preparing the sample".into());
        }
        handle.edit_deferred_for_project(self.project_generation, move |doc| {
            doc.transaction("Assign sample", |doc| self.apply(doc))
        })
    }

    fn apply(self, doc: &mut opendj_engine::maschine::Document) -> Result<(), String> {
        if doc.project().groups.get(self.group).is_none() {
            return Err("target group does not exist".into());
        }
        if let Some((source_sound, expected_path)) = &self.source_sample {
            let current = doc.project().groups[self.group]
                .sounds
                .get(*source_sound)
                .and_then(|sound| sound.sampler.as_ref())
                .and_then(|sampler| sampler.zones.first())
                .map(|zone| zone.sample.path.as_str());
            if current != Some(expected_path.as_str()) {
                return Err("Sample changed while preparing the edit; retry".into());
            }
        }
        for (offset, (low, high)) in self.ranges.iter().copied().enumerate() {
            let sound = self.sound + offset;
            let target = doc.project().groups[self.group]
                .sounds
                .get(sound)
                .ok_or("target sound does not exist")?;
            if target.sampler.is_none() {
                crate::maschine_ui::load_native(doc, self.group, sound, "Sampler")?;
            }
            if self.preserve_zone {
                let count = doc.project().groups[self.group].sounds[sound]
                    .sampler
                    .as_ref()
                    .map_or(0, |s| s.zones.len());
                if count == 0 {
                    return Err("No sample zone to normalize".into());
                }
                let zones: Vec<_> = (0..count)
                    .map(|i| ZoneSpec {
                        from: ZoneFrom::Zone(i),
                        file: (i == 0).then(|| self.path.clone()),
                        set: vec![],
                    })
                    .collect();
                doc.set_zones(self.group, sound, &zones, "Normalize sample")?;
                continue;
            }
            doc.set_zones(
                self.group,
                sound,
                &[ZoneSpec {
                    from: ZoneFrom::Template,
                    file: Some(self.path.clone()),
                    set: vec![
                        ("play_range/low", low as f64),
                        ("play_range/high", high as f64),
                        ("loop_range/low", low as f64),
                        ("loop_range/high", high as f64),
                    ],
                }],
                "Capture sample",
            )?;
        }
        Ok(())
    }
}

fn wav(frames: &[[f32; 2]], rate: u32) -> Result<Vec<u8>, String> {
    let len = frames
        .len()
        .checked_mul(4)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or("capture too large")?;
    let size = len.checked_add(36).ok_or("capture too large")?;
    let byte_rate = rate.checked_mul(4).ok_or("invalid sample rate")?;
    let mut out = Vec::with_capacity(len as usize + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&len.to_le_bytes());
    for frame in frames {
        for sample in frame {
            out.extend_from_slice(
                &((sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16).to_le_bytes(),
            );
        }
    }
    Ok(out)
}

/// Register capture controls for both the app and the supported-controls listing.
pub fn create_controls(registry: &opendj_control::ControlRegistry) {
    use opendj_control::Behavior;
    let group = "[Maschine3]";
    registry.create(group, "sampling_source", 0.0, 0.0, 3.0, Behavior::Free);
    registry.create(group, "sampling_stem", 0.0, 0.0, 4.0, Behavior::Free);
    registry.create(group, "sampling_bars", 4.0, 1.0, 16.0, Behavior::Free);
    registry.create(group, "sampling_slice", 0.0, 0.0, 2.0, Behavior::Free);
    registry.push(group, "sampling_capture");
    registry.push(group, "sampling_cancel");
    registry.indicator(group, "sampling_recording", 1.0);
    registry.indicator(group, "sampling_waiting", 1.0);
}

/// Wire deck capture to the Maschine panel. Keep both timers alive with the window.
pub fn install(
    app: &crate::AppWindow,
    engine: std::rc::Rc<opendj_engine::AudioEngine>,
    registry: Arc<opendj_control::ControlRegistry>,
) -> (slint::Timer, slint::Timer) {
    let editor_timer = install_editor(app, engine.clone(), registry.clone());
    create_controls(&registry);
    let group = "[Maschine3]";
    let recording = registry
        .get(group, "sampling_recording")
        .expect("sampling recording");
    let waiting = registry
        .get(group, "sampling_waiting")
        .expect("sampling waiting");
    use slint::ComponentHandle;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;
    let pending: Rc<RefCell<Option<PendingCapture>>> = Rc::new(RefCell::new(None));
    let weak = app.as_weak();
    let engine_capture = engine.clone();
    let controls = registry.clone();
    let pending_capture = pending.clone();
    let capture_recording = recording.clone();
    let capture_waiting = waiting.clone();
    app.global::<crate::MaschineUi>()
        .on_sample_capture(move |source, stem, bars, slice| {
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<crate::MaschineUi>();
            if pending_capture.borrow().is_some() {
                ui.set_status("A capture is already being prepared".into());
                return;
            }
            if !(0..4).contains(&source) {
                ui.set_status("Choose a source deck".into());
                return;
            }
            let Some(target) = engine_capture.maschine(2) else {
                ui.set_status("Maschine deck unavailable".into());
                return;
            };
            let project_generation = target.project_generation();
            if source >= 2 {
                if stem != 0 {
                    ui.set_status("Live instrument capture requires Mix".into());
                    return;
                }
                let Some(live) = engine_capture.machine_capture(source as usize) else {
                    ui.set_status("Live capture unavailable".into());
                    return;
                };
                let read =
                    |group: &str, name: &str| controls.get(group, name).map_or(0.0, |c| c.get());
                let bpm = read(&format!("[Groove{}]", source + 1), "bpm");
                let rate = engine_capture.sample_rate() as u32;
                if !(20.0..=400.0).contains(&bpm) || bars <= 0 || bars > 16 {
                    ui.set_status("Invalid tempo or length".into());
                    return;
                }
                let group = read("[Maschine3]", "group").round() as usize;
                let sound = if slice == 0 {
                    read("[Maschine3]", "sound").round() as usize
                } else {
                    0
                };
                let mode = match slice {
                    1 => Some(SliceMode::Grid),
                    2 => Some(SliceMode::Transients),
                    _ => None,
                };
                let count = (rate as f64 * 60.0 / bpm * 4.0 * bars as f64).round() as usize;
                let (tx, rx) = mpsc::channel();
                let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let worker_cancel = cancel.clone();
                let capture_generation = match live.lock() {
                    Ok(mut capture) => match capture.start(count) {
                        Ok(()) => capture.generation(),
                        Err(error) => {
                            ui.set_status(error.into());
                            return;
                        }
                    },
                    Err(_) => {
                        ui.set_status("Live capture unavailable".into());
                        return;
                    }
                };
                let worker_live = live.clone();
                std::thread::spawn(move || {
                    let result = (|| loop {
                        if worker_cancel.load(std::sync::atomic::Ordering::Acquire) {
                            worker_live
                                .lock()
                                .map_err(|_| "Live capture unavailable")?
                                .cancel_generation(capture_generation);
                            break Err("Capture cancelled".into());
                        }
                        let frames = worker_live
                            .lock()
                            .map_err(|_| "Live capture unavailable")?
                            .poll_generation(capture_generation)?;
                        if let Some(frames) = frames {
                            if worker_cancel.load(std::sync::atomic::Ordering::Acquire) {
                                break Err("Capture cancelled".into());
                            }
                            break prepare_frames(
                                frames,
                                rate,
                                group,
                                sound,
                                mode,
                                project_generation,
                            );
                        }
                        std::thread::sleep(Duration::from_millis(40));
                    })();
                    let _ = tx.send(result);
                });
                *pending_capture.borrow_mut() = Some(PendingCapture {
                    receiver: rx,
                    cancelled: cancel,
                    live: Some((live, capture_generation)),
                });
                capture_recording.set_bool(true);
                ui.set_status("Recording live instrument…".into());
                return;
            }
            let track = engine_capture
                .loaded_tracks()
                .read()
                .ok()
                .and_then(|t| t[source as usize].clone());
            let Some(track) = track else {
                ui.set_status("Source deck has no track".into());
                return;
            };
            let Some(audio) = track.audio.clone() else {
                ui.set_status("Source audio is not ready".into());
                return;
            };
            let value =
                |group: &str, name: &str| controls.get(group, name).map_or(0.0, |v| v.get());
            let position =
                value(&format!("[Channel{}]", source + 1), "playposition").clamp(0.0, 1.0);
            let request = Request {
                project_generation,
                start_frame: (position * audio.len() as f64) as usize,
                audio,
                bpm: track.bpm,
                bars: bars.max(0) as u32,
                stem: match stem {
                    1 => Stem::Drums,
                    2 => Stem::Bass,
                    3 => Stem::Melody,
                    4 => Stem::Voice,
                    _ => Stem::Mix,
                },
                group: value("[Maschine3]", "group").round() as usize,
                sound: if slice == 0 {
                    value("[Maschine3]", "sound").round() as usize
                } else {
                    0
                },
                slice: match slice {
                    1 => Some(SliceMode::Grid),
                    2 => Some(SliceMode::Transients),
                    _ => None,
                },
            };
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            *pending_capture.borrow_mut() = Some(PendingCapture {
                receiver: start(request, cancel.clone()),
                cancelled: cancel,
                live: None,
            });
            capture_waiting.set_bool(true);
            ui.set_status("Preparing capture…".into());
        });
    let cancel_pending = pending.clone();
    let cancel_recording = recording.clone();
    let cancel_waiting = waiting.clone();
    let weak_cancel = app.as_weak();
    app.global::<crate::MaschineUi>().on_sample_cancel(move || {
        if let Some(capture) = cancel_pending.borrow_mut().take() {
            capture.cancel();
            cancel_recording.set_bool(false);
            cancel_waiting.set_bool(false);
            if let Some(app) = weak_cancel.upgrade() {
                app.global::<crate::MaschineUi>()
                    .set_status("Capture cancelled".into());
            }
        }
    });
    let weak = app.as_weak();
    let poll_registry = registry.clone();
    let mut capture_version = poll_registry
        .get("[Maschine3]", "sampling_capture")
        .map_or(0, |c| c.version());
    let mut cancel_version = poll_registry
        .get("[Maschine3]", "sampling_cancel")
        .map_or(0, |c| c.version());
    let mut capture_down = false;
    let mut cancel_down = false;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            if let Some(app) = weak.upgrade() {
                let ui = app.global::<crate::MaschineUi>();
                if let Some(control) = poll_registry.get("[Maschine3]", "sampling_capture") {
                    let version = control.version();
                    let down = control.get_bool();
                    let pressed = !capture_down && (down || version != capture_version);
                    capture_version = version;
                    capture_down = down;
                    if pressed {
                        let read = |name: &str, default: i32| {
                            poll_registry
                                .get("[Maschine3]", name)
                                .map_or(default, |v| v.get().round() as i32)
                        };
                        ui.invoke_sample_capture(
                            read("sampling_source", 0),
                            read("sampling_stem", 0),
                            read("sampling_bars", 4),
                            read("sampling_slice", 0),
                        );
                    }
                }
                if let Some(control) = poll_registry.get("[Maschine3]", "sampling_cancel") {
                    let version = control.version();
                    let down = control.get_bool();
                    let pressed = !cancel_down && (down || version != cancel_version);
                    cancel_version = version;
                    cancel_down = down;
                    if pressed {
                        ui.invoke_sample_cancel();
                    }
                }
            }
            let result = pending.borrow_mut().as_ref().and_then(|capture| {
                match capture.receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some(Err("Capture worker stopped".into()))
                    }
                }
            });
            let Some(result) = result else { return };
            *pending.borrow_mut() = None;
            recording.set_bool(false);
            waiting.set_bool(false);
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<crate::MaschineUi>();
            let result = result.and_then(|prepared| {
                let mut image = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(512, 80);
                for (x, peak) in prepared.waveform.iter().enumerate() {
                    let height = (peak.clamp(0.0, 1.0) * 39.0).round() as usize;
                    for y in 40usize.saturating_sub(height)..=(40 + height).min(79) {
                        image.make_mut_slice()[y * 512 + x] = slint::Rgba8Pixel {
                            r: 100,
                            g: 210,
                            b: 180,
                            a: 255,
                        };
                    }
                }
                let handle = engine.maschine(2).ok_or("Maschine deck unavailable")?;
                let receipt = prepared.bind(&handle)?;
                watch_edit(receipt, app.as_weak());
                ui.set_sample_waveform(slint::Image::from_rgba8(image));
                ui.set_sample_name("Captured sample".into());
                ui.set_sample_start(0.0);
                ui.set_sample_end(1.0);
                ui.set_sample_loaded(true);
                Ok(())
            });
            ui.set_status(match result {
                Ok(()) => "Finishing capture…".into(),
                Err(e) => e.into(),
            });
        },
    );
    (timer, editor_timer)
}

fn selected(reg: &opendj_control::ControlRegistry) -> (usize, usize) {
    let read = |name| {
        reg.get("[Maschine3]", name)
            .map_or(0, |c| c.get().round().max(0.0) as usize)
    };
    (read("group").min(7), read("sound").min(15))
}

fn wave_peaks(frames: &[[f32; 2]]) -> Vec<f32> {
    (0..512)
        .map(|x| {
            let a = x * frames.len() / 512;
            let b = ((x + 1) * frames.len() / 512).max(a + 1).min(frames.len());
            frames
                .get(a..b)
                .unwrap_or(&[])
                .iter()
                .fold(0.0f32, |v, f| v.max(f[0].abs()).max(f[1].abs()))
        })
        .collect()
}
fn wave_image(peaks: &[f32]) -> slint::Image {
    let mut image = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(512, 80);
    for (x, peak) in peaks.iter().take(512).enumerate() {
        let height = (peak.clamp(0.0, 1.0) * 39.0).round() as usize;
        for y in 40usize.saturating_sub(height)..=(40 + height).min(79) {
            image.make_mut_slice()[y * 512 + x] = slint::Rgba8Pixel {
                r: 100,
                g: 210,
                b: 180,
                a: 255,
            };
        }
    }
    slint::Image::from_rgba8(image)
}

fn install_editor(
    app: &crate::AppWindow,
    engine: std::rc::Rc<opendj_engine::AudioEngine>,
    registry: Arc<opendj_control::ControlRegistry>,
) -> slint::Timer {
    use opendj_engine::maschine::{Target, Value};
    use slint::ComponentHandle;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;
    let selection = Rc::new(RefCell::new((0.0f32, 1.0f32)));
    let markers = Rc::new(RefCell::new(vec![0.0f32, 1.0f32]));
    app.global::<crate::MaschineUi>()
        .set_sample_markers(slint::ModelRc::from(markers.borrow().as_slice()));
    let sample_length = Rc::new(RefCell::new((usize::MAX, usize::MAX, 0usize)));
    let (prepared_tx, prepared_rx) = mpsc::channel::<Result<Prepared, String>>();
    let weak = app.as_weak();
    let selection_range = selection.clone();
    app.global::<crate::MaschineUi>()
        .on_sample_range(move |start, end| {
            *selection_range.borrow_mut() = (start, end);
            if let Some(app) = weak.upgrade() {
                let ui = app.global::<crate::MaschineUi>();
                ui.set_sample_start(start);
                ui.set_sample_end(end);
            }
        });
    let weak = app.as_weak();
    let actions_engine = engine.clone();
    let actions_registry = registry.clone();
    let actions_selection = selection.clone();
    let actions_markers = markers.clone();
    let actions_length = sample_length.clone();
    let marker_values = markers.clone();
    let marker_weak = app.as_weak();
    app.global::<crate::MaschineUi>()
        .on_sample_marker(move |position| {
            let Some(app) = marker_weak.upgrade() else {
                return;
            };
            let mut points = marker_values.borrow_mut();
            let p = position.clamp(0.0, 1.0);
            if points.len() < 17
                && p > 0.0
                && p < 1.0
                && !points.iter().any(|x| (*x - p).abs() < 0.00001)
            {
                points.push(p);
                points.sort_by(f32::total_cmp);
            }
            app.global::<crate::MaschineUi>()
                .set_sample_markers(slint::ModelRc::from(points.as_slice()));
        });
    let edit_engine = engine.clone();
    let edit_registry = registry.clone();
    let edit_length = sample_length.clone();
    let weak_edit = app.as_weak();
    app.global::<crate::MaschineUi>()
        .on_sample_edit(move |field, value| {
            let Some(app) = weak_edit.upgrade() else {
                return;
            };
            let ui = app.global::<crate::MaschineUi>();
            let (group, sound) = selected(&edit_registry);
            let Some(handle) = edit_engine.maschine(2) else {
                ui.set_status("Maschine deck unavailable".into());
                return;
            };
            let project_generation = handle.project_generation();
            let (loaded_group, loaded_sound, frames) = *edit_length.borrow();
            if (group, sound) != (loaded_group, loaded_sound) {
                ui.set_status("Sample is still loading".into());
                return;
            }
            if frames == 0 {
                ui.set_status("Select a loaded sample".into());
                return;
            }
            let (key, number) = match field.as_str() {
                "gain" => ("gain", value.clamp(0.0, 2.0) as f64),
                "pitch" => ("tune", value.clamp(-24.0, 24.0) as f64),
                "fade-in" => (
                    "attack",
                    (value.clamp(0.0, 1.0) * frames as f32).round() as f64,
                ),
                "fade-out" => (
                    "decay",
                    (value.clamp(0.0, 1.0) * frames as f32).round() as f64,
                ),
                "loop" => ("loop_active", if value > 0.0 { 1.0 } else { 0.0 }),
                _ => {
                    ui.set_status("Unknown sample parameter".into());
                    return;
                }
            };
            let result = handle.edit_deferred_for_project(project_generation, move |d| {
                d.set(
                    "Edit sample",
                    Target::Zone(group, sound, 0),
                    key,
                    Value::Float(number),
                )
            });
            match result {
                Ok(receipt) => watch_edit(receipt, app.as_weak()),
                Err(e) => ui.set_status(e.into()),
            }
        });
    app.global::<crate::MaschineUi>()
        .on_sample_action(move |action| {
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<crate::MaschineUi>();
            let (group, sound) = selected(&actions_registry);
            let Some(handle) = actions_engine.maschine(2) else {
                ui.set_status("Maschine deck unavailable".into());
                return;
            };
            let project_generation = handle.project_generation();
            let name = action.as_str();
            if name == "clear-markers" {
                *actions_markers.borrow_mut() = vec![0.0, 1.0];
                ui.set_sample_markers(slint::ModelRc::from(actions_markers.borrow().as_slice()));
                return;
            }
            if name == "load" {
                let tx = prepared_tx.clone();
                crate::prefs::choose_file("Load sample", move |path| {
                    let result = opendj_engine::decode::Decoder::open(&path)
                        .and_then(|d| d.run(|_| true))
                        .and_then(|audio| {
                            let frames = (0..audio.len()).map(|i| audio.frame(i)).collect();
                            prepare_frames(
                                frames,
                                audio.sample_rate,
                                group,
                                sound,
                                None,
                                project_generation,
                            )
                        });
                    let _ = tx.send(result);
                });
                return;
            }
            if name == "NORMALIZE" || name == "SLICE" {
                let tx = prepared_tx.clone();
                let slice = name == "SLICE";
                let slice_mode = ui.get_sample_slice_mode();
                let points = actions_markers.borrow().clone();
                let expected_path = handle.snapshot_cached().and_then(|p| {
                    p.groups
                        .get(group)
                        .and_then(|g| g.sounds.get(sound))
                        .and_then(|s| s.sampler.as_ref())
                        .and_then(|s| s.zones.first())
                        .map(|z| z.sample.path.clone())
                });
                std::thread::spawn(move || {
                    let result = handle
                        .sample(group, sound, 0)
                        .and_then(|s| s.ok_or_else(|| "No sample in selected sound".to_string()))
                        .and_then(|sample| {
                            let mut frames = sample.frames.clone();
                            if !slice {
                                let peak = frames
                                    .iter()
                                    .fold(0.0f32, |v, f| v.max(f[0].abs()).max(f[1].abs()));
                                if peak > 0.0 {
                                    for f in &mut frames {
                                        f[0] /= peak;
                                        f[1] /= peak;
                                    }
                                }
                            }
                            let mode = if !slice {
                                None
                            } else {
                                Some(match slice_mode {
                                    1 => SliceMode::Transients,
                                    2 => SliceMode::Manual(
                                        points
                                            .iter()
                                            .map(|p| (*p * frames.len() as f32).round() as usize)
                                            .collect(),
                                    ),
                                    _ => SliceMode::Grid,
                                })
                            };
                            prepare_frames(
                                frames,
                                sample.rate,
                                group,
                                if slice { 0 } else { sound },
                                mode,
                                project_generation,
                            )
                            .map(|mut prepared| {
                                prepared.preserve_zone = !slice;
                                prepared.source_sample = expected_path.map(|path| (sound, path));
                                prepared
                            })
                        });
                    let _ = tx.send(result);
                });
                ui.set_status("Preparing sample…".into());
                return;
            }
            let result = match name {
                "TRIM" => {
                    let (loaded_group, loaded_sound, len) = *actions_length.borrow();
                    let len = if (group, sound) == (loaded_group, loaded_sound) {
                        len
                    } else {
                        0
                    };
                    let (start, end) = *actions_selection.borrow();
                    if len == 0 || end <= start {
                        Err("Select a nonempty sample range".into())
                    } else {
                        let low = (start.clamp(0.0, 1.0) * (len - 1) as f32).round() as i64;
                        let high = (end.clamp(0.0, 1.0) * (len - 1) as f32).round() as i64;
                        handle.edit_deferred_for_project(project_generation, move |d| {
                            d.set(
                                "Trim",
                                Target::Zone(group, sound, 0),
                                "play_range/low",
                                Value::Int(low),
                            )?;
                            d.set(
                                "Trim",
                                Target::Zone(group, sound, 0),
                                "play_range/high",
                                Value::Int(high),
                            )?;
                            d.set(
                                "Trim",
                                Target::Zone(group, sound, 0),
                                "loop_range/low",
                                Value::Int(low),
                            )?;
                            d.set(
                                "Trim",
                                Target::Zone(group, sound, 0),
                                "loop_range/high",
                                Value::Int(high),
                            )?;
                            Ok(())
                        })
                    }
                }
                "REVERSE" => {
                    let reverse = handle.snapshot_cached().and_then(|p| {
                        p.groups
                            .get(group)
                            .and_then(|g| g.sounds.get(sound))
                            .and_then(|s| s.sampler.as_ref())
                            .map(|s| !s.reverse)
                    });
                    match reverse {
                        Some(on) => {
                            handle.edit_deferred_for_project(project_generation, move |d| {
                                d.set(
                                    "Reverse",
                                    Target::Sampler(group, sound),
                                    "reverse",
                                    Value::Int(on as i64),
                                )
                            })
                        }
                        None => Err("No sampler in selected sound".into()),
                    }
                }
                _ => Err("Unknown sample action".into()),
            };
            match result {
                Ok(receipt) => {
                    watch_edit(receipt, app.as_weak());
                    ui.set_status("Updating sample…".into());
                }
                Err(e) => ui.set_status(e.into()),
            }
        });
    let weak = app.as_weak();
    let (view_tx, view_rx) = mpsc::channel::<(
        (u64, usize, usize),
        Result<Option<(usize, Vec<f32>)>, String>,
    )>();
    let mut key: Option<(u64, usize, usize)> = None;
    let mut waiting = false;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<crate::MaschineUi>();
            if let Ok(result) = prepared_rx.try_recv() {
                let result = result.and_then(|prepared| {
                    prepared.bind(&engine.maschine(2).ok_or("Maschine deck unavailable")?)
                });
                match result {
                    Ok(receipt) => {
                        watch_edit(receipt, app.as_weak());
                        ui.set_status("Finishing sample…".into());
                    }
                    Err(e) => ui.set_status(e.into()),
                }
                key = None;
            }
            if let Ok((finished_key, result)) = view_rx.try_recv() {
                waiting = false;
                if Some(finished_key) == key
                    && selected(&registry) == (finished_key.1, finished_key.2)
                {
                    match result {
                        Ok(Some((frames, peaks))) if frames > 0 => {
                            *sample_length.borrow_mut() = (finished_key.1, finished_key.2, frames);
                            ui.set_sample_waveform(wave_image(&peaks));
                            ui.set_sample_name("Selected sound".into());
                            ui.set_sample_loaded(true);
                            let (_, group, sound) = finished_key;
                            if let Some(project) =
                                engine.maschine(2).and_then(|h| h.snapshot_cached())
                            {
                                if let Some(zone) = project
                                    .groups
                                    .get(group)
                                    .and_then(|g| g.sounds.get(sound))
                                    .and_then(|s| s.sampler.as_ref())
                                    .and_then(|s| s.zones.first())
                                {
                                    let length = frames as f32;
                                    ui.set_sample_gain(zone.gain as f32);
                                    ui.set_sample_pitch(zone.tune as f32);
                                    ui.set_sample_fade_in(
                                        (zone.fade_in as f32 / length).clamp(0.0, 1.0),
                                    );
                                    ui.set_sample_fade_out(
                                        (zone.fade_out as f32 / length).clamp(0.0, 1.0),
                                    );
                                    ui.set_sample_loop(zone.loop_active);
                                    ui.set_sample_loop_start(
                                        (zone.loop_range.0 as f32 / length).clamp(0.0, 1.0),
                                    );
                                    ui.set_sample_loop_end(
                                        (zone.loop_range.1 as f32 / length).clamp(0.0, 1.0),
                                    );
                                    let start = (zone.play_range.0 as f32 / length).clamp(0.0, 1.0);
                                    let end = (zone.play_range.1 as f32 / length).clamp(0.0, 1.0);
                                    *selection.borrow_mut() = (start, end);
                                    ui.set_sample_start(start);
                                    ui.set_sample_end(end);
                                }
                            }
                        }
                        _ => {
                            *sample_length.borrow_mut() = (usize::MAX, usize::MAX, 0);
                            ui.set_sample_loaded(false);
                        }
                    }
                }
            }
            let Some(handle) = engine.maschine(2) else {
                return;
            };
            let (group, sound) = selected(&registry);
            let new_key = (handle.snapshot_version(), group, sound);
            if key != Some(new_key) && !waiting {
                if key.is_none_or(|old: (u64, usize, usize)| old.1 != group || old.2 != sound) {
                    *markers.borrow_mut() = vec![0.0, 1.0];
                    ui.set_sample_markers(slint::ModelRc::from(markers.borrow().as_slice()));
                }
                key = Some(new_key);
                waiting = true;
                *sample_length.borrow_mut() = (usize::MAX, usize::MAX, 0);
                ui.set_sample_loaded(false);
                let tx = view_tx.clone();
                std::thread::spawn(move || {
                    let result = handle
                        .sample(group, sound, 0)
                        .map(|sample| sample.map(|s| (s.frames.len(), wave_peaks(&s.frames))));
                    let _ = tx.send((new_key, result));
                });
            }
        },
    );
    timer
}

fn watch_edit(receipt: mpsc::Receiver<Result<bool, String>>, weak: slint::Weak<crate::AppWindow>) {
    std::thread::spawn(move || {
        let result = receipt
            .recv()
            .unwrap_or_else(|_| Err("Maschine editor stopped".into()));
        let _ = slint::invoke_from_event_loop(move || {
            use slint::ComponentHandle;
            if let Some(app) = weak.upgrade() {
                app.global::<crate::MaschineUi>().set_status(match result {
                    Ok(_) => "Sample ready".into(),
                    Err(e) => e.into(),
                });
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendj_engine::maschine::Document;

    #[test]
    fn sample_assignment_replaces_synth_and_rolls_back_incomplete_slice_assignment() {
        let root = std::env::temp_dir().join(format!(
            "opendj-sample-assignment-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("sample.wav");
        std::fs::write(&path, wav(&[[0.1, 0.2]; 32], 44_100).unwrap()).unwrap();
        let prepared = |ranges| Prepared {
            project_generation: 1,
            path: path.clone(),
            ranges,
            group: 0,
            sound: 0,
            waveform: Vec::new(),
            preserve_zone: false,
            source_sample: None,
        };
        let mut doc = Document::new_project().unwrap();
        crate::maschine_ui::load_native(&mut doc, 0, 0, "Kick").unwrap();
        doc.transaction("Assign sample", |doc| prepared(vec![(0, 31)]).apply(doc))
            .unwrap();
        let sound = &doc.project().groups[0].sounds[0];
        assert!(sound.sampler.is_some());
        assert_eq!(
            sound
                .slots
                .iter()
                .filter(|slot| slot.is_instrument())
                .count(),
            1
        );
        assert_eq!(sound.slots[0].module, "Sampler");
        assert!(!sound.slots.iter().any(|slot| slot.module == "Kick"));

        let original = doc.to_bytes().unwrap();
        assert!(doc
            .transaction("Assign slices", |doc| prepared(vec![(0, 31); 17])
                .apply(doc))
            .is_err());
        assert_eq!(doc.to_bytes().unwrap(), original);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
