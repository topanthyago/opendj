//! Prepare deck captures on a worker; bind the result on the UI thread.
use std::io::Write;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use opendj_engine::machine_capture::{self, SliceMode};
use opendj_engine::maschine::{MaschineHandle, ZoneFrom, ZoneSpec};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct Prepared {
    project_generation: u64,
    path: PathBuf,
    ranges: Vec<(usize, usize)>,
    group: usize,
    sound: usize,
    waveform: Vec<f32>,
    preserve_zone: bool,
    source_sample: Option<(usize, usize, String)>,
    sample: Arc<opendj_engine::maschine::Sample>,
    beat_markers: Vec<usize>,
    tunes: [f64; 16],
    gains: [f64; 16],
    slice_mode: i32,
    edit_range: (usize, usize),
}

struct PendingCapture {
    receiver: mpsc::Receiver<Result<Prepared, String>>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    live: Option<(Arc<Mutex<machine_capture::CaptureHandle>>, u64)>,
}

impl PendingCapture {
    fn cancel(self) {
        drop(self);
    }
}

impl Drop for PendingCapture {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some((live, generation)) = &self.live {
            live.lock()
                .unwrap_or_else(|error| error.into_inner())
                .cancel_generation(*generation);
        }
    }
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
    let sample = Arc::new(opendj_engine::maschine::Sample { rate, frames });
    let mut starts = match &slice {
        Some(SliceMode::Grid) => {
            opendj_engine::maschine::sample_edit::split_positions((0, sample.frames.len()), 16)
        }
        Some(SliceMode::Transients) => opendj_engine::maschine::sample_edit::detect_positions(
            &sample,
            (0, sample.frames.len()),
            0.5,
            true,
        ),
        Some(SliceMode::Manual(points)) => points
            .iter()
            .copied()
            .filter(|point| *point < sample.frames.len())
            .collect(),
        None => vec![0],
    };
    starts.retain(|start| *start < sample.frames.len());
    starts.sort_unstable();
    starts.dedup();
    starts.truncate(16);
    if starts.is_empty() {
        starts.push(0);
    }
    let ranges = starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            (
                *start,
                starts
                    .get(index + 1)
                    .copied()
                    .unwrap_or(sample.frames.len())
                    - 1,
            )
        })
        .collect();
    let frames = &sample.frames;
    let waveform = (0..512)
        .map(|i| {
            let a = i * frames.len() / 512;
            let b = ((i + 1) * frames.len() / 512).max(a + 1).min(frames.len());
            frames[a..b]
                .iter()
                .fold(0.0f32, |peak, f| peak.max(f[0].abs()).max(f[1].abs()))
        })
        .collect();
    let path = store_sample(&frames, rate)?;
    Ok(Prepared {
        project_generation,
        path,
        ranges,
        group,
        sound,
        waveform,
        preserve_zone: false,
        source_sample: None,
        edit_range: (0, sample.frames.len()),
        sample,
        beat_markers: Vec::new(),
        tunes: [0.0; 16],
        gains: [1.0; 16],
        slice_mode: match slice {
            Some(SliceMode::Transients) => 1,
            Some(SliceMode::Grid) => 0,
            _ => 2,
        },
    })
}

/// Store immutable, content-addressed audio shared by captures and offline edits.
pub(crate) fn store_sample(frames: &[[f32; 2]], rate: u32) -> Result<PathBuf, String> {
    let bytes = wav(frames, rate)?;
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
    Ok(path)
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

    fn bind_staged(
        self,
        handle: &MaschineHandle,
        replace: bool,
    ) -> Result<mpsc::Receiver<Result<bool, String>>, String> {
        let generation = self.project_generation;
        if handle.project_generation() != generation {
            return Err("Project changed while preparing the sample".into());
        }
        handle.edit_deferred_for_project(generation, move |doc| {
            doc.transaction("Apply captured sample to Cue", move |doc| {
                self.apply_staged(doc, replace)
            })
        })
    }

    fn apply_staged(
        self,
        doc: &mut opendj_engine::maschine::Document,
        replace: bool,
    ) -> Result<(), String> {
        let (_, cue_bus) = opendj_engine::maschine::cue_playback_project(doc.project())?;
        if cue_bus >= 16 {
            return Err(
                "All SDK outputs are assigned. Free a stereo output before applying to Cue".into(),
            );
        }
        let group = doc
            .project()
            .groups
            .get(self.group)
            .ok_or("Target Group is unavailable")?;
        if self.ranges.is_empty()
            || self.sound + self.ranges.len() > group.sounds.len()
            || self.ranges.len() > 16
        {
            return Err("Choose a destination with enough Sound slots".into());
        }
        if !replace
            && group.sounds[self.sound..self.sound + self.ranges.len()]
                .iter()
                .any(|sound| {
                    !sound.slots.is_empty() || sound.sampler.is_some() || sound.audio.is_some()
                })
        {
            return Err("Destination contains Sounds. Enable Replace occupied, then Apply. Undo restores them".into());
        }
        let group = self.group;
        self.apply(doc)?;
        doc.set_cue(opendj_engine::maschine::Target::Group(group), true)
    }

    fn reslice(&mut self, mode: i32) -> Result<(), String> {
        let (begin, len) = self.edit_range;
        let mut starts = match mode {
            1 => opendj_engine::maschine::sample_edit::detect_positions(
                &self.sample,
                (begin, len),
                0.5,
                true,
            ),
            2 => vec![begin],
            3 => self.beat_markers.clone(),
            _ => opendj_engine::maschine::sample_edit::split_positions((begin, len), 16),
        };
        starts.retain(|start| *start >= begin && *start < len);
        starts.sort_unstable();
        starts.dedup();
        if starts.len() > 16 {
            if mode == 1 {
                starts.truncate(16);
            } else {
                return Err("More than 16 beat slices. Use Split 16 or a shorter capture".into());
            }
        }
        if starts.is_empty() {
            return Err("No performed beat markers in this sample. Choose Split or Detect".into());
        }
        self.slice_mode = mode;
        self.ranges = starts
            .iter()
            .enumerate()
            .map(|(index, start)| (*start, starts.get(index + 1).copied().unwrap_or(len) - 1))
            .collect();
        Ok(())
    }

    fn apply(self, doc: &mut opendj_engine::maschine::Document) -> Result<(), String> {
        if doc.project().groups.get(self.group).is_none() {
            return Err("target group does not exist".into());
        }
        if let Some((source_group, source_sound, expected_path)) = &self.source_sample {
            let current = doc
                .project()
                .groups
                .get(*source_group)
                .ok_or("Source Group changed")?
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
            let base_key = target.channel.base_key;
            let rename = target.name.is_empty()
                || target.name == target.instrument
                || target.name == format!("Sound {}", sound + 1);
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
                        ("root_key", base_key as f64),
                        ("play_range/low", low as f64),
                        ("play_range/high", high as f64),
                        ("loop_range/low", low as f64),
                        ("loop_range/high", high as f64),
                        ("tune", self.tunes[offset]),
                        ("gain", self.gains[offset]),
                    ],
                }],
                "Capture sample",
            )?;
            if self.ranges.len() > 1 && rename {
                doc.rename(
                    opendj_engine::maschine::Target::Sound(self.group, sound),
                    &format!("Slice {:02}", offset + 1),
                )?;
            }
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
    registry.create(group, "sampling_source", 0.0, 0.0, 4.0, Behavior::Free);
    registry.create(group, "sampling_stem", 0.0, 0.0, 4.0, Behavior::Free);
    registry.create(group, "sampling_bars", 4.0, 1.0, 16.0, Behavior::Free);
    registry.create(group, "sampling_slice", 1.0, 0.0, 3.0, Behavior::Free);
    registry.create(group, "sampling_next_bar", 1.0, 0.0, 1.0, Behavior::Free);
    registry.toggle(group, "sampling_monitor");
    registry.indicator(group, "sampling_progress", 1.0);
    registry.indicator(group, "cue_available", 1.0);
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
    let staged = std::rc::Rc::new(std::cell::RefCell::new(None));
    let editor_timer = install_editor(app, engine.clone(), registry.clone(), staged.clone());
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
    let capture_staged = staged.clone();
    app.global::<crate::MaschineUi>()
        .on_sample_capture(move |source, stem, bars, slice| {
            let Some(app) = weak.upgrade() else { return };
            let ui = app.global::<crate::MaschineUi>();
            if capture_staged.borrow().is_some() {
                ui.set_status("Apply or discard the prepared take before another capture".into());
                return;
            }
            if pending_capture.borrow().is_some() {
                ui.set_status("A capture is already being prepared".into());
                return;
            }
            if !(0..5).contains(&source) || !(1..=16).contains(&bars) || !(0..=4).contains(&stem) {
                ui.set_status("Choose a valid source and capture length".into());
                return;
            }
            let Some(target) = engine_capture.maschine(2) else {
                return;
            };
            let project_generation = target.project_generation();
            if source == 4 && stem != 0 {
                ui.set_status("Master capture requires Mix".into());
                return;
            }
            if source < 4 && stem > 0 {
                let track = engine_capture
                    .loaded_tracks()
                    .read()
                    .ok()
                    .and_then(|tracks| tracks[source as usize].clone());
                if track
                    .and_then(|track| track.audio.clone())
                    .and_then(|audio| audio.stems().map(|_| ()))
                    .is_none()
                {
                    ui.set_status(
                        "Selected stem is unavailable. Prepare stems on the source deck first"
                            .into(),
                    );
                    return;
                }
            }
            let Some(live) = engine_capture.machine_capture(source as usize) else {
                return;
            };
            let next_bar = controls
                .get("[Maschine3]", "sampling_next_bar")
                .is_none_or(|control| control.get_bool());
            ui.set_sample_origin(
                format!(
                    "{} · {} · {} beats",
                    if source == 4 {
                        "Master".to_string()
                    } else {
                        format!("Deck {}", source + 1)
                    },
                    [
                        "Mix · post FX",
                        "Drums · pre FX",
                        "Bass · pre FX",
                        "Melody · pre FX",
                        "Voice · pre FX"
                    ][stem as usize],
                    bars * 4
                )
                .into(),
            );
            let _ = target.stop_preview();
            ui.set_sample_replace_occupied(false);
            let capture_generation = match live.lock() {
                Ok(mut capture) => {
                    match capture.start_beats(bars as u32 * 4, stem as usize, next_bar) {
                        Ok(()) => capture.generation(),
                        Err(error) => {
                            ui.set_status(error.into());
                            return;
                        }
                    }
                }
                Err(_) => {
                    ui.set_status("Live capture unavailable".into());
                    return;
                }
            };
            let (tx, rx) = mpsc::channel();
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_cancel = cancel.clone();
            let worker_live = live.clone();
            let rate = engine_capture.sample_rate() as u32;
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(180);
                let result = (|| loop {
                    if worker_cancel.load(std::sync::atomic::Ordering::Acquire)
                        || target.project_generation() != project_generation
                    {
                        worker_live
                            .lock()
                            .map_err(|_| "Live capture unavailable")?
                            .cancel_generation(capture_generation);
                        break Err("Capture cancelled or project changed".into());
                    }
                    if std::time::Instant::now() > deadline {
                        worker_live
                            .lock()
                            .map_err(|_| "Live capture unavailable")?
                            .cancel_generation(capture_generation);
                        break Err("Capture timed out waiting for the source".into());
                    }
                    let mut capture = worker_live.lock().map_err(|_| "Live capture unavailable")?;
                    if let Some(frames) = capture.poll_generation(capture_generation)? {
                        let markers = capture.take_beat_markers();
                        drop(capture);
                        if worker_cancel.load(std::sync::atomic::Ordering::Acquire) {
                            break Err("Capture cancelled".into());
                        }
                        let mut prepared = prepare_frames(
                            frames,
                            rate,
                            0,
                            0,
                            Some(SliceMode::Grid),
                            project_generation,
                        )?;
                        prepared.beat_markers = markers;
                        prepared.reslice(match slice {
                            2 => 1,
                            3 => 3,
                            0 => 2,
                            _ => 0,
                        })?;
                        break Ok(prepared);
                    }
                    drop(capture);
                    std::thread::sleep(Duration::from_millis(40));
                })();
                let _ = tx.send(result);
            });
            *pending_capture.borrow_mut() = Some(PendingCapture {
                receiver: rx,
                cancelled: cancel,
                live: Some((live, capture_generation)),
            });
            capture_waiting.set_bool(true);
            capture_recording.set_bool(false);
            ui.set_status(
                if next_bar {
                    "Armed · next source bar · Capture stays in preview"
                } else {
                    "Recording performed audio · Capture stays in preview"
                }
                .into(),
            );
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
            if let Some(capture) = pending.borrow().as_ref() {
                if let Some((live, _)) = &capture.live {
                    if let Ok(live) = live.lock() {
                        let (active, progress) = live.progress();
                        recording.set_bool(active);
                        waiting.set_bool(!active);
                        if let Some(control) = registry.get("[Maschine3]", "sampling_progress") {
                            control.set(progress);
                        }
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
                let handle = engine.maschine(2).ok_or("Maschine deck unavailable")?;
                if handle.project_generation() != prepared.project_generation {
                    return Err("Project changed during capture".into());
                }
                show_staged(&ui, &prepared, 0);
                *staged.borrow_mut() = Some(prepared);
                Ok(())
            });
            ui.set_status(match result {
                Ok(()) => {
                    "Preview ready · audition slices in Cue, then Apply to the chosen Group".into()
                }
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

fn show_staged(ui: &crate::MaschineUi, prepared: &Prepared, selected: usize) {
    let selected = selected.min(prepared.ranges.len().saturating_sub(1));
    let (start, end) = prepared.ranges[selected];
    let len = prepared.sample.frames.len() as f32;
    ui.set_sample_staged(true);
    ui.set_sample_loaded(true);
    ui.set_sample_name(format!("Captured take · {} slices · Cue", prepared.ranges.len()).into());
    ui.set_sample_waveform(wave_image(&prepared.waveform));
    ui.set_sample_slice_count(prepared.ranges.len() as i32);
    ui.set_sample_selected_slice(selected as i32);
    ui.set_sample_slice_mode(prepared.slice_mode);
    ui.set_sample_start(start as f32 / len);
    ui.set_sample_end((end + 1) as f32 / len);
    ui.set_sample_gain(prepared.gains[selected] as f32);
    ui.set_sample_pitch(prepared.tunes[selected] as f32);
    let mut markers: Vec<f32> = prepared
        .ranges
        .iter()
        .map(|(start, _)| *start as f32 / len)
        .collect();
    markers.push(1.0);
    ui.set_sample_markers(slint::ModelRc::from(markers.as_slice()));
}

fn preview_sample(
    handle: MaschineHandle,
    sample: Arc<opendj_engine::maschine::Sample>,
    range: (usize, usize),
    tune: f64,
    gain: f64,
    weak: slint::Weak<crate::AppWindow>,
) {
    std::thread::spawn(move || {
        let result = (|| {
            let frames = sample
                .frames
                .get(range.0..range.1)
                .ok_or("Invalid preview range")?;
            let frames = frames
                .iter()
                .map(|frame| [frame[0] * gain as f32, frame[1] * gain as f32])
                .collect();
            let rate = (sample.rate as f64 * 2.0f64.powf(tune / 12.0))
                .round()
                .clamp(1.0, u32::MAX as f64) as u32;
            handle.preview_audio(Arc::new(opendj_engine::maschine::Sample { rate, frames }))
        })();
        if let Err(error) = result {
            let _ = slint::invoke_from_event_loop(move || {
                use slint::ComponentHandle;
                if let Some(app) = weak.upgrade() {
                    app.global::<crate::MaschineUi>().set_status(error.into());
                }
            });
        }
    });
}

fn install_editor(
    app: &crate::AppWindow,
    engine: std::rc::Rc<opendj_engine::AudioEngine>,
    registry: Arc<opendj_control::ControlRegistry>,
    staged: std::rc::Rc<std::cell::RefCell<Option<Prepared>>>,
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
    let range_staged = staged.clone();
    app.global::<crate::MaschineUi>()
        .on_sample_range(move |start, end| {
            *selection_range.borrow_mut() = (start, end);
            if let Some(app) = weak.upgrade() {
                let ui = app.global::<crate::MaschineUi>();
                ui.set_sample_start(start);
                ui.set_sample_end(end);
                if let Some(prepared) = range_staged.borrow_mut().as_mut() {
                    let index = (ui.get_sample_selected_slice().max(0) as usize)
                        .min(prepared.ranges.len() - 1);
                    let len = prepared.sample.frames.len();
                    let low = (start.clamp(0.0, 1.0) * len as f32).round() as usize;
                    let high = (end.clamp(0.0, 1.0) * len as f32).round().min(len as f32) as usize;
                    let after_previous = index == 0 || low > prepared.ranges[index - 1].0;
                    let before_next = prepared
                        .ranges
                        .get(index + 1)
                        .is_none_or(|range| low < range.0);
                    if high > low && after_previous && before_next {
                        if let Some(range) = prepared.ranges.get_mut(index) {
                            *range = (low, high - 1);
                        }
                    } else {
                        ui.set_status(
                            "Choose a nonempty slice range between its neighboring starts".into(),
                        );
                    }
                    show_staged(&ui, prepared, index);
                }
            }
        });
    let weak = app.as_weak();
    let actions_engine = engine.clone();
    let actions_registry = registry.clone();
    let actions_selection = selection.clone();
    let actions_markers = markers.clone();
    let actions_length = sample_length.clone();
    let marker_values = markers.clone();
    let marker_staged = staged.clone();
    let marker_weak = app.as_weak();
    app.global::<crate::MaschineUi>()
        .on_sample_marker(move |position| {
            let Some(app) = marker_weak.upgrade() else {
                return;
            };
            if let Some(prepared) = marker_staged.borrow_mut().as_mut() {
                let len = prepared.sample.frames.len();
                let at = (position.clamp(0.0, 1.0) * len as f32).round() as usize;
                let mut starts: Vec<usize> = prepared.ranges.iter().map(|range| range.0).collect();
                if starts.len() < 16 && at > 0 && at < len && !starts.contains(&at) {
                    starts.push(at);
                    starts.sort_unstable();
                    prepared.ranges = starts
                        .iter()
                        .enumerate()
                        .map(|(i, start)| (*start, starts.get(i + 1).copied().unwrap_or(len) - 1))
                        .collect();
                    show_staged(&app.global::<crate::MaschineUi>(), prepared, 0);
                }
                return;
            }
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
    let edit_staged = staged.clone();
    app.global::<crate::MaschineUi>()
        .on_sample_edit(move |field, value| {
            let Some(app) = weak_edit.upgrade() else {
                return;
            };
            let ui = app.global::<crate::MaschineUi>();
            if let Some(prepared) = edit_staged.borrow_mut().as_mut() {
                let index = ui.get_sample_selected_slice().max(0) as usize;
                if index < prepared.ranges.len() {
                    match field.as_str() {
                        "gain" => prepared.gains[index] = value.clamp(0.0, 2.0) as f64,
                        "pitch" => prepared.tunes[index] = value.clamp(-24.0, 24.0) as f64,
                        _ => {
                            ui.set_status(
                                "Apply the take to Cue to edit the sampler envelope".into(),
                            );
                            return;
                        }
                    }
                    show_staged(&ui, prepared, index);
                }
                return;
            }
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
    let preview_staged = staged.clone();
    let preview_engine = engine.clone();
    let preview_registry = registry.clone();
    let preview_weak = app.as_weak();
    let preview_length = sample_length.clone();
    app.global::<crate::MaschineUi>()
        .on_sample_preview(move |index| {
            let Some(app) = preview_weak.upgrade() else {
                return;
            };
            let ui = app.global::<crate::MaschineUi>();
            let Some(handle) = preview_engine.maschine(2) else {
                return;
            };
            if !preview_engine.headphones_ready() {
                ui.set_status(
                    "Headphone route unavailable · connect headphones to audition Cue".into(),
                );
                return;
            }
            if let Some(prepared) = preview_staged.borrow().as_ref() {
                if handle.project_generation() != prepared.project_generation {
                    ui.set_status("Project changed. Discard this capture".into());
                    return;
                }
                let selected = index.max(0) as usize;
                if selected >= prepared.ranges.len() {
                    return;
                }
                show_staged(&ui, prepared, selected);
                let range = if index < 0 {
                    (0, prepared.sample.frames.len())
                } else {
                    let (a, b) = prepared.ranges[selected];
                    (a, b + 1)
                };
                preview_sample(
                    handle,
                    prepared.sample.clone(),
                    range,
                    prepared.tunes[selected],
                    prepared.gains[selected],
                    app.as_weak(),
                );
                ui.set_status("Auditioning capture · Cue only".into());
            } else {
                let (group, sound) = selected(&preview_registry);
                let range = (*preview_length.borrow()).2;
                let start = (ui.get_sample_start().clamp(0.0, 1.0) * range as f32).round() as usize;
                let end = (ui.get_sample_end().clamp(0.0, 1.0) * range as f32).round() as usize;
                let gain = ui.get_sample_gain() as f64;
                let tune = ui.get_sample_pitch() as f64;
                let weak = app.as_weak();
                std::thread::spawn(move || {
                    if let Ok(Some(sample)) = handle.sample(group, sound, 0) {
                        preview_sample(handle, sample, (start, end), tune, gain, weak);
                    }
                });
            }
        });
    let apply_pending = Rc::new(RefCell::new(None::<mpsc::Receiver<Result<bool, String>>>));
    let action_pending = apply_pending.clone();
    let action_staged = staged.clone();
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
            if name == "STOP" {
                let _ = handle.stop_preview();
                return;
            }
            if name == "DISCARD" {
                *action_staged.borrow_mut() = None;
                ui.set_sample_staged(false);
                ui.set_sample_loaded(false);
                let _ = handle.stop_preview();
                let _ = handle.set_sampling_preview(None);
                ui.set_status("Capture discarded · Group unchanged".into());
                return;
            }
            if let Some(prepared) = action_staged.borrow_mut().as_mut() {
                if name == "RESLICE" || name == "clear-markers" || name == "SLICE" {
                    let mode = if name == "clear-markers" {
                        2
                    } else {
                        ui.get_sample_slice_mode()
                    };
                    match prepared.reslice(mode) {
                        Ok(()) => show_staged(&ui, prepared, 0),
                        Err(error) => ui.set_status(error.into()),
                    }
                } else if name == "APPLY" {
                    if action_pending.borrow().is_some() {
                        ui.set_status("Applying capture…".into());
                        return;
                    }
                    let mut assign = prepared.clone();
                    assign.group = group;
                    assign.sound = if ui.get_sample_stage_to_group() {
                        0
                    } else {
                        sound
                    };
                    if !ui.get_sample_stage_to_group() {
                        let index = ui.get_sample_selected_slice().max(0) as usize;
                        assign.ranges = vec![prepared.ranges[index]];
                        assign.tunes[0] = prepared.tunes[index];
                        assign.gains[0] = prepared.gains[index];
                    }
                    match assign.bind_staged(&handle, ui.get_sample_replace_occupied()) {
                        Ok(receipt) => {
                            *action_pending.borrow_mut() = Some(receipt);
                            ui.set_status(
                                "Applying capture to Cue · Undo restores previous Sounds".into(),
                            );
                        }
                        Err(error) => ui.set_status(error.into()),
                    }
                } else {
                    ui.set_status("Audition, slice and tune the take before Apply".into());
                }
                return;
            }
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
                if slice {
                    ui.set_sample_origin(
                        format!("Selected Sound · Group {} / Sound {}", group + 1, sound + 1)
                            .into(),
                    );
                }
                let slice_mode = ui.get_sample_slice_mode();
                let points = actions_markers.borrow().clone();
                let source_range = *actions_selection.borrow();
                let source_zone = handle.snapshot_cached().and_then(|p| {
                    p.groups
                        .get(group)
                        .and_then(|g| g.sounds.get(sound))
                        .and_then(|s| s.sampler.as_ref())
                        .and_then(|s| s.zones.first())
                        .map(|z| (z.sample.path.clone(), z.tune, z.gain))
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
                            .and_then(|mut prepared| {
                                prepared.preserve_zone = !slice;
                                if let Some((path, tune, gain)) = source_zone {
                                    prepared.source_sample = Some((group, sound, path));
                                    if slice {
                                        prepared.tunes.fill(tune);
                                        prepared.gains.fill(gain);
                                    }
                                }
                                if slice {
                                    let len = prepared.sample.frames.len();
                                    let low = (source_range.0.clamp(0.0, 1.0) * len as f32).round()
                                        as usize;
                                    let high = (source_range.1.clamp(0.0, 1.0) * len as f32).round()
                                        as usize;
                                    if high > low {
                                        prepared.edit_range = (low, high.min(len));
                                    }
                                    if slice_mode != 2 {
                                        prepared.reslice(slice_mode.min(2))?;
                                    } else {
                                        let (low, high) = prepared.edit_range;
                                        let mut starts: Vec<usize> = prepared
                                            .ranges
                                            .iter()
                                            .map(|range| range.0)
                                            .filter(|start| *start >= low && *start < high)
                                            .collect();
                                        if !starts.contains(&low) {
                                            starts.insert(0, low);
                                        }
                                        prepared.ranges = starts
                                            .iter()
                                            .enumerate()
                                            .map(|(i, start)| {
                                                (
                                                    *start,
                                                    starts.get(i + 1).copied().unwrap_or(high) - 1,
                                                )
                                            })
                                            .collect();
                                    }
                                }
                                Ok(prepared)
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
            let apply_result = apply_pending
                .borrow()
                .as_ref()
                .and_then(|pending| pending.try_recv().ok());
            if let Some(result) = apply_result {
                *apply_pending.borrow_mut() = None;
                match result {
                    Ok(_) => {
                        *staged.borrow_mut() = None;
                        ui.set_sample_staged(false);
                        ui.set_sample_loaded(false);
                        ui.set_status(
                            "Group in Cue · play or REC a Pattern, then release to Master".into(),
                        );
                        key = None;
                    }
                    Err(error) => ui.set_status(error.into()),
                }
            }
            if let Ok(result) = prepared_rx.try_recv() {
                let result = result.and_then(|prepared| {
                    if prepared.ranges.len() > 1 && !prepared.preserve_zone {
                        show_staged(&ui, &prepared, 0);
                        *staged.borrow_mut() = Some(prepared);
                        return Ok(None);
                    }
                    prepared
                        .bind(&engine.maschine(2).ok_or("Maschine deck unavailable")?)
                        .map(Some)
                });
                match result {
                    Ok(None) => ui.set_status(
                        "Slices ready · audition in Cue, then Apply to the chosen Group".into(),
                    ),
                    Ok(Some(receipt)) => {
                        watch_edit(receipt, app.as_weak());
                        ui.set_status("Finishing sample…".into());
                    }
                    Err(e) => ui.set_status(e.into()),
                }
                key = None;
            }
            if let Some(available) = registry.get("[Maschine3]", "cue_available") {
                available.set_bool(engine.headphones_ready());
            }
            if let Some(handle) = engine.maschine(2) {
                let preview = staged.borrow().as_ref().map(|prepared| {
                    opendj_engine::maschine::SamplingPreview {
                        project_generation: prepared.project_generation,
                        sample: prepared.sample.clone(),
                        ranges: prepared.ranges.clone(),
                        tunes: prepared.tunes,
                        gains: prepared.gains,
                    }
                });
                let _ = handle.set_sampling_preview(preview);
            }
            if staged.borrow().is_some() {
                if engine.maschine(2).is_some_and(|handle| {
                    staged.borrow().as_ref().is_some_and(|prepared| {
                        prepared.project_generation != handle.project_generation()
                    })
                }) {
                    *staged.borrow_mut() = None;
                    ui.set_sample_staged(false);
                    ui.set_sample_loaded(false);
                    key = None;
                    ui.set_status("Capture discarded after project change".into());
                } else {
                    return;
                }
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
    fn staged_sixteen_pads_preserve_notes_and_undo_the_explicit_replacement() {
        let mut doc = Document::new_project().unwrap();
        crate::maschine_ui::load_native(&mut doc, 0, 0, "Kick").unwrap();
        let note = opendj_engine::maschine::Note {
            sound: 0,
            position: 123,
            length: 277,
            pitch: 53,
            velocity: 93,
            probability: 47,
            expr: None,
        };
        doc.add_pattern(0, 0, "Keep notes").unwrap();
        doc.add_note(0, 0, note).unwrap();
        let before = doc.to_bytes().unwrap();
        let mut prepared =
            prepare_frames(vec![[0.2, -0.1]; 1600], 100, 0, 0, Some(SliceMode::Grid), 1).unwrap();
        prepared.tunes[5] = 7.0;
        prepared.gains[5] = 0.5;
        assert!(doc
            .transaction("Protected apply", |doc| prepared
                .clone()
                .apply_staged(doc, false))
            .is_err());
        assert_eq!(doc.to_bytes().unwrap(), before);
        doc.transaction("Explicit replacement", |doc| {
            prepared.apply_staged(doc, true)
        })
        .unwrap();
        assert!(doc.project().groups[0].cue);
        assert_eq!(doc.project().groups[0].patterns[0].notes, vec![note]);
        for (i, sound) in doc.project().groups[0].sounds.iter().enumerate() {
            let zone = &sound.sampler.as_ref().unwrap().zones[0];
            assert_eq!(zone.play_range, ((i * 100) as i64, (i * 100 + 99) as i64));
            assert_eq!(sound.name, format!("Slice {:02}", i + 1));
            assert_eq!(zone.root_key, sound.channel.base_key);
        }
        let zone = &doc.project().groups[0].sounds[5]
            .sampler
            .as_ref()
            .unwrap()
            .zones[0];
        assert_eq!(zone.tune, 7.0);
        assert_eq!(zone.gain, 0.5);
        assert!(doc.undo());
        assert_eq!(doc.to_bytes().unwrap(), before);
    }

    #[test]
    fn staged_grid_uses_performed_beats_and_detect_uses_sdk_onsets() {
        let mut frames = vec![[0.0; 2]; 96_000];
        for i in 0..16 {
            for frame in &mut frames[i * 6000..i * 6000 + 900] {
                *frame = [0.5; 2];
            }
        }
        let mut prepared = prepare_frames(frames, 48_000, 0, 0, None, 1).unwrap();
        prepared.beat_markers = (0..16).map(|i| i * 6000).collect();
        prepared.reslice(3).unwrap();
        assert_eq!(prepared.ranges.len(), 16);
        assert_eq!(prepared.ranges[4], (24_000, 29_999));
        prepared.beat_markers = (0..32).map(|i| i * 3000).collect();
        let before = prepared.ranges.clone();
        assert!(prepared.reslice(3).is_err());
        assert_eq!(prepared.ranges, before);
        let mut expected = opendj_engine::maschine::sample_edit::detect_positions(
            &prepared.sample,
            (0, 96_000),
            0.5,
            true,
        );
        expected.truncate(16);
        prepared.reslice(1).unwrap();
        assert_eq!(
            prepared
                .ranges
                .iter()
                .map(|range| range.0)
                .collect::<Vec<_>>(),
            expected
        );
    }

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
            sample: Arc::new(opendj_engine::maschine::Sample {
                rate: 44_100,
                frames: vec![[0.1, 0.2]; 32],
            }),
            beat_markers: Vec::new(),
            tunes: [0.0; 16],
            gains: [1.0; 16],
            slice_mode: 0,
            edit_range: (0, 32),
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
