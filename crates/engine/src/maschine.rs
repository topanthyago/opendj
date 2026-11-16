//! DECKARD as an instrument on an openDJ deck. The original Groove Box is
//! still available through `[GrooveN],backend = 0`.
//!
//! Documents and engine construction stay away from the audio callback. A
//! prepared engine and its timelines cross a bounded queue; the audio side
//! returns replaced objects over a second queue for destruction elsewhere.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use opendj_control::{Behavior, ControlHandle, ControlRegistry};

pub use deckard_doc::{
    note_tools, Chain, Document, FileItem, NewModule, Target, ZoneFrom, ZoneSpec,
};
pub use deckard_engine::edit as sample_edit;
pub use deckard_engine::sample::Sample;
pub use deckard_engine::synth::defaults as instrument_defaults;
pub use deckard_engine::{export as audio_export, Engine as OfflineEngine, SampleCache};
pub use deckard_engine::{timeline, Mode, Timeline};
use deckard_engine::{Engine, Live, Timelines};
pub use deckard_model::{
    level_db, level_from_db, Color, Note, Pattern, Project, Scene, SlotInfo, Sound, Swing, PPQ,
};
pub use mxprj::decode::Value;
pub use mxprj::sound_info::SoundInfo;

use crate::effects::BLOCK;
use crate::groove::{GrooveControls, GrooveStep, Source};
use crate::groove_fills;
use crate::kits::Kit;

pub const GROUPS: usize = 8;
pub const SOUNDS: usize = 16;
const QUEUE: usize = 8;

/// Controls for the embedded instrument. Indices are zero-based; pad names
/// are numbered from 1, like the controller.
#[derive(Clone)]
pub struct MaschineControls {
    pub group: ControlHandle,
    pub sound: ControlHandle,
    pub scene: ControlHandle,
    pub pattern: ControlHandle,
    pub keyboard: ControlHandle,
    pub octave: ControlHandle,
    pub semitone: ControlHandle,
    pub record: ControlHandle,
    pub count_in: ControlHandle,
    pub quantize: ControlHandle,
    pub grid: ControlHandle,
    pub choke: ControlHandle,
    pub metronome: ControlHandle,
    pub note_repeat: ControlHandle,
    pub repeat_rate: ControlHandle,
    pub undo: ControlHandle,
    pub redo: ControlHandle,
    pub ready: ControlHandle,
    pub error: ControlHandle,
    pub needs_main: ControlHandle,
    pub save_pending: ControlHandle,
    pub save_error: ControlHandle,
    pub position: ControlHandle,
    pub playing: ControlHandle,
    pub tempo: ControlHandle,
    pub playing_scene: ControlHandle,
    pub queued_scene: ControlHandle,
    pub pads: [ControlHandle; SOUNDS],
    pub pad_velocity: [ControlHandle; SOUNDS],
    pub colors: [ControlHandle; SOUNDS],
    pub meters: [[ControlHandle; SOUNDS]; GROUPS],
}

impl MaschineControls {
    fn new(registry: &ControlRegistry, deck: usize) -> Self {
        let group = format!("[Maschine{}]", deck + 1);
        let create = |key: &str, value, min, max| {
            registry.create(&group, key, value, min, max, Behavior::Free)
        };
        Self {
            group: create("group", 0.0, 0.0, 7.0),
            sound: create("sound", 0.0, 0.0, 15.0),
            scene: create("scene", 0.0, 0.0, 127.0),
            pattern: create("pattern", 0.0, 0.0, 127.0),
            keyboard: registry.toggle(&group, "keyboard"),
            octave: create("octave", 0.0, -5.0, 5.0),
            semitone: create("semitone", 0.0, -12.0, 12.0),
            record: registry.toggle(&group, "record"),
            count_in: registry.push(&group, "count_in"),
            quantize: registry.toggle(&group, "quantize"),
            grid: create("grid", 240.0, 15.0, 3840.0),
            choke: registry.push(&group, "choke"),
            metronome: registry.toggle(&group, "metronome"),
            note_repeat: registry.toggle(&group, "note_repeat"),
            repeat_rate: create("repeat_rate", 240.0, 15.0, 3840.0),
            undo: registry.push(&group, "undo"),
            redo: registry.push(&group, "redo"),
            ready: registry.indicator(&group, "ready", 1.0),
            error: registry.indicator(&group, "error", 1.0),
            needs_main: registry.indicator(&group, "needs_main", 1.0),
            save_pending: registry.indicator(&group, "save_pending", 1.0),
            save_error: registry.indicator(&group, "save_error", 1.0),
            position: registry.create(&group, "position", 0.0, 0.0, f64::MAX, Behavior::Indicator),
            playing: registry.indicator(&group, "playing", 1.0),
            tempo: registry.indicator(&group, "tempo", 1000.0),
            playing_scene: registry.create(
                &group,
                "playing_scene",
                -1.0,
                -1.0,
                127.0,
                Behavior::Indicator,
            ),
            queued_scene: registry.create(
                &group,
                "queued_scene",
                -1.0,
                -1.0,
                127.0,
                Behavior::Indicator,
            ),
            pads: std::array::from_fn(|k| create(&format!("pad{}", k + 1), 0.0, 0.0, 1.0)),
            pad_velocity: std::array::from_fn(|k| {
                create(&format!("pad{}_velocity", k + 1), 1.0, 0.0, 1.0)
            }),
            colors: std::array::from_fn(|k| {
                registry.indicator(&group, &format!("pad{}_color", k + 1), 20.0)
            }),
            meters: std::array::from_fn(|g| {
                std::array::from_fn(|s| {
                    registry.indicator(&group, &format!("group{}_sound{}_meter", g + 1, s + 1), 1.0)
                })
            }),
        }
    }
}

struct Prepared {
    engine: Engine,
    timelines: Timelines,
    target: Option<(Mode, deckard_engine::Timeline)>,
    telemetry: deckard_engine::Telemetry,
    base_keys: [[i64; SOUNDS]; GROUPS],
    roles: [[usize; crate::groove::ROWS]; GROUPS],
    project: Option<Arc<Project>>,
}

type Edit = Box<dyn FnOnce(&mut Document) -> Result<(), String> + Send + 'static>;

enum Request {
    Initialize,
    New,
    Edit(Edit),
    EditDeferred(Edit),
    EditDeferredForProject(u64, Edit),
    Undo,
    Redo,
    Open(PathBuf),
    Save(PathBuf),
    Flush,
    SelectPattern {
        scene: usize,
        group: usize,
        pattern: Option<usize>,
    },
}

struct Job {
    request: Request,
    reply: std::sync::mpsc::SyncSender<Result<bool, String>>,
}

struct Snapshot {
    project: Arc<Project>,
    path: Option<PathBuf>,
}

struct Shared {
    cache: Mutex<SampleCache>,
    ready: Mutex<rtrb::Producer<Prepared>>,
    retired: Mutex<rtrb::Consumer<Prepared>>,
    junk: Mutex<rtrb::Consumer<deckard_engine::Junk>>,
    preview: Mutex<rtrb::Producer<Option<Arc<Sample>>>>,
    retired_previews: Mutex<rtrb::Consumer<Arc<Sample>>>,
    snapshot: RwLock<Option<Snapshot>>,
    revision: AtomicU64,
    project_generation: AtomicU64,
    rate: u32,
    controls: MaschineControls,
    last_report: Mutex<Option<deckard_engine::LoadReport>>,
    last_error: Mutex<Option<String>>,
}

impl Shared {
    fn set_error(&self, error: String) {
        self.controls.error.set_bool(true);
        if let Ok(mut last) = self.last_error.lock() {
            *last = Some(error);
        }
    }

    fn clear_error(&self) {
        if !self.controls.save_error.get_bool() {
            self.controls.error.set_bool(false);
            if let Ok(mut last) = self.last_error.lock() {
                *last = None;
            }
        }
    }

    fn collect(&self) {
        if let Ok(mut retired) = self.retired.lock() {
            while retired.pop().is_ok() {}
        }
        if let Ok(mut junk) = self.junk.lock() {
            while junk.pop().is_ok() {}
        }
        if let Ok(mut retired) = self.retired_previews.lock() {
            while retired.pop().is_ok() {}
        }
    }

    /// May load files and plug-ins: never call on the audio thread. A new VST
    /// must be prepared from the main thread, as DECKARD's host requires.
    fn prepare(&self) -> Result<(), String> {
        self.collect();
        let version = self.revision.load(Ordering::Acquire);
        let (project, path) = {
            let snapshot = self
                .snapshot
                .read()
                .map_err(|_| "Maschine snapshot unavailable")?;
            let snapshot = snapshot.as_ref().ok_or("Maschine project not ready")?;
            (snapshot.project.clone(), snapshot.path.clone())
        };
        let scene = (self.controls.scene.get().round() as usize)
            .min(project.scenes.len().saturating_sub(1));
        let mode = Mode::Scene(scene);
        let (mut engine, report) = {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| "Maschine sample cache unavailable")?;
            Engine::new(&project, path.as_deref(), self.rate, mode, &mut cache)
        };
        let needs_main = report
            .plugin_errors
            .iter()
            .any(|s| s.contains("only be loaded on the main thread"));
        if let Ok(mut last) = self.last_report.lock() {
            *last = Some(report);
        }
        if needs_main {
            self.controls.needs_main.set_bool(true);
            return Err("Maschine plug-ins must be prepared on the main thread".into());
        }
        self.controls.needs_main.set_bool(false);
        engine.looping = true;
        engine.sequencing = false;
        engine.process(&mut [0.0; 2]);
        let mut telemetry = telemetry();
        engine.publish(&mut telemetry);
        let modes: Vec<_> = (0..project.scenes.len()).map(Mode::Scene).collect();
        let timelines = Timelines::of(&project, &modes);
        let target = Some((mode, deckard_engine::timeline(&project, mode)));
        let base_keys = std::array::from_fn(|g| {
            std::array::from_fn(|s| {
                project
                    .groups
                    .get(g)
                    .and_then(|group| group.sounds.get(s))
                    .map_or(60, |sound| sound.channel.base_key.clamp(0, 127))
            })
        });
        let roles = std::array::from_fn(|g| roles_of(project.groups.get(g)));
        if self.revision.load(Ordering::Acquire) != version {
            return Err("Maschine project changed while its audio was prepared".into());
        }
        let mut ready = self
            .ready
            .lock()
            .map_err(|_| "Maschine audio queue unavailable")?;
        ready
            .push(Prepared {
                engine,
                timelines,
                target,
                telemetry,
                base_keys,
                roles,
                project: Some(project),
            })
            .map_err(|_| "Maschine audio update queue is full".to_string())?;
        self.clear_error();
        Ok(())
    }
}

struct Editor {
    document: Option<Document>,
    legacy: GrooveControls,
    kits: Arc<[Kit]>,
    recorded: rtrb::Consumer<deckard_engine::Recorded>,
    held_recording: HashMap<(usize, usize, i64), deckard_engine::Recorded>,
    autosave: Option<PathBuf>,
    project_lock: Option<std::fs::File>,
    backed_up: bool,
    dirty_since: Option<Instant>,
    shared: Arc<Shared>,
}

impl Editor {
    fn ensure_document(&mut self) -> Result<(), String> {
        if self.document.is_none() {
            self.lock_project()?;
            let path = self
                .autosave
                .clone()
                .ok_or("no configuration directory for Maschine projects")?;
            if path.exists() {
                self.backup_if_needed()?;
                let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                let origin = project_origin(&path, &data)?;
                let mut document = Document::from_bytes(data, origin)?;
                let modified = complete_groups(&mut document)?;
                self.document = Some(document);
                self.publish_snapshot(modified)?;
                return Ok(());
            }
            let mut doc = empty_project()?;
            import_legacy(&mut doc, &self.legacy, &self.kits, self.shared.rate)?;
            if let Some(manifest) = factory_manifest() {
                if manifest.is_file() {
                    save_initial_legacy(&doc, &path)?;
                    crate::maschine_factory::import(&mut doc, &manifest)?;
                }
            }
            self.document = Some(reset_initial_history(doc)?);
            self.publish_snapshot(true)?;
        }
        Ok(())
    }

    fn publish_snapshot(&mut self, modified: bool) -> Result<(), String> {
        let doc = self.document.as_ref().ok_or("Maschine project not ready")?;
        let mut snapshot = self
            .shared
            .snapshot
            .write()
            .map_err(|_| "Maschine snapshot unavailable")?;
        *snapshot = Some(Snapshot {
            project: Arc::new(doc.project().clone()),
            path: doc.path().map(Path::to_path_buf),
        });
        self.shared.revision.fetch_add(1, Ordering::Release);
        if modified {
            self.dirty_since = Some(Instant::now());
            self.shared.controls.save_pending.set_bool(true);
        }
        Ok(())
    }

    fn backup_if_needed(&mut self) -> Result<(), String> {
        self.lock_project()?;
        if !self.backed_up {
            let path = self
                .autosave
                .as_ref()
                .ok_or("no configuration directory for Maschine projects")?;
            if path.exists() {
                backup_project(path)?;
            }
            self.backed_up = true;
        }
        Ok(())
    }

    fn lock_project(&mut self) -> Result<(), String> {
        if self.project_lock.is_none() {
            let path = self
                .autosave
                .as_ref()
                .ok_or("no configuration directory for Maschine projects")?;
            self.project_lock = Some(acquire_project_lock(path)?);
        }
        Ok(())
    }

    fn autosave_if_due(&mut self) -> Result<(), String> {
        if !self
            .dirty_since
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(300))
        {
            return Ok(());
        }
        self.autosave_now()
    }

    fn autosave_now(&mut self) -> Result<(), String> {
        if self.dirty_since.is_none() {
            return Ok(());
        }
        self.backup_if_needed()?;
        let doc = self.document.as_ref().ok_or("Maschine project not ready")?;
        let bytes = doc.to_bytes()?;
        let path = self
            .autosave
            .as_ref()
            .ok_or("no configuration directory for Maschine projects")?;
        save_project(path, &bytes, doc.path())?;
        self.dirty_since = None;
        self.shared.controls.save_pending.set_bool(false);
        self.shared.controls.save_error.set_bool(false);
        self.shared.clear_error();
        Ok(())
    }

    fn request(&mut self, request: Request) -> Result<bool, String> {
        match request {
            Request::Open(path) => {
                self.lock_project()?;
                let mut document = Document::open(&path)?;
                complete_groups(&mut document)?;
                self.autosave_now()?;
                let autosave = self
                    .autosave
                    .as_ref()
                    .ok_or("no configuration directory for Maschine projects")?;
                if autosave.exists() {
                    backup_project(autosave)?;
                }
                self.backed_up = true;
                self.document = Some(document);
                self.shared
                    .project_generation
                    .fetch_add(1, Ordering::AcqRel);
                self.publish_snapshot(true)?;
                Ok(true)
            }
            Request::Save(path) => {
                self.ensure_document()?;
                self.document
                    .as_mut()
                    .expect("initialized")
                    .save_as(&path)?;
                self.publish_snapshot(true)?;
                Ok(false)
            }
            Request::Initialize => {
                self.ensure_document()?;
                Ok(false)
            }
            Request::New => {
                self.lock_project()?;
                self.autosave_now()?;
                let path = self
                    .autosave
                    .as_ref()
                    .ok_or("no configuration directory for Maschine projects")?;
                if path.exists() {
                    backup_project(path)?;
                }
                self.backed_up = true;
                self.document = Some(reset_initial_history(empty_project()?)?);
                self.shared
                    .project_generation
                    .fetch_add(1, Ordering::AcqRel);
                self.publish_snapshot(true)?;
                Ok(true)
            }
            Request::Flush => {
                self.autosave_now()?;
                Ok(false)
            }
            Request::SelectPattern {
                scene,
                group,
                pattern,
            } => {
                if group >= GROUPS || scene > 127 || pattern.is_some_and(|p| p > 127) {
                    return Err("Maschine selection is outside the current bank".into());
                }
                self.ensure_document()?;
                self.document
                    .as_mut()
                    .expect("initialized")
                    .assign_pattern(scene, group, pattern)?;
                self.shared.controls.scene.set(scene as f64);
                self.shared.controls.group.set(group as f64);
                if let Some(pattern) = pattern {
                    self.shared.controls.pattern.set(pattern as f64);
                }
                self.publish_snapshot(true)?;
                Ok(true)
            }
            Request::Edit(edit) => {
                self.ensure_document()?;
                self.document
                    .as_mut()
                    .expect("initialized")
                    .transaction("Edit", edit)?;
                self.publish_snapshot(true)?;
                Ok(true)
            }
            Request::EditDeferredForProject(generation, edit) => {
                if self.shared.project_generation.load(Ordering::Acquire) != generation {
                    return Err("Maschine project changed while preparing the edit".into());
                }
                self.request(Request::EditDeferred(edit))
            }
            Request::EditDeferred(edit) => {
                self.ensure_document()?;
                self.document
                    .as_mut()
                    .expect("initialized")
                    .transaction("Edit", edit)?;
                self.publish_snapshot(true)?;
                self.shared.prepare()?;
                Ok(true)
            }
            Request::Undo | Request::Redo => {
                self.ensure_document()?;
                let document = self.document.as_mut().expect("initialized");
                let changed = if matches!(request, Request::Undo) {
                    document.undo()
                } else {
                    document.redo()
                };
                if changed {
                    self.publish_snapshot(true)?;
                }
                Ok(changed)
            }
        }
    }

    fn take_recording(&mut self) -> Result<bool, String> {
        let mut notes = Vec::new();
        while let Ok(event) = self.recorded.pop() {
            let key = (event.group, event.sound, event.pitch);
            if event.velocity > 0 && event.length == i64::MAX {
                self.held_recording.insert(key, event);
            } else if event.velocity > 0 {
                notes.push((event, event.length));
            } else if let Some(start) = self.held_recording.remove(&key) {
                notes.push((start, (event.played - start.played).round().max(1.0) as i64));
            }
        }
        if notes.is_empty() {
            return Ok(false);
        }
        let doc = self.document.as_mut().ok_or("Maschine project not ready")?;
        let mut changed = false;
        for (event, length) in notes {
            let Mode::Scene(scene) = event.mode else {
                continue;
            };
            let Some(pattern) = doc
                .project()
                .scenes
                .get(scene)
                .and_then(|s| s.patterns.iter().find(|(group, _)| *group == event.group))
                .map(|(_, pattern)| *pattern)
            else {
                continue;
            };
            let Some(p) = doc
                .project()
                .groups
                .get(event.group)
                .and_then(|g| g.patterns.get(pattern))
            else {
                continue;
            };
            let tick = if self.shared.controls.quantize.get_bool() {
                let grid = self.shared.controls.grid.get().round().max(1.0);
                (event.tick / grid).round() * grid
            } else {
                event.tick
            };
            let position = p.start + (tick.round() as i64).rem_euclid(p.length.max(1));
            doc.add_note(
                event.group,
                pattern,
                Note {
                    sound: event.sound,
                    position,
                    length,
                    pitch: event.pitch,
                    velocity: event.velocity,
                    probability: 100,
                    expr: None,
                },
            )?;
            changed = true;
        }
        if changed {
            self.publish_snapshot(true)?;
        }
        Ok(changed)
    }
}

/// A Send handle to the document owned exclusively by the editor thread.
#[derive(Clone)]
pub struct MaschineHandle {
    jobs: std::sync::mpsc::Sender<Job>,
    shared: Arc<Shared>,
}

impl MaschineHandle {
    /// Identity of the current document, unchanged by ordinary edits.
    pub fn project_generation(&self) -> u64 {
        self.shared.project_generation.load(Ordering::Acquire)
    }

    pub fn snapshot_version(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    /// Returns immediately if the editor is publishing a new snapshot.
    pub fn snapshot_cached(&self) -> Option<Arc<Project>> {
        self.shared
            .snapshot
            .try_read()
            .ok()?
            .as_ref()
            .map(|s| s.project.clone())
    }

    fn request(&self, request: Request) -> Result<bool, String> {
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        self.jobs
            .send(Job { request, reply })
            .map_err(|_| "Maschine editor stopped")?;
        result.recv().map_err(|_| "Maschine editor stopped")?
    }

    /// A full snapshot. May wait for initialization and must not be used by audio.
    pub fn snapshot(&self) -> Result<Project, String> {
        self.request(Request::Initialize)?;
        let snapshot = self
            .shared
            .snapshot
            .read()
            .map_err(|_| "Maschine snapshot unavailable")?;
        snapshot
            .as_ref()
            .map(|s| (*s.project).clone())
            .ok_or_else(|| "Maschine snapshot unavailable".into())
    }

    /// A project and its original path from one consistent snapshot.
    pub fn snapshot_with_path(&self) -> Result<(Arc<Project>, Option<PathBuf>), String> {
        self.request(Request::Initialize)?;
        let snapshot = self
            .shared
            .snapshot
            .read()
            .map_err(|_| "Maschine snapshot unavailable")?;
        let snapshot = snapshot.as_ref().ok_or("Maschine project not ready")?;
        Ok((snapshot.project.clone(), snapshot.path.clone()))
    }

    /// Run a transaction on the editor thread, then prepare its audio on the
    /// caller's thread. New VST instances require the app's main thread.
    pub fn edit(
        &self,
        f: impl FnOnce(&mut Document) -> Result<(), String> + Send + 'static,
    ) -> Result<(), String> {
        if self.request(Request::Edit(Box::new(f)))? {
            self.shared.prepare()?;
        }
        Ok(())
    }

    /// Queue an edit and audio preparation on the worker. The receiver reports
    /// completion; plug-ins that require the main thread set `needs_main`.
    pub fn edit_deferred(
        &self,
        f: impl FnOnce(&mut Document) -> Result<(), String> + Send + 'static,
    ) -> Result<std::sync::mpsc::Receiver<Result<bool, String>>, String> {
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        self.jobs
            .send(Job {
                request: Request::EditDeferred(Box::new(f)),
                reply,
            })
            .map_err(|_| "Maschine editor stopped")?;
        Ok(result)
    }

    /// Queue an edit only if the originating project is still current.
    pub fn edit_deferred_for_project(
        &self,
        generation: u64,
        f: impl FnOnce(&mut Document) -> Result<(), String> + Send + 'static,
    ) -> Result<std::sync::mpsc::Receiver<Result<bool, String>>, String> {
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        self.jobs
            .send(Job {
                request: Request::EditDeferredForProject(generation, Box::new(f)),
                reply,
            })
            .map_err(|_| "Maschine editor stopped")?;
        Ok(result)
    }

    pub fn undo(&self) -> Result<bool, String> {
        let changed = self.request(Request::Undo)?;
        if changed {
            self.shared.prepare()?;
        }
        Ok(changed)
    }

    pub fn redo(&self) -> Result<bool, String> {
        let changed = self.request(Request::Redo)?;
        if changed {
            self.shared.prepare()?;
        }
        Ok(changed)
    }

    pub fn open(&self, path: &Path) -> Result<(), String> {
        self.request(Request::Open(path.to_path_buf()))?;
        self.shared.prepare()
    }

    /// Start a new empty project after saving and backing up the current one.
    pub fn new_project(&self) -> Result<(), String> {
        self.request(Request::New)?;
        self.shared.prepare()
    }

    /// Assign a group's pattern and focus it as one editor operation.
    pub fn select_pattern(
        &self,
        scene: usize,
        group: usize,
        pattern: Option<usize>,
    ) -> Result<(), String> {
        self.request(Request::SelectPattern {
            scene,
            group,
            pattern,
        })?;
        self.shared.prepare()
    }

    pub fn save_as(&self, path: &Path) -> Result<(), String> {
        self.request(Request::Save(path.to_path_buf()))?;
        Ok(())
    }

    /// Persist any pending edits before the application shuts down.
    pub fn flush(&self) -> Result<(), String> {
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        self.jobs
            .send(Job {
                request: Request::Flush,
                reply,
            })
            .map_err(|_| "Maschine editor stopped")?;
        result
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "Maschine autosave timed out")??;
        Ok(())
    }

    /// Prepare a pending project on the app's main thread after a worker found
    /// a plug-in requiring it. A normal project edit calls this automatically.
    pub fn prepare_on_main(&self) -> Result<(), String> {
        self.shared.prepare()
    }

    pub fn load_report(&self) -> Result<Option<deckard_engine::LoadReport>, String> {
        Ok(self
            .shared
            .last_report
            .lock()
            .map_err(|_| "Maschine load report unavailable")?
            .clone())
    }

    pub fn last_error(&self) -> Option<String> {
        self.shared.last_error.try_lock().ok()?.clone()
    }

    /// Resolve a zone's sample for a display or editor; may read the disk.
    pub fn sample(
        &self,
        group: usize,
        sound: usize,
        zone: usize,
    ) -> Result<Option<Arc<Sample>>, String> {
        let (sample, path) = {
            let snapshot = self
                .shared
                .snapshot
                .read()
                .map_err(|_| "Maschine snapshot unavailable")?;
            let snapshot = snapshot.as_ref().ok_or("Maschine project not ready")?;
            let sample = snapshot
                .project
                .groups
                .get(group)
                .and_then(|g| g.sounds.get(sound))
                .and_then(|s| s.sampler.as_ref())
                .and_then(|s| s.zones.get(zone))
                .map(|z| z.sample.clone());
            (sample, snapshot.path.clone())
        };
        let Some(sample) = sample else {
            return Ok(None);
        };
        let mut cache = self
            .shared
            .cache
            .lock()
            .map_err(|_| "Maschine sample cache unavailable")?;
        Ok(cache.sample(&sample, path.as_deref()))
    }

    /// Preview an exact frame range, prepared without touching the audio thread.
    pub fn preview(
        &self,
        group: usize,
        sound: usize,
        zone: usize,
        start: usize,
        end: usize,
    ) -> Result<(), String> {
        let source = self
            .sample(group, sound, zone)?
            .ok_or("sample not available")?;
        if start >= end || end > source.frames.len() {
            return Err("invalid preview range".into());
        }
        let sample = Arc::new(Sample {
            rate: source.rate,
            frames: source.frames[start..end].to_vec(),
        });
        self.shared.collect();
        self.shared
            .preview
            .lock()
            .map_err(|_| "preview queue unavailable")?
            .push(Some(sample))
            .map_err(|_| "preview queue is full".to_string())
    }

    pub fn stop_preview(&self) -> Result<(), String> {
        self.shared.collect();
        self.shared
            .preview
            .lock()
            .map_err(|_| "preview queue unavailable")?
            .push(None)
            .map_err(|_| "preview queue is full".to_string())
    }

    /// Preview a file from the browser without changing the project.
    pub fn preview_file(&self, path: &Path) -> Result<(), String> {
        let sample = Sample::load(path)?;
        if sample.frames.is_empty() || sample.rate == 0 {
            return Err("empty preview sample".into());
        }
        self.shared.collect();
        self.shared
            .preview
            .lock()
            .map_err(|_| "preview queue unavailable")?
            .push(Some(Arc::new(sample)))
            .map_err(|_| "preview queue is full".to_string())
    }
}

/// Audio-thread side of a DECKARD instrument. No audio device is opened here.
pub(crate) struct Maschine {
    pub controls: MaschineControls,
    engine: Engine,
    telemetry: deckard_engine::Telemetry,
    incoming: rtrb::Consumer<Prepared>,
    retired: rtrb::Producer<Prepared>,
    junk: rtrb::Producer<deckard_engine::Junk>,
    recorded: rtrb::Producer<deckard_engine::Recorded>,
    preview: rtrb::Consumer<Option<Arc<Sample>>>,
    retired_previews: rtrb::Producer<Arc<Sample>>,
    preview_sample: Option<Arc<Sample>>,
    preview_position: f64,
    preview_active: bool,
    rate: f64,
    scratch: Vec<f32>,
    pad_last: [f64; SOUNDS],
    pad_seen: [u32; SOUNDS],
    count_in_seen: u32,
    held: [Option<(usize, usize, i64)>; SOUNDS],
    focused_group: usize,
    selected_sounds: [usize; GROUPS],
    last_sound: usize,
    sound_seen: u32,
    keyboard_focus: Option<(usize, usize, i64)>,
    base_keys: [[i64; SOUNDS]; GROUPS],
    roles: [[usize; crate::groove::ROWS]; GROUPS],
    project: Option<Arc<Project>>,
    fill_seed: u32,
    crash_next: bool,
    break_gate: bool,
    was_playing: bool,
    active: bool,
    repeat: Option<(bool, i64, bool)>,
    own_bpm: f64,
}

impl Maschine {
    pub fn new(
        registry: &ControlRegistry,
        deck: usize,
        rate: f64,
        legacy: GrooveControls,
        kits: Arc<[Kit]>,
    ) -> (Self, MaschineHandle) {
        let controls = MaschineControls::new(registry, deck);
        let (tx, incoming) = rtrb::RingBuffer::new(QUEUE);
        let (retired, rx) = rtrb::RingBuffer::new(QUEUE);
        let (junk, junk_rx) = rtrb::RingBuffer::new(512);
        let (recorded, recorded_rx) = rtrb::RingBuffer::new(4096);
        let (preview_tx, preview) = rtrb::RingBuffer::new(QUEUE);
        let (retired_previews, preview_retired_rx) = rtrb::RingBuffer::new(QUEUE);
        let (mut engine, _) = Engine::new(
            &Project::default(),
            None,
            rate as u32,
            Mode::Scene(0),
            &mut SampleCache::default(),
        );
        engine.looping = true;
        engine.sequencing = false;
        let shared = Arc::new(Shared {
            cache: Mutex::new(SampleCache::default()),
            ready: Mutex::new(tx),
            retired: Mutex::new(rx),
            junk: Mutex::new(junk_rx),
            preview: Mutex::new(preview_tx),
            retired_previews: Mutex::new(preview_retired_rx),
            snapshot: RwLock::new(None),
            revision: AtomicU64::new(0),
            project_generation: AtomicU64::new(1),
            rate: rate as u32,
            controls: controls.clone(),
            last_report: Mutex::new(None),
            last_error: Mutex::new(None),
        });
        let (jobs, requests) = std::sync::mpsc::channel::<Job>();
        let handle = MaschineHandle {
            jobs,
            shared: shared.clone(),
        };
        let worker_controls = controls.clone();
        std::thread::Builder::new()
            .name(format!("maschine-editor-{}", deck + 1))
            .spawn(move || {
                // `Document` deliberately never crosses this thread boundary: the
                // upstream project decoder keeps non-Send `Rc` callbacks.
                let mut editor = Editor {
                    document: None,
                    legacy,
                    kits,
                    recorded: recorded_rx,
                    held_recording: HashMap::new(),
                    autosave: autosave_path(deck),
                    project_lock: None,
                    backed_up: false,
                    dirty_since: None,
                    shared,
                };
                let mut initialized = false;
                let mut attempted = false;
                let mut scene_seen = worker_controls.scene.version();
                let mut group_seen = worker_controls.group.version();
                let mut pattern_seen = worker_controls.pattern.version();
                let mut selected_patterns = [0usize; GROUPS];
                loop {
                    match requests.recv_timeout(Duration::from_millis(4)) {
                        Ok(job) => {
                            let selects = matches!(&job.request, Request::SelectPattern { .. });
                            let result = editor.request(job.request);
                            if selects && result.is_ok() {
                                scene_seen = worker_controls.scene.version();
                                group_seen = worker_controls.group.version();
                                pattern_seen = worker_controls.pattern.version();
                                let group = worker_controls.group.get().round() as usize;
                                if group < GROUPS {
                                    selected_patterns[group] =
                                        worker_controls.pattern.get().round() as usize;
                                }
                            }
                            if let Err(error) = &result {
                                editor.shared.set_error(error.clone());
                            } else {
                                editor.shared.clear_error();
                            }
                            let _ = job.reply.send(result);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            if let Err(error) = editor.autosave_now() {
                                eprintln!("Maschine autosave: {error}");
                            }
                            break;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    editor.shared.collect();
                    if let Err(error) = editor.autosave_if_due() {
                        worker_controls.error.set_bool(true);
                        worker_controls.save_error.set_bool(true);
                        if let Ok(mut last) = editor.shared.last_error.lock() {
                            *last = Some(error);
                        }
                        editor.dirty_since = Some(Instant::now());
                    }
                    match editor.take_recording() {
                        Ok(true) => {
                            if let Err(error) = editor.shared.prepare() {
                                editor.shared.set_error(error);
                            }
                        }
                        Ok(false) => {}
                        Err(error) => editor.shared.set_error(error),
                    }
                    if !editor.legacy.backend.get_bool() {
                        attempted = false;
                    }
                    if editor.legacy.backend.get_bool() && !initialized && !attempted {
                        attempted = true;
                        let result = editor
                            .ensure_document()
                            .and_then(|()| editor.shared.prepare());
                        worker_controls.error.set_bool(result.is_err());
                        if let Err(error) = &result {
                            editor.shared.set_error(error.clone());
                        }
                        initialized = result.is_ok();
                    }
                    for (control, is_undo) in [
                        (&worker_controls.undo, true),
                        (&worker_controls.redo, false),
                    ] {
                        if control.get_bool() {
                            control.set_bool(false);
                            let request = if is_undo {
                                Request::Undo
                            } else {
                                Request::Redo
                            };
                            match editor.request(request) {
                                Ok(true) => {
                                    if let Err(error) = editor.shared.prepare() {
                                        editor.shared.set_error(error);
                                    }
                                }
                                Ok(false) => {}
                                Err(error) => editor.shared.set_error(error),
                            }
                        }
                    }
                    let scene_changed = worker_controls.scene.version() != scene_seen;
                    let group_changed = worker_controls.group.version() != group_seen;
                    let pattern_changed = worker_controls.pattern.version() != pattern_seen;
                    scene_seen = worker_controls.scene.version();
                    group_seen = worker_controls.group.version();
                    pattern_seen = worker_controls.pattern.version();
                    if editor.document.is_some()
                        && (scene_changed || group_changed)
                        && !pattern_changed
                    {
                        let scene = worker_controls.scene.get().round() as usize;
                        let group = worker_controls.group.get().round() as usize;
                        if group < GROUPS {
                            let pattern = editor
                                .document
                                .as_ref()
                                .and_then(|d| d.project().scenes.get(scene))
                                .and_then(|s| s.patterns.iter().find(|(g, _)| *g == group))
                                .map(|(_, p)| *p)
                                .unwrap_or(selected_patterns[group]);
                            selected_patterns[group] = pattern;
                            worker_controls.pattern.set(pattern as f64);
                            pattern_seen = worker_controls.pattern.version();
                        }
                    }
                    if pattern_changed {
                        let scene = worker_controls.scene.get().round() as usize;
                        let group = worker_controls.group.get().round() as usize;
                        let pattern = worker_controls.pattern.get().round() as usize;
                        if group < GROUPS {
                            selected_patterns[group] = pattern;
                        }
                        let result = editor
                            .document
                            .as_mut()
                            .ok_or_else(|| "project not ready".to_string())
                            .and_then(|d| d.assign_pattern(scene, group, Some(pattern)))
                            .and_then(|()| editor.publish_snapshot(true))
                            .and_then(|()| editor.shared.prepare());
                        if let Err(error) = result {
                            editor.shared.set_error(error);
                        }
                    } else if scene_changed {
                        if let Err(error) = editor.shared.prepare() {
                            editor.shared.set_error(error);
                        }
                    }
                }
            })
            .expect("start Maschine editor");
        let pad_seen = std::array::from_fn(|i| controls.pads[i].version());
        let count_in_seen = controls.count_in.version();
        let sound_seen = controls.sound.version();
        (
            Self {
                controls,
                engine,
                telemetry: telemetry(),
                incoming,
                retired,
                junk,
                recorded,
                preview,
                retired_previews,
                preview_sample: None,
                preview_position: 0.0,
                preview_active: false,
                rate,
                scratch: vec![0.0; BLOCK * 2],
                pad_last: [0.0; SOUNDS],
                pad_seen,
                count_in_seen,
                held: [None; SOUNDS],
                focused_group: 0,
                selected_sounds: [0; GROUPS],
                last_sound: 0,
                sound_seen,
                keyboard_focus: Some((0, 0, 60)),
                base_keys: [[60; SOUNDS]; GROUPS],
                roles: [[0, 1, 2, 3, 4, 5, 6, 7]; GROUPS],
                project: None,
                fill_seed: crate::groove::fill_seed(deck),
                crash_next: false,
                break_gate: false,
                was_playing: false,
                active: false,
                repeat: None,
                own_bpm: f64::NAN,
            },
            handle,
        )
    }

    /// Install prepared project changes, keeping sounding voices and transport.
    pub fn poll(&mut self) {
        while !self.junk.is_full() {
            let Some(junk) = self.engine.take_junk() else {
                break;
            };
            let result = self.junk.push(junk);
            debug_assert!(result.is_ok());
        }
        if !self.retired_previews.is_full() {
            if let Ok(sample) = self.preview.pop() {
                self.preview_active = sample.is_some();
                self.preview_position = 0.0;
                if let Some(old) = std::mem::replace(&mut self.preview_sample, sample) {
                    let result = self.retired_previews.push(old);
                    debug_assert!(result.is_ok());
                }
            }
        }
        if self.retired.is_full() || !self.engine.can_adopt() {
            return;
        }
        if let Ok(mut next) = self.incoming.pop() {
            next.engine.keep_timelines_for_adoption();
            let old = self.engine.adopt_with(next.engine, &mut next.timelines);
            std::mem::swap(&mut self.base_keys, &mut next.base_keys);
            std::mem::swap(&mut self.roles, &mut next.roles);
            std::mem::swap(&mut self.project, &mut next.project);
            self.own_bpm = f64::NAN;
            std::mem::swap(&mut self.telemetry, &mut next.telemetry);
            if let Some((mode, timeline)) = next.target.as_mut() {
                self.engine.queue_timeline_at_bar(*mode, timeline);
            }
            next.engine = old;
            let result = self.retired.push(next);
            debug_assert!(result.is_ok());
            self.controls.ready.set_bool(true);
        }
    }

    pub fn render(
        &mut self,
        play: bool,
        source: Source,
        signals: &[GrooveStep],
        controls: &GrooveControls,
        break_start: bool,
        out: &mut [[f32; 2]],
        cue: &mut [[f32; 2]],
        mut kick: impl FnMut(usize),
    ) {
        self.active = true;
        self.poll();
        self.engine.set_recording(self.controls.record.get_bool());
        self.engine.metronome = self.controls.metronome.get_bool();
        if self.controls.choke.get_bool() {
            self.controls.choke.set_bool(false);
            self.engine.schedule(0, Live::ChokeAll);
        }
        let repeat = (
            self.controls.note_repeat.get_bool(),
            self.controls.repeat_rate.get().round() as i64,
            self.controls.keyboard.get_bool(),
        );
        if self.repeat != Some(repeat) {
            let mut arp = deckard_model::Arp::default();
            arp.rate = repeat.1.max(1);
            let mode = if repeat.2 {
                deckard_engine::repeat::RepeatMode::Arp
            } else {
                deckard_engine::repeat::RepeatMode::NoteRepeat
            };
            self.engine.set_repeat(repeat.0.then_some((arp, mode)));
            self.repeat = Some(repeat);
        }
        if play && !self.was_playing {
            if let Some(beat) = source.beat {
                self.engine.sync_start();
                self.engine.follow(&deckard_engine::sync::Follow {
                    tempo: source.bpm,
                    beat,
                    absolute: false,
                });
            } else {
                self.engine.seek(0);
                self.engine.sequencing = true;
            }
        } else if !play && self.was_playing {
            self.engine.stop();
        }
        self.was_playing = play;
        self.engine.set_followed(source.beat.is_some());
        if let Some(beat) = source.beat {
            self.own_bpm = f64::NAN;
            self.engine.follow(&deckard_engine::sync::Follow {
                tempo: source.bpm,
                beat,
                absolute: false,
            });
        } else if self.own_bpm != source.bpm {
            self.engine.set_tempo(source.bpm);
            self.own_bpm = source.bpm;
        }
        let count_changed = self.controls.count_in.version() != self.count_in_seen;
        if play && (self.controls.count_in.get_bool() || count_changed) {
            self.controls.count_in.set_bool(false);
            self.count_in_seen = self.controls.count_in.version();
            let frames =
                (self.rate * 60.0 / source.bpm.max(1.0) * self.engine.quantum()).round() as u64;
            self.engine.count_in(frames);
        }
        let group = (self.controls.group.get().round() as usize).min(GROUPS - 1);
        let sound = (self.controls.sound.get().round() as usize).min(SOUNDS - 1);
        let sound_changed = self.controls.sound.version() != self.sound_seen;
        if group != self.focused_group {
            self.selected_sounds[self.focused_group] = self.last_sound;
            self.focused_group = group;
            if sound_changed {
                self.selected_sounds[group] = sound;
            } else {
                self.controls.sound.set(self.selected_sounds[group] as f64);
            }
        } else {
            self.selected_sounds[group] = sound;
        }
        self.last_sound = self.selected_sounds[group];
        self.sound_seen = self.controls.sound.version();
        self.sync_keyboard_focus(group, self.last_sound);
        self.engine.set_focus(Some(group));
        for pad in 0..SOUNDS {
            let value = self.controls.pads[pad].get();
            let version = self.controls.pads[pad].version();
            let changed = version != self.pad_seen[pad];
            let down = self.pad_last[pad] > 0.0;
            self.pad_seen[pad] = version;
            self.pad_last[pad] = value;
            if !down && (value > 0.0 || changed) {
                let keyboard = self.controls.keyboard.get_bool();
                let sound = if keyboard {
                    (self.controls.sound.get().round() as usize).min(SOUNDS - 1)
                } else {
                    pad
                };
                if !keyboard {
                    self.controls.sound.set(sound as f64);
                }
                self.selected_sounds[group] = sound;
                self.last_sound = sound;
                self.sound_seen = self.controls.sound.version();
                let pitch = if keyboard {
                    60 + pad as i64
                        + self.controls.octave.get().round() as i64 * 12
                        + self.controls.semitone.get().round() as i64
                } else {
                    self.base_keys[group][sound]
                };
                let pitch = pitch.clamp(0, 127);
                let level = if value > 0.0 {
                    value
                } else {
                    self.controls.pad_velocity[pad].get()
                };
                let velocity = (level * 127.0).round().clamp(1.0, 127.0) as i64;
                self.engine.schedule(
                    0,
                    Live::NoteOn {
                        group,
                        sound,
                        pitch,
                        velocity,
                        length: i64::MAX,
                    },
                );
                self.held[pad] = Some((group, sound, pitch));
            }
            if value <= 0.0 && (down || changed) {
                if let Some((group, sound, pitch)) = self.held[pad].take() {
                    self.engine.schedule(
                        0,
                        Live::NoteRelease {
                            group,
                            sound,
                            pitch,
                        },
                    );
                }
            }
            self.controls.colors[pad].set(if value > 0.0 { 9.0 } else { 13.0 });
        }
        self.sync_keyboard_focus(group, self.last_sound);
        let start_frame = self.engine.output_frame();
        self.engine.clear_played_notes();
        if self.break_gate != break_start {
            self.break_gate = break_start;
            self.engine
                .set_sequence_gate_sounds(None, None, break_start);
            if break_start {
                self.crash_next = false;
            }
        }
        let mut cursor = 0;
        for &signal in signals {
            let boundary = signal.frame.min(out.len());
            if boundary < cursor {
                continue;
            }
            self.render_audio(&mut out[cursor..boundary]);
            cursor = boundary;
            if self.engine.sequencing {
                self.apply_step(controls, signal, group);
            }
        }
        self.render_audio(&mut out[cursor..]);
        for note in self.engine.played_notes() {
            if note.group < GROUPS && note.sound == self.roles[note.group][0] {
                let offset = note.frame.saturating_sub(start_frame) as usize;
                if offset < out.len() {
                    kick(offset);
                }
            }
        }
        cue.fill([0.0; 2]);
        if self.preview_active {
            if let Some(sample) = &self.preview_sample {
                let step = sample.rate as f64 / self.rate;
                for frame in cue.iter_mut() {
                    let i = self.preview_position as usize;
                    if i >= sample.frames.len() {
                        self.preview_active = false;
                        break;
                    }
                    let next = (i + 1).min(sample.frames.len() - 1);
                    let t = (self.preview_position - i as f64) as f32;
                    for (channel, output) in frame.iter_mut().enumerate() {
                        *output = sample.frames[i][channel] * (1.0 - t)
                            + sample.frames[next][channel] * t;
                    }
                    self.preview_position += step;
                }
            }
        }
        self.engine.publish(&mut self.telemetry);
        self.controls.position.set(self.telemetry.position.max(0.0));
        self.controls.playing.set_bool(self.telemetry.sequencing);
        self.controls.tempo.set(self.telemetry.tempo);
        self.controls.playing_scene.set(match self.telemetry.mode {
            Some(Mode::Scene(scene)) => scene as f64,
            _ => -1.0,
        });
        self.controls.queued_scene.set(match self.telemetry.queued {
            Some(Mode::Scene(scene)) => scene as f64,
            _ => -1.0,
        });
        let recorded = &mut self.recorded;
        self.telemetry
            .recorded
            .retain(|event| recorded.push(*event).is_err());
        for g in 0..GROUPS {
            for s in 0..SOUNDS {
                let peak = self
                    .telemetry
                    .meters
                    .get_mut(g)
                    .and_then(|row| row.get_mut(s));
                let value = peak.map_or(0.0, |p| {
                    let value = p.0.max(p.1);
                    *p = (0.0, 0.0);
                    value
                });
                self.controls.meters[g][s].set(value as f64);
            }
        }
    }

    pub fn suspend(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.was_playing = false;
        self.preview_active = false;
        self.engine.stop();
        self.controls.playing.set_bool(false);
        for meter in self.controls.meters.iter().flatten() {
            meter.set(0.0);
        }
    }

    pub fn active(&self) -> bool {
        self.active
    }

    fn render_audio(&mut self, out: &mut [[f32; 2]]) {
        for block in out.chunks_mut(BLOCK) {
            let audio = &mut self.scratch[..block.len() * 2];
            self.engine.process(audio);
            for (frame, samples) in block.iter_mut().zip(audio.chunks_exact(2)) {
                *frame = [samples[0], samples[1]];
            }
        }
    }

    fn apply_step(&mut self, controls: &GrooveControls, signal: GrooveStep, group: usize) {
        if signal.break_active {
            self.break_gate = true;
            self.crash_next = false;
            self.engine.set_sequence_gate_sounds(None, None, true);
            return;
        }
        self.break_gate = false;
        let (written, normal) = self.written_steps(group, signal.step);
        let roles = self.roles[group];
        let plan = overlay_plan(
            controls,
            signal,
            normal,
            written,
            roles,
            self.fill_seed,
            &mut self.crash_next,
        );
        if plan.gate == 0 {
            self.engine.set_sequence_gate_sounds(None, None, false);
            return;
        }
        self.engine
            .set_sequence_gate_sounds(Some(group), Some(plan.gate), false);
        for (row, velocity) in plan.hits.into_iter().enumerate() {
            if velocity <= 0.0 {
                continue;
            }
            let sound = roles[row];
            let velocity = ((velocity.clamp(0.0, 1.25) as f64 / 1.25).cbrt() * 127.0)
                .round()
                .clamp(1.0, 127.0) as i64;
            self.engine.note_on_generated(
                group,
                sound,
                self.base_keys[group][sound],
                velocity,
                PPQ / 4,
            );
        }
    }

    fn written_steps(
        &self,
        group: usize,
        step: usize,
    ) -> ([u32; crate::groove::ROWS], [f32; crate::groove::ROWS]) {
        let mut written = [0u32; crate::groove::ROWS];
        let mut normal = [0f32; crate::groove::ROWS];
        let Some(project) = &self.project else {
            return (written, normal);
        };
        let Mode::Scene(scene) = self.engine.mode() else {
            return (written, normal);
        };
        let Some(pattern) = project
            .scenes
            .get(scene)
            .and_then(|scene| scene.patterns.iter().find(|(g, _)| *g == group))
            .and_then(|(_, p)| project.groups.get(group)?.patterns.get(*p))
        else {
            return (written, normal);
        };
        let length = pattern.length.max(1);
        let bar = PPQ * 4;
        let position = (self.engine.position().round() as i64).rem_euclid(length);
        let bar_start = (position / bar) * bar;
        let grid = PPQ / 4;
        for note in &pattern.notes {
            let offset = note.position - pattern.start - bar_start;
            if offset < 0 || offset >= bar || offset % grid != 0 {
                continue;
            }
            let note_step = (offset / grid) as usize;
            for (row, sound) in self.roles[group].iter().enumerate() {
                if note.sound == *sound {
                    written[row] |= 1 << note_step;
                    if note_step == step {
                        normal[row] = normal[row].max((note.velocity.clamp(0, 127) as f32) / 127.0);
                    }
                }
            }
        }
        (written, normal)
    }

    fn sync_keyboard_focus(&mut self, group: usize, sound: usize) {
        let key = self.base_keys[group][sound];
        let focus = (group, sound, key);
        if self.keyboard_focus == Some(focus) {
            return;
        }
        self.keyboard_focus = Some(focus);
        let delta = key - 60;
        self.controls.octave.set((delta / 12) as f64);
        self.controls.semitone.set((delta % 12) as f64);
    }
}

fn telemetry() -> deckard_engine::Telemetry {
    let mut t = deckard_engine::Telemetry::with_room();
    t.recorded.reserve(4096);
    t.recorded_expr.reserve(4096);
    t.recorded_controls.reserve(4096);
    t
}

fn roles_of(group: Option<&deckard_model::Group>) -> [usize; crate::groove::ROWS] {
    let Some(group) = group else {
        return [0, 1, 2, 3, 4, 5, 6, 7];
    };
    let named = |sound: usize, words: &[&str]| {
        group.sounds.get(sound).is_some_and(|s| {
            let name = s.name.to_lowercase();
            words.iter().any(|word| name.contains(word))
        })
    };
    if group.sounds.len() >= SOUNDS
        && named(0, &["kick", "bumbo"])
        && named(2, &["snare", "caixa"])
        && named(6, &["clap", "palma"])
    {
        // The 16-sound factory banks put the eight roles in pairs.
        [0, 2, 4, 5, 6, 10, 11, 14]
    } else {
        // A legacy or external kit keeps its own first-eight order.
        [0, 1, 2, 3, 4, 5, 6, 7]
    }
}

#[derive(Default)]
struct OverlayPlan {
    gate: u32,
    hits: [f32; crate::groove::ROWS],
}

/// Apply the original Groove figures to the focused group's matching sounds.
/// Sounds outside those roles remain on the DECKARD timeline.
fn overlay_plan(
    controls: &GrooveControls,
    signal: GrooveStep,
    normal: [f32; crate::groove::ROWS],
    written: [u32; crate::groove::ROWS],
    roles: [usize; crate::groove::ROWS],
    seed: u32,
    crash_next: &mut bool,
) -> OverlayPlan {
    let bar = (signal.beat / 4.0 + 1e-6).floor() as i64;
    let fill = if controls.fill.get_bool() {
        Some(groove_fills::held_fill(bar, seed))
    } else {
        groove_fills::auto_fill(controls.auto_fill.get().round() as u8, bar, seed)
            .filter(|fill| signal.step >= fill.from)
    };
    let remix = groove_fills::remix_level(controls.remix.get());
    let mut plan = OverlayPlan::default();
    let mut replace_all = false;
    if remix > 0 {
        plan.hits = groove_fills::remix(remix, signal.step, normal, written);
        replace_all = true;
    } else if let Some(fill) = fill {
        if controls.auto_fill_style.get_bool() {
            let result = groove_fills::pattern_fill(fill, signal.step, normal, written);
            for row in 0..crate::groove::ROWS {
                if result[row] > normal[row] {
                    plan.gate |= 1u32 << roles[row];
                    plan.hits[row] = result[row];
                }
            }
        } else {
            plan.hits = fill.hits(signal.step);
            replace_all = true;
        }
        if fill.steps() >= 8
            && signal.step == crate::groove::STEPS - 1
            && (!controls.auto_fill_style.get_bool() || written[crate::groove::ROWS - 1] != 0)
        {
            *crash_next = true;
        }
    } else if signal.step == 0 && std::mem::take(crash_next) {
        let row = crate::groove::ROWS - 1;
        if normal[row] < 1.0 {
            plan.gate |= 1u32 << roles[row];
            plan.hits[row] = 1.0;
        }
    }
    if replace_all {
        for sound in roles {
            plan.gate |= 1u32 << sound;
        }
    }
    plan
}

fn complete_groups(document: &mut Document) -> Result<bool, String> {
    let mut changed = false;
    while document.project().groups.len() < GROUPS {
        let group = document.add_group()?;
        document.add_pattern(group, 0, "Pattern 1")?;
        changed = true;
    }
    for group in 0..GROUPS {
        if document.ensure_sound_slots(group, SOUNDS)? > 0 {
            changed = true;
        }
    }
    Ok(changed)
}

fn empty_project() -> Result<Document, String> {
    let mut document = Document::new_project()?;
    for group in 0..GROUPS {
        if group > 0 {
            document.add_group()?;
        }
        let pattern = document.add_pattern(group, 0, "Pattern 1")?;
        document.assign_pattern(0, group, Some(pattern))?;
    }
    Ok(document)
}

fn reset_initial_history(document: Document) -> Result<Document, String> {
    let bytes = document.to_bytes()?;
    let mut clean = Document::from_bytes(bytes, None)?;
    clean.mark_unsaved();
    Ok(clean)
}

/// Translate the current eight-row Groove Box into Group A. Its sample files
/// are content-addressed and live alongside settings so saved projects can
/// reopen them; existing files are checked and never overwritten.
fn import_legacy(
    d: &mut Document,
    c: &GrooveControls,
    kits: &[Kit],
    rate: u32,
) -> Result<(), String> {
    let kit = kits
        .get((c.kit.get().round() as usize).min(kits.len().saturating_sub(1)))
        .ok_or("no Groove Box kit")?;
    let any_solo = c.solo.iter().any(ControlHandle::get_bool);
    for row in 0..crate::groove::ROWS {
        let index = (c.sound[row].get().round() as usize).min(kit.sounds.len().saturating_sub(1));
        let sound = kit.sounds.get(index).ok_or("empty Groove Box kit")?;
        let file = persist_sample(&sound.sample, rate)?;
        d.add_slot(0, row, 0, &NewModule::Sampler)?;
        d.set_initial(Target::Sampler(0, row), "polyphony", Value::Int(1))?;
        let last = sound.sample.len().saturating_sub(1) as f64;
        d.set_zones(
            0,
            row,
            &[deckard_doc::ZoneSpec {
                from: deckard_doc::ZoneFrom::Template,
                file: Some(file),
                set: vec![
                    ("play_range/high", last),
                    ("loop_range/high", last),
                    ("tune", c.pitch[row].get()),
                ],
            }],
            "Import Groove sound",
        )?;
        d.rename(Target::Sound(0, row), sound.name)?;
        // The DECKARD Sampler's velocity response is cubic. Leaving room for
        // the old 1.25 accent requires 1.25x sound gain and lower velocities.
        let gain = (c.level[row].get() * 1.25).max(0.0);
        let level = if gain > 0.0 {
            deckard_model::level_from_db(20.0 * gain.log10())
        } else {
            0.0
        };
        d.set_level(Target::Sound(0, row), level)?;
        d.set_pan(Target::Sound(0, row), c.pan[row].get())?;
        d.set_mute(
            Target::Sound(0, row),
            if any_solo {
                !c.solo[row].get_bool()
            } else {
                c.mute[row].get_bool()
            },
        )?;
    }
    let swing = c.swing.get().clamp(0.0, 0.75);
    let amount = ((1.0 + 80.0 * swing / 9.0).sqrt() - 1.0) * 50.0;
    d.set_swing(
        Target::Group(0),
        deckard_model::Swing {
            amount,
            cycle: 0,
            invert: false,
        },
    )?;
    for slot in 0..crate::groove::GROOVES {
        let pattern = if slot == 0 {
            0
        } else {
            d.add_pattern_in_slot(0, 0, slot, &format!("Pattern {}", slot + 1))?
        };
        let mut notes = Vec::new();
        for row in 0..crate::groove::ROWS {
            let bits = c.patterns[slot][row].get() as u32;
            let soft = c.soft[slot][row].get() as u32;
            let accent = c.accent[slot][row].get() as u32;
            let maybe = c.maybe[slot][row].get() as u32;
            for step in 0..crate::groove::STEPS {
                let bit = 1 << step;
                if bits & bit == 0 {
                    continue;
                }
                let gain = if accent & bit != 0 {
                    1.25f64
                } else if soft & bit != 0 {
                    0.5
                } else {
                    1.0
                };
                notes.push(Note {
                    sound: row,
                    position: step as i64 * PPQ / 4,
                    length: PPQ / 4,
                    pitch: 60,
                    velocity: ((gain / 1.25).cbrt() * 127.0).round() as i64,
                    probability: if maybe & bit != 0 { 50 } else { 100 },
                    expr: None,
                });
            }
        }
        d.edit_notes("Import Groove pattern", 0, pattern, &[], &notes)?;
    }
    let selected = (c.groove.get().round() as usize).min(crate::groove::GROOVES - 1);
    d.assign_pattern(0, 0, Some(selected))?;
    Ok(())
}

fn persist_sample(sample: &crate::sampler::Sample, rate: u32) -> Result<PathBuf, String> {
    let length = sample
        .len()
        .checked_mul(4)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or("Groove sample is too large")?;
    let mut wav = Vec::with_capacity(length as usize + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(length + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&length.to_le_bytes());
    for i in 0..sample.len() {
        for v in sample.frame(i) {
            wav.extend_from_slice(&((v * i16::MAX as f32).round() as i16).to_le_bytes());
        }
    }
    #[cfg(not(test))]
    let root = crate::paths::config_base()
        .ok_or("no configuration directory")?
        .join(crate::paths::app_folder_name())
        .join("maschine")
        .join("samples");
    #[cfg(test)]
    let root = std::env::temp_dir().join(format!("opendj-maschine-tests-{}", std::process::id()));
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let mut crc = flate2::Crc::new();
    crc.update(&wav);
    for suffix in 0..1000u32 {
        let path = root.join(format!("{:08x}-{}-{suffix}.wav", crc.sum(), wav.len()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(&wav)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                file.sync_all()
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))? == wav {
                    return Ok(path);
                }
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Err("too many Groove sample filename collisions".into())
}

fn autosave_path(deck: usize) -> Option<PathBuf> {
    #[cfg(not(test))]
    let root = crate::paths::config_base()?
        .join(crate::paths::app_folder_name())
        .join("maschine");
    #[cfg(test)]
    let root = std::env::temp_dir().join(format!("opendj-maschine-tests-{}", std::process::id()));
    Some(root.join(format!("deck{}.mxprj", deck + 1)))
}

fn factory_manifest() -> Option<PathBuf> {
    // Tests opt in explicitly and use per-process temporary project storage.
    #[cfg(test)]
    if std::env::var_os("OPENDJ_TEST_ALLOW_FACTORY").is_none() {
        return None;
    }
    std::env::var_os("OPENDJ_MASCHINE_FACTORY_KITS").map(PathBuf::from)
}

/// Preserve the legacy import before a factory kit replaces its sounds.
fn save_initial_legacy(document: &Document, autosave: &Path) -> Result<PathBuf, String> {
    let parent = autosave
        .parent()
        .ok_or("project file has no parent directory")?;
    let stem = autosave.file_stem().ok_or("project file has no name")?;
    let dir = parent.join(stem);
    let bytes = document.to_bytes()?;
    let primary = dir.join("initial-legacy.mxprj");
    let destination = if primary.exists() {
        if std::fs::read(&primary).map_err(|e| format!("{}: {e}", primary.display()))? == bytes {
            return Ok(primary);
        }
        dir.join(format!("initial-legacy-{}.mxprj", fingerprint(&bytes)))
    } else {
        primary
    };
    if destination.exists() {
        if std::fs::read(&destination).map_err(|e| format!("{}: {e}", destination.display()))?
            == bytes
        {
            return Ok(destination);
        }
        return Err("initial legacy backup filename collision".into());
    }
    atomic_write(&destination, &bytes)?;
    Ok(destination)
}

/// Stable fingerprint linking a project to its original sample-resolution
/// directory in the companion metadata. Both hashes are over the saved bytes.
fn fingerprint(bytes: &[u8]) -> String {
    let mut crc = flate2::Crc::new();
    crc.update(bytes);
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, byte| {
        (h ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    });
    format!("{:08x}-{hash:016x}-{}", crc.sum(), bytes.len())
}

fn metadata_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".meta.json");
    PathBuf::from(name)
}

fn acquire_project_lock(path: &Path) -> Result<std::fs::File, String> {
    let parent = path
        .parent()
        .ok_or("project file has no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let lock_path = PathBuf::from(name);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("{}: {e}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the file descriptor stays owned by `file` for the lock's lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!(
                "cannot lock Maschine project {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(file)
}

#[cfg(unix)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
#[cfg(unix)]
fn decode_path(bytes: Vec<u8>) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}
#[cfg(windows)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}
#[cfg(windows)]
fn decode_path(bytes: Vec<u8>) -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    if bytes.len() % 2 != 0 {
        return Err("invalid saved project path".into());
    }
    let wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
}
#[cfg(not(any(unix, windows)))]
fn encode_path(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}
#[cfg(not(any(unix, windows)))]
fn decode_path(bytes: Vec<u8>) -> Result<PathBuf, String> {
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|e| e.to_string())
}

/// Atomically replace one file using a unique same-directory temporary file.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("project file has no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let mut temp = path.as_os_str().to_os_string();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    temp.push(format!(".{}.{}.saving", std::process::id(), nonce));
    let temp = PathBuf::from(temp);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| format!("{}: {e}", temp.display()))?;
        file.write_all(bytes)
            .map_err(|e| format!("{}: {e}", temp.display()))?;
        file.sync_all()
            .map_err(|e| format!("{}: {e}", temp.display()))?;
        std::fs::rename(&temp, path).map_err(|e| format!("{}: {e}", path.display()))?;
        #[cfg(unix)]
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("{}: {e}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

/// Metadata is written first and retains both the previous and next origin.
/// If interrupted between the two renames the old project still finds its own
/// entry. Old sidecars are backed up with the project on the next session.
fn save_project(path: &Path, bytes: &[u8], origin: Option<&Path>) -> Result<(), String> {
    let meta = metadata_path(path);
    let mut entries: Vec<serde_json::Value> = if meta.exists() {
        let data = std::fs::read(&meta).map_err(|e| format!("{}: {e}", meta.display()))?;
        serde_json::from_slice(&data).map_err(|e| format!("{}: {e}", meta.display()))?
    } else {
        Vec::new()
    };
    let id = fingerprint(bytes);
    entries
        .retain(|entry| entry.get("id").and_then(serde_json::Value::as_str) != Some(id.as_str()));
    entries.push(serde_json::json!({"id": id, "origin": origin.map(encode_path)}));
    if entries.len() > 3 {
        entries.drain(..entries.len() - 3);
    }
    let metadata = serde_json::to_vec(&entries).map_err(|e| e.to_string())?;
    atomic_write(&meta, &metadata)?;
    atomic_write(path, bytes)
}

fn project_origin(path: &Path, bytes: &[u8]) -> Result<Option<PathBuf>, String> {
    let meta = metadata_path(path);
    if !meta.exists() {
        return Ok(Some(path.to_path_buf()));
    }
    let data = std::fs::read(&meta).map_err(|e| format!("{}: {e}", meta.display()))?;
    let entries: Vec<serde_json::Value> =
        serde_json::from_slice(&data).map_err(|e| format!("{}: {e}", meta.display()))?;
    let id = fingerprint(bytes);
    let entry = entries
        .iter()
        .rev()
        .find(|entry| entry.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str()))
        .ok_or("Maschine autosave and origin metadata do not match")?;
    let value = entry
        .get("origin")
        .ok_or("Maschine autosave origin is missing")?;
    if value.is_null() {
        return Ok(None);
    }
    let raw: Vec<u8> = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    decode_path(raw).map(Some)
}

/// Keep the pre-existing autosave and its metadata once before modifying them.
fn backup_project(path: &Path) -> Result<(), String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    for source in [path.to_path_buf(), metadata_path(path)] {
        if !source.exists() {
            continue;
        }
        let mut name = source.as_os_str().to_os_string();
        name.push(format!(".backup-{stamp}"));
        let target = PathBuf::from(name);
        let mut input =
            std::fs::File::open(&source).map_err(|e| format!("{}: {e}", source.display()))?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| format!("{}: {e}", target.display()))?;
        std::io::copy(&mut input, &mut output).map_err(|e| format!("{}: {e}", target.display()))?;
        output
            .sync_all()
            .map_err(|e| format!("{}: {e}", target.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groove::{GrooveControls, Source};
    use crate::kits::{Kit, KitSound};

    fn fixture(deck: usize) -> (Maschine, MaschineHandle, GrooveControls) {
        let registry = ControlRegistry::default();
        let controls = GrooveControls::new(&registry, deck);
        for slot in 0..crate::groove::GROOVES {
            for row in 0..crate::groove::ROWS {
                controls.patterns[slot][row].set(0.0);
                controls.soft[slot][row].set(0.0);
                controls.accent[slot][row].set(0.0);
                controls.maybe[slot][row].set(0.0);
            }
        }
        controls.patterns[0][0].set(1.0);
        let sample = Arc::new(crate::sampler::Sample::from_mono(
            &vec![0.25; 8000],
            48_000.0,
        ));
        let kit = Kit {
            name: "Test",
            sounds: (0..9)
                .map(|_| KitSound {
                    name: "Test",
                    sample: sample.clone(),
                    pitch: 0.0,
                })
                .collect(),
        };
        let (machine, handle) = Maschine::new(
            &registry,
            deck,
            48_000.0,
            controls.clone(),
            Arc::from([kit]),
        );
        (machine, handle, controls)
    }

    #[test]
    fn deferred_edit_rejects_replaced_project() {
        let (_machine, handle, _) = fixture(99);
        let original = handle.project_generation();
        handle.snapshot().unwrap(); // Lazy initialization is the same project.
        assert_eq!(handle.project_generation(), original);
        handle.new_project().unwrap();
        assert_ne!(handle.project_generation(), original);
        let edited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = edited.clone();
        let result = handle
            .edit_deferred_for_project(original, move |_| {
                flag.store(true, Ordering::Release);
                Ok(())
            })
            .unwrap()
            .recv()
            .unwrap();
        assert!(result.is_err());
        assert!(!edited.load(Ordering::Acquire));
        let current = handle.project_generation();
        handle
            .edit_deferred_for_project(current, |_| Ok(()))
            .unwrap()
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(handle.project_generation(), current);

        handle
            .edit(|doc| doc.rename(Target::Group(0), "Before open checkpoint"))
            .unwrap();
        let path = autosave_path(99).unwrap();
        let incoming = path.with_file_name("incoming-deferred-test.mxprj");
        empty_project().unwrap().save_as(&incoming).unwrap();
        handle.open(&incoming).unwrap();
        assert_ne!(handle.project_generation(), current);
        let result = handle
            .edit_deferred_for_project(current, |_| Err("stale edit ran".into()))
            .unwrap()
            .recv()
            .unwrap();
        assert_eq!(
            result.unwrap_err(),
            "Maschine project changed while preparing the edit"
        );
        let prefix = format!("{}.backup-", path.file_name().unwrap().to_string_lossy());
        let restored = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .filter_map(|entry| std::fs::read(entry.path()).ok())
            .filter_map(|bytes| Document::from_bytes(bytes, None).ok())
            .any(|doc| doc.project().groups[0].name == "Before open checkpoint");
        assert!(
            restored,
            "Open must preserve the outgoing project in a backup"
        );
    }

    #[test]
    fn real_document_plays_two_groups_and_autosave_reopens() {
        // A deck outside the app's range keeps this test's autosave isolated.
        let deck = 31;
        let (mut machine, handle, legacy) = fixture(deck);
        let project = handle.snapshot().expect("import legacy project");
        assert_eq!(project.groups.len(), GROUPS);
        assert!(project.groups.iter().all(|g| g.sounds.len() == SOUNDS));
        assert_eq!(project.groups[0].patterns.len(), 4);
        assert!(project.groups[0].sounds[8..]
            .iter()
            .all(|s| s.slots.is_empty()));
        assert!(project.groups[0].patterns[0]
            .notes
            .iter()
            .any(|n| n.sound == 0 && n.position == 0));
        handle
            .edit(|doc| {
                let params = deckard_engine::synth::defaults("Kick")
                    .unwrap()
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value))
                    .collect();
                doc.add_slot(
                    1,
                    0,
                    0,
                    &NewModule::Instrument {
                        class: "Kick".into(),
                        params,
                    },
                )?;
                doc.add_note(
                    1,
                    0,
                    Note {
                        sound: 0,
                        position: 0,
                        length: PPQ / 4,
                        pitch: 60,
                        velocity: 127,
                        probability: 100,
                        expr: None,
                    },
                )?;
                Ok(())
            })
            .expect("prepare second group");
        machine.poll();
        assert!(machine.controls.ready.get_bool());
        let mut output = vec![[0.0; 2]; 512];
        let mut cue = vec![[0.0; 2]; 512];
        let source = Source {
            bpm: 120.0,
            beat: None,
        };
        machine.render(
            true,
            source,
            &[],
            &legacy,
            false,
            &mut output,
            &mut cue,
            |_| {},
        );
        assert!(
            output.iter().any(|f| f[0].abs() > 0.001),
            "instrument reaches deck"
        );
        assert!(machine.controls.meters[0][0].get() > 0.0, "group A sounded");
        assert!(
            machine.controls.meters[1][0].get() > 0.0,
            "group B sounded in parallel"
        );
        machine.controls.group.set(1.0);
        machine.render(
            true,
            source,
            &[],
            &legacy,
            false,
            &mut output,
            &mut cue,
            |_| {},
        );
        assert!(
            machine.controls.meters[0][0].get() > 0.0,
            "focus does not mute group A"
        );
        assert!(
            machine.controls.meters[1][0].get() > 0.0,
            "group B keeps playing"
        );
        machine.controls.group.set(0.0);
        machine.controls.sound.set(12.0);
        machine.render(
            true,
            source,
            &[],
            &legacy,
            false,
            &mut output,
            &mut cue,
            |_| {},
        );
        assert_eq!(
            machine.controls.sound.get(),
            12.0,
            "simultaneous group and sound selection"
        );
        machine.controls.group.set(1.0);
        machine.render(
            true,
            source,
            &[],
            &legacy,
            false,
            &mut output,
            &mut cue,
            |_| {},
        );
        assert_eq!(
            machine.controls.sound.get(),
            0.0,
            "group B remembers its own sound"
        );
        machine.controls.group.set(0.0);
        machine.render(
            true,
            source,
            &[],
            &legacy,
            false,
            &mut output,
            &mut cue,
            |_| {},
        );
        assert_eq!(
            machine.controls.sound.get(),
            12.0,
            "group A remembers its own sound"
        );
        machine.controls.sound.set(0.0);
        machine.render(
            true,
            source,
            &[],
            &legacy,
            false,
            &mut output,
            &mut cue,
            |_| {},
        );
        machine.controls.keyboard.set_bool(true);
        machine.controls.octave.set(1.0);
        machine.controls.semitone.set(2.0);
        machine.controls.pads[0].set(1.0);
        machine.render(
            true,
            source,
            &[],
            &legacy,
            false,
            &mut output,
            &mut cue,
            |_| {},
        );
        assert!(
            machine
                .engine
                .played_notes()
                .iter()
                .any(|n| n.group == 0 && n.sound == 0 && n.pitch == 74 && !n.sequenced),
            "keyboard applies octave and semitone"
        );
        assert!(handle.undo().unwrap());
        assert!(handle.redo().unwrap());
        handle.flush().expect("flush autosave");
        let path = autosave_path(deck).unwrap();
        assert!(path.exists(), "autosave is on disk");
        drop(machine);
        drop(handle);
        drop(legacy);
        std::thread::sleep(Duration::from_millis(50));
        let (_machine, reopened, _legacy) = fixture(deck);
        let restored = reopened.snapshot().expect("reopen autosave");
        assert_eq!(restored.groups.len(), GROUPS);
        assert!(restored.groups[1].patterns[0]
            .notes
            .iter()
            .any(|n| n.sound == 0));
        assert!(restored.groups[1].sounds[0]
            .slots
            .iter()
            .any(|s| s.module == "Kick"));
    }

    #[test]
    fn autosave_metadata_roundtrips_the_original_directory() {
        let path = autosave_path(60).unwrap();
        let original = PathBuf::from("/tmp/original project/session.mxprj");
        save_project(&path, b"first", Some(&original)).unwrap();
        save_project(&path, b"second", None).unwrap();
        assert_eq!(project_origin(&path, b"first").unwrap(), Some(original));
        assert_eq!(project_origin(&path, b"second").unwrap(), None);
        assert!(project_origin(&path, b"other").is_err());
    }

    #[test]
    fn fill_masks_only_the_eight_mapped_factory_sounds() {
        let registry = ControlRegistry::default();
        let controls = GrooveControls::new(&registry, 29);
        let roles = [0, 2, 4, 5, 6, 10, 11, 14];
        let signal = GrooveStep {
            frame: 0,
            step: 0,
            beat: 0.0,
            break_active: false,
        };
        controls.fill.set_bool(true);
        let plan = overlay_plan(&controls, signal, [0.0; 8], [0; 8], roles, 1, &mut false);
        let expected = roles
            .into_iter()
            .fold(0u32, |mask, sound| mask | 1 << sound);
        assert_eq!(plan.gate, expected);
        assert_eq!(plan.gate & (1 << 1), 0, "other factory sound keeps playing");
        assert_eq!(plan.gate & (1 << 15), 0, "sound 16 keeps playing");
        assert!(plan.hits.iter().any(|&velocity| velocity > 0.0));
        controls.fill.set_bool(false);
        let plain = overlay_plan(&controls, signal, [0.0; 8], [0; 8], roles, 1, &mut false);
        assert_eq!(plain.gate, 0);
    }

    #[test]
    fn embedded_fill_keeps_other_groups_and_break_keeps_live_pad() {
        let (mut machine, handle, legacy) = fixture(32);
        handle.snapshot().unwrap();
        handle
            .edit(|doc| {
                let params = deckard_engine::synth::defaults("Kick")
                    .unwrap()
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value))
                    .collect();
                doc.add_slot(
                    1,
                    0,
                    0,
                    &NewModule::Instrument {
                        class: "Kick".into(),
                        params,
                    },
                )?;
                doc.add_note(
                    1,
                    0,
                    Note {
                        sound: 0,
                        position: 0,
                        length: PPQ / 4,
                        pitch: 60,
                        velocity: 127,
                        probability: 100,
                        expr: None,
                    },
                )?;
                Ok(())
            })
            .unwrap();
        machine.poll();
        legacy.fill.set_bool(true);
        machine.controls.record.set_bool(true);
        let source = Source {
            bpm: 120.0,
            beat: None,
        };
        let signal = GrooveStep {
            frame: 0,
            step: 0,
            beat: 0.0,
            break_active: false,
        };
        let mut out = vec![[0.0; 2]; 512];
        let mut cue = vec![[0.0; 2]; 512];
        let mut kicks = Vec::new();
        machine.render(
            true,
            source,
            &[signal],
            &legacy,
            false,
            &mut out,
            &mut cue,
            |f| kicks.push(f),
        );
        let played = machine.engine.played_notes();
        assert!(
            played.iter().any(|n| n.group == 0 && !n.sequenced),
            "fill played in focused group"
        );
        assert!(
            !played.iter().any(|n| n.group == 0 && n.sequenced),
            "replaced roles are gated"
        );
        assert!(
            played.iter().any(|n| n.group == 1 && n.sequenced),
            "other group keeps its pattern"
        );
        assert!(kicks.contains(&0), "duck sees a real kick");
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            handle.snapshot().unwrap().groups[0].patterns[0].notes.len(),
            1,
            "generated fill did not enter REC"
        );
        machine.controls.record.set_bool(false);
        machine.controls.pads[0].set(1.0);
        let breaking = GrooveStep {
            break_active: true,
            ..signal
        };
        machine.render(
            true,
            source,
            &[breaking],
            &legacy,
            true,
            &mut out,
            &mut cue,
            |_| {},
        );
        assert!(
            machine
                .engine
                .played_notes()
                .iter()
                .any(|n| n.group == 0 && !n.sequenced),
            "a manual pad still plays during the break"
        );
    }

    #[test]
    fn quick_count_in_waits_one_bar_before_recording() {
        let (mut machine, handle, legacy) = fixture(33);
        handle.snapshot().unwrap();
        handle.edit(|_| Ok(())).unwrap();
        machine.poll();
        machine.controls.record.set_bool(true);
        machine.controls.count_in.set_bool(true);
        machine.controls.count_in.set_bool(false);
        let source = Source {
            bpm: 120.0,
            beat: None,
        };
        let mut out = vec![[0.0; 2]; 512];
        let mut cue = vec![[0.0; 2]; 512];
        machine.render(
            true,
            source,
            &[],
            &legacy,
            false,
            &mut out,
            &mut cue,
            |_| {},
        );
        assert!(
            !machine.controls.playing.get_bool(),
            "sequencer waits during count-in"
        );
        for _ in 0..190 {
            machine.render(
                true,
                source,
                &[],
                &legacy,
                false,
                &mut out,
                &mut cue,
                |_| {},
            );
        }
        assert!(
            machine.controls.playing.get_bool(),
            "sequencer starts after the bar"
        );
    }
}
