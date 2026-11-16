//! DECKARD as an instrument on an openDJ deck. The original Groove Box is
//! still available through `[GrooveN],backend = 0`.
//!
//! Documents and engine construction stay away from the audio callback. A
//! prepared engine and its timelines cross a bounded queue; the audio side
//! returns replaced objects over a second queue for destruction elsewhere.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

#[path = "maschine_paths.rs"]
mod project_paths;

use crate::effects::BLOCK;
use crate::groove::{GrooveControls, GrooveManualHit, GroovePadEdit, GrooveStep, Source};
use crate::groove_dice::{self, Dice, Habits, Move, Track};
use crate::groove_fills;
use crate::kits::Kit;

pub const GROUPS: usize = 8;
pub const SOUNDS: usize = 16;
const QUEUE: usize = 8;

/// Runs on DECKARD's original sequenced note, so variation never needs to
/// recreate a note and lose its pitch, length or expression.
struct GrooveVariation {
    habits: Arc<Habits>,
    rng: groove_dice::Rng,
}

impl deckard_engine::SequenceTransform for GrooveVariation {
    fn note(&mut self, note: &deckard_engine::NoteEvent, context: &[u64; 16]) -> Option<i64> {
        let vary = f32::from_bits(context[2] as u32).clamp(0.0, 1.0);
        if vary <= 0.0 || note.group != context[0] as usize {
            return Some(note.velocity);
        }
        let Some(row) = context[4..12]
            .iter()
            .position(|sound| *sound as usize == note.sound)
        else {
            return Some(note.velocity);
        };
        let role = if row == crate::groove::ROWS - 1 && context[12] == 8 {
            8
        } else {
            row
        };
        let step = context[3] as usize % crate::groove::STEPS;
        let genre = context[1] as usize;
        if !groove_dice::keeps(vary, role, step, genre, &self.habits, &mut self.rng) {
            return None;
        }
        // The DECKARD Sampler's velocity curve is cubic; scale its velocity
        // by the cube root of the original Groove's amplitude variation.
        let gain = groove_dice::humanize(1.0, vary, &mut self.rng)
            .max(0.0)
            .cbrt();
        Some((note.velocity as f32 * gain).round().clamp(1.0, 127.0) as i64)
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
pub struct LegacySoundSettings {
    pub sound: Option<usize>,
    pub level: Option<f64>,
    pub pitch: Option<f64>,
    pub decay: Option<f64>,
    pub tone: Option<f64>,
    pub pan: Option<f64>,
    pub mute: Option<bool>,
    pub solo: Option<bool>,
}

#[derive(Clone, Copy)]
struct LegacyMixSnapshot {
    kit: usize,
    rows: [LegacySoundSettings; crate::groove::ROWS],
}

pub struct NativeGroovePreset {
    pub group_file: Vec<u8>,
    pub project: Vec<u8>,
    pub origin: Option<Vec<u8>>,
}

// Small, copy-only messages from the audio callback. The document and all
// note editing, allocation and preparation remain on the editor thread.
#[derive(Clone, Copy)]
enum GrooveAction {
    Dice {
        movement: Move,
        row: Option<usize>,
        locked: [bool; crate::groove::ROWS],
        genre: usize,
    },
    Load {
        genre: usize,
        index: usize,
        slot: Option<usize>,
    },
    LoadSet {
        genre: usize,
        set: usize,
        tone: [f64; crate::groove::ROWS],
    },
    Swing(f64),
    SelectSlot(usize),
    Pad(GroovePadEdit),
    TrackDelta {
        slot: usize,
        row: usize,
        before: Track,
        after: Track,
    },
    Mix {
        kit: usize,
        settings: [LegacySoundSettings; crate::groove::ROWS],
        rebuild: [bool; crate::groove::ROWS],
        replace: [bool; crate::groove::ROWS],
        parameters: [u8; crate::groove::ROWS],
        mute_changed: [bool; crate::groove::ROWS],
    },
    Undo,
}

#[derive(Clone, Copy)]
struct GrooveCommand {
    generation: u64,
    group: usize,
    scene: usize,
    action: GrooveAction,
}

struct GrooveUndo {
    generation: u64,
    group: usize,
    slot: usize,
    removed: Vec<Note>,
    added: Vec<Note>,
}

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
    legacy_sounds: [[bool; SOUNDS]; GROUPS],
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
    UndoStep,
    RedoStep,
    Open(PathBuf),
    Save(PathBuf),
    Flush,
    ExportGroovePreset {
        group: usize,
        reply: std::sync::mpsc::SyncSender<Result<NativeGroovePreset, String>>,
    },
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
    groove_overflow: AtomicBool,
    groove_lost: AtomicBool,
    rate: u32,
    kits: Arc<[Kit]>,
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
        if !self.controls.save_error.get_bool() && !self.groove_lost.load(Ordering::Acquire) {
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
        if self
            .ready
            .lock()
            .map_err(|_| "Maschine audio queue unavailable")?
            .is_full()
        {
            return Err("Maschine audio update queue is full".into());
        }
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
        let root = legacy_samples_root().ok();
        let legacy_sounds = std::array::from_fn(|g| {
            std::array::from_fn(|s| {
                project
                    .groups
                    .get(g)
                    .and_then(|group| group.sounds.get(s))
                    .is_some_and(|sound| {
                        root.as_ref()
                            .is_some_and(|root| is_managed_sound(sound, root))
                    })
            })
        });
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
                legacy_sounds,
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
    groove_commands: rtrb::Consumer<GrooveCommand>,
    groove_dice: Dice,
    groove_undo: Vec<GrooveUndo>,
    groove_solo_saved: [Option<[bool; crate::groove::ROWS]>; GROUPS],
    autosave: Option<PathBuf>,
    project_lock: Option<std::fs::File>,
    backed_up: bool,
    dirty_since: Option<Instant>,
    shared: Arc<Shared>,
}

impl Editor {
    fn take_groove_commands(&mut self) -> Result<bool, String> {
        let mut changed = false;
        if let Ok(mut command) = self.groove_commands.pop() {
            // A dragged swing control can produce many updates while the
            // worker prepares audio. Keep the last consecutive value.
            while matches!(command.action, GrooveAction::Swing(_))
                && self.groove_commands.peek().is_ok_and(|next| {
                    matches!(next.action, GrooveAction::Swing(_))
                        && next.group == command.group
                        && next.generation == command.generation
                })
            {
                command = self.groove_commands.pop().expect("peeked");
            }
            while matches!(command.action, GrooveAction::Mix { .. })
                && self.groove_commands.peek().is_ok_and(|next| {
                    matches!(next.action, GrooveAction::Mix { .. })
                        && next.group == command.group
                        && next.generation == command.generation
                })
            {
                let next = self.groove_commands.pop().expect("peeked");
                merge_mix(&mut command.action, next.action);
            }
            if command.generation != self.shared.project_generation.load(Ordering::Acquire) {
                return Ok(false);
            }
            self.ensure_document()?;
            let doc = self.document.as_mut().expect("initialized");
            match command.action {
                GrooveAction::Swing(value) => {
                    let old = doc
                        .project()
                        .groups
                        .get(command.group)
                        .ok_or("no such group")?
                        .swing;
                    let amount = groove_swing(value);
                    if (old.amount - amount).abs() > 1e-9 {
                        doc.set_swing(Target::Group(command.group), Swing { amount, ..old })?;
                        changed = true;
                    }
                }
                GrooveAction::SelectSlot(slot) => {
                    doc.transaction("Select Groove", |doc| {
                        let pattern = pattern_in_slot(doc, command.group, slot)?;
                        doc.assign_pattern(command.scene, command.group, Some(pattern))
                    })?;
                    changed = true;
                }
                GrooveAction::Pad(edit) => match edit {
                    GroovePadEdit::Load { genre, index } => {
                        let tracks = library_tracks(genre, index)?;
                        let pattern = scene_pattern(doc.project(), command.scene, command.group)?;
                        load_tracks(
                            doc,
                            command.group,
                            pattern,
                            tracks,
                            [true; crate::groove::ROWS],
                        )?;
                        changed = true;
                    }
                    _ => {
                        let pattern = scene_pattern(doc.project(), command.scene, command.group)?;
                        changed = apply_pad_edit(doc, command.group, pattern, edit)?;
                    }
                },
                GrooveAction::TrackDelta {
                    slot,
                    row,
                    before,
                    after,
                } => {
                    doc.transaction("Edit Groove step", |doc| {
                        let pattern = pattern_in_slot(doc, command.group, slot)?;
                        apply_track_delta(doc, command.group, pattern, row, before, after)
                    })?;
                    changed = true;
                }
                GrooveAction::Mix {
                    kit,
                    mut settings,
                    rebuild,
                    replace,
                    parameters,
                    mute_changed,
                } => {
                    let solo_touched = settings.iter().any(|s| s.solo.is_some());
                    let any_solo = settings.iter().any(|s| s.solo == Some(true));
                    let mut saved = self.groove_solo_saved[command.group];
                    if solo_touched && any_solo {
                        let was_active = saved.is_some();
                        if saved.is_none() {
                            let g = doc
                                .project()
                                .groups
                                .get(command.group)
                                .ok_or("no such group")?;
                            let roles = roles_of(Some(g));
                            saved = Some(std::array::from_fn(|r| {
                                g.sounds.get(roles[r]).is_some_and(|s| s.muted)
                            }));
                        }
                        if was_active {
                            if let Some(mutes) = saved.as_mut() {
                                for row in 0..crate::groove::ROWS {
                                    if mute_changed[row] {
                                        if let Some(mute) = settings[row].mute {
                                            mutes[row] = mute;
                                        }
                                    }
                                }
                            }
                        }
                    } else if solo_touched {
                        if let Some(mutes) = saved.take() {
                            for row in 0..crate::groove::ROWS {
                                settings[row].mute = Some(if mute_changed[row] {
                                    settings[row].mute.unwrap_or(mutes[row])
                                } else {
                                    mutes[row]
                                });
                            }
                        } else {
                            for row in 0..crate::groove::ROWS {
                                if !mute_changed[row] {
                                    settings[row].mute = None;
                                    settings[row].solo = None;
                                }
                            }
                        }
                    }
                    doc.transaction("Edit Groove sound", |doc| {
                        apply_legacy_sounds(
                            doc,
                            command.group,
                            &self.kits,
                            self.shared.rate,
                            Some(kit),
                            settings,
                            rebuild,
                            replace,
                            parameters,
                        )
                    })?;
                    self.groove_solo_saved[command.group] = saved;
                    changed = true;
                }
                GrooveAction::Dice {
                    movement,
                    row,
                    locked,
                    genre,
                } => {
                    let pattern = scene_pattern(doc.project(), command.scene, command.group)?;
                    let group = &doc.project().groups[command.group];
                    let slot = group.patterns[pattern].slot;
                    let roles = roles_of(Some(group));
                    let rows = std::array::from_fn(|r| row.map_or(!locked[r], |only| only == r));
                    let before = tracks_of(&group.patterns[pattern], roles)?;
                    let mut after = before;
                    for r in 0..crate::groove::ROWS {
                        if rows[r] {
                            let role = if r == crate::groove::ROWS - 1
                                && self.legacy.sound[r].get().round() as usize == 8
                            {
                                8
                            } else {
                                r
                            };
                            groove_dice::apply(
                                movement,
                                &mut after[r],
                                role,
                                genre,
                                &self.groove_dice.habits,
                                &mut self.groove_dice.rng,
                            );
                        }
                    }
                    let (removed, added) = dice_notes(
                        &group.patterns[pattern],
                        roles,
                        rows,
                        before,
                        after,
                        movement,
                        group,
                    )?;
                    if !removed.is_empty() || !added.is_empty() {
                        doc.edit_notes("Groove Dice", command.group, pattern, &removed, &added)?;
                        if self.groove_undo.len() == 16 {
                            self.groove_undo.remove(0);
                        }
                        self.groove_undo.push(GrooveUndo {
                            generation: command.generation,
                            group: command.group,
                            slot,
                            removed,
                            added,
                        });
                        changed = true;
                    }
                    self.legacy.can_undo.set_bool(!self.groove_undo.is_empty());
                }
                GrooveAction::Undo => {
                    if let Some(edit) = self.groove_undo.last() {
                        if edit.generation != command.generation {
                            self.groove_undo.clear();
                        } else {
                            let group = doc
                                .project()
                                .groups
                                .get(edit.group)
                                .ok_or("no such group")?;
                            let pattern = group
                                .patterns
                                .iter()
                                .position(|p| p.slot == edit.slot)
                                .ok_or("Dice pattern no longer exists")?;
                            let notes = &group.patterns[pattern].notes;
                            if edit.added.iter().any(|n| !notes.contains(n))
                                || edit.removed.iter().any(|old| {
                                    notes.iter().any(|n| {
                                        n.sound == old.sound
                                            && n.position == old.position
                                            && n.pitch == old.pitch
                                            && !edit.added.contains(n)
                                    })
                                })
                            {
                                return Err("Dice undo conflicts with a later note edit".into());
                            }
                            doc.edit_notes(
                                "Undo Groove Dice",
                                edit.group,
                                pattern,
                                &edit.added,
                                &edit.removed,
                            )?;
                            self.groove_undo.pop();
                            changed = true;
                        }
                    }
                    self.legacy.can_undo.set_bool(!self.groove_undo.is_empty());
                }
                GrooveAction::Load { genre, index, slot } => {
                    let tracks = library_tracks(genre, index)?;
                    doc.transaction("Load Groove", |doc| {
                        let pattern = match slot {
                            Some(slot) => pattern_in_slot(doc, command.group, slot)?,
                            None => scene_pattern(doc.project(), command.scene, command.group)?,
                        };
                        load_tracks(
                            doc,
                            command.group,
                            pattern,
                            tracks,
                            [true; crate::groove::ROWS],
                        )
                    })?;
                    changed = true;
                }
                GrooveAction::LoadSet { genre, set, tone } => {
                    let tracks: Vec<_> = (0..crate::groove::GROOVES)
                        .map(|slot| library_tracks(genre, set * crate::groove::GROOVES + slot + 1))
                        .collect::<Result<_, _>>()?;
                    doc.transaction("Load Groove set", |doc| {
                        for (slot, tracks) in tracks.into_iter().enumerate() {
                            let pattern = pattern_in_slot(doc, command.group, slot)?;
                            load_tracks(
                                doc,
                                command.group,
                                pattern,
                                tracks,
                                [true; crate::groove::ROWS],
                            )?;
                        }
                        if let Some(defaults) = crate::patterns::set_defaults(genre, set) {
                            let settings = std::array::from_fn(|row| LegacySoundSettings {
                                sound: Some(defaults.sounds[row] as usize),
                                level: Some(defaults.levels[row]),
                                pitch: Some(defaults.pitches[row]),
                                decay: Some(defaults.decay[row]),
                                tone: Some(tone[row]),
                                ..Default::default()
                            });
                            apply_legacy_sounds(
                                doc,
                                command.group,
                                &self.kits,
                                self.shared.rate,
                                Some(defaults.kit),
                                settings,
                                [true; crate::groove::ROWS],
                                [true; crate::groove::ROWS],
                                [7; crate::groove::ROWS],
                            )?;
                            let old = doc.project().groups[command.group].swing;
                            doc.set_swing(
                                Target::Group(command.group),
                                Swing {
                                    amount: groove_swing(defaults.swing),
                                    ..old
                                },
                            )?;
                        }
                        Ok(())
                    })?;
                    changed = true;
                }
            }
        }
        if changed {
            self.publish_snapshot(true)?;
        }
        Ok(changed)
    }

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
                let mut document = project_paths::normalize(Document::from_bytes(data, origin)?)?;
                let modified = complete_groups(&mut document)? || document.is_dirty();
                self.document = Some(document);
                self.groove_solo_saved = [None; GROUPS];
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
            Request::ExportGroovePreset { group, reply } => {
                let result = (|| {
                    self.ensure_document()?;
                    let doc = self.document.as_ref().expect("initialized");
                    let group_file =
                        doc.file_bytes(FileItem::Group(group), &SoundInfo::default())?;
                    let mut isolated =
                        Document::from_bytes(doc.to_bytes()?, doc.path().map(Path::to_path_buf))?;
                    for other in (0..isolated.project().groups.len()).rev() {
                        if other != group {
                            isolated.delete_group(other)?;
                        }
                    }
                    Ok(NativeGroovePreset {
                        group_file,
                        project: isolated.to_bytes()?,
                        origin: doc.path().map(encode_path),
                    })
                })();
                let _ = reply.send(result);
                Ok(false)
            }
            Request::Open(path) => {
                self.lock_project()?;
                let mut document = project_paths::normalize(Document::open(&path)?)?;
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
                self.groove_solo_saved = [None; GROUPS];
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
                self.groove_solo_saved = [None; GROUPS];
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
            Request::Undo | Request::Redo | Request::UndoStep | Request::RedoStep => {
                self.ensure_document()?;
                let document = self.document.as_mut().expect("initialized");
                let changed = match request {
                    Request::Undo => document.undo(),
                    Request::Redo => document.redo(),
                    Request::UndoStep => document.undo_step(),
                    Request::RedoStep => document.redo_step(),
                    _ => unreachable!(),
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
    pub fn export_native_groove_preset(&self, group: usize) -> Result<NativeGroovePreset, String> {
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        self.request(Request::ExportGroovePreset { group, reply })?;
        result
            .recv()
            .map_err(|_| "Maschine editor stopped".to_string())?
    }

    pub fn load_native_groove_preset(
        &self,
        group: usize,
        scene: usize,
        source_group: usize,
        source_scene: usize,
        preset: NativeGroovePreset,
    ) -> Result<(), String> {
        self.edit(move |doc| {
            let origin = preset.origin.map(decode_path).transpose()?;
            let source = Document::from_bytes(preset.project, origin)?;
            let source_data = source
                .project()
                .groups
                .get(source_group)
                .ok_or("preset group is missing")?
                .clone();
            let old_patterns = doc
                .project()
                .groups
                .get(group)
                .ok_or("no such group")?
                .patterns
                .clone();
            let source_slot = scene_pattern(source.project(), source_scene, source_group)
                .ok()
                .and_then(|p| source_data.patterns.get(p))
                .map(|p| p.slot);

            // A preset can be loaded into a project in a different folder.
            // Resolve its zones now so they still name the original samples.
            let mut resolver = deckard_engine::library::Resolver::new(source.path());
            let mut zones = Vec::new();
            let mut takes = Vec::new();
            for (sound, data) in source_data.sounds.iter().take(SOUNDS).enumerate() {
                if let Some(sampler) = &data.sampler {
                    let mut paths = Vec::new();
                    for zone in &sampler.zones {
                        let path = if zone.sample.path.is_empty() {
                            None
                        } else {
                            Some(resolver.resolve(&zone.sample).ok_or_else(|| {
                                format!("preset sample is missing: {}", zone.sample.path)
                            })?)
                        };
                        paths.push(path);
                    }
                    zones.push((sound, paths));
                }
                if let Some(audio) = &data.audio {
                    for (take, item) in audio.takes.iter().enumerate() {
                        if !item.sample.path.is_empty() {
                            let file = resolver.resolve(&item.sample).ok_or_else(|| {
                                format!("preset sample is missing: {}", item.sample.path)
                            })?;
                            takes.push((sound, take, file));
                        }
                    }
                }
            }
            // The decoder can undo its own failed group import. Probe it on a
            // separate document first, so a malformed preset cannot change
            // the history length of the transaction editing the real project.
            let mut probe =
                Document::from_bytes(doc.to_bytes()?, doc.path().map(Path::to_path_buf))?;
            probe.load_group_file_with(group, &preset.group_file, false, false)?;
            doc.load_group_file_with(group, &preset.group_file, false, false)?;
            for pattern in &source_data.patterns {
                pattern_in_slot(doc, group, pattern.slot)?;
            }
            let mut clips = Vec::new();
            for sound in 0..source_data.sounds.len().min(SOUNDS) {
                clips.push(
                    source
                        .copy_sound(source_group, sound)?
                        .carried(&source, doc),
                );
            }
            doc.paste_sounds(group, 0, &clips)?;
            for (sound, paths) in zones {
                let specs: Vec<_> = paths
                    .into_iter()
                    .enumerate()
                    .map(|(i, file)| ZoneSpec {
                        from: ZoneFrom::Zone(i),
                        file,
                        set: Vec::new(),
                    })
                    .collect();
                doc.set_zones(group, sound, &specs, "Resolve preset samples")?;
            }
            for (sound, take, file) in takes {
                let target = Target::Take(group, sound, take);
                let fields = [
                    "length",
                    "play_range/low",
                    "play_range/high",
                    "selection_range/low",
                    "selection_range/high",
                    "unknown_98",
                ];
                let saved: Vec<_> = fields
                    .into_iter()
                    .filter_map(|field| {
                        doc.value(target, field)
                            .cloned()
                            .map(|value| (field, value))
                    })
                    .collect();
                doc.set_take_sample(group, sound, take, &file, 1, "Resolve preset take")?;
                for (field, value) in saved {
                    doc.set("Restore take settings", target, field, value)?;
                }
            }
            let slots: Vec<_> = doc.project().groups[group]
                .patterns
                .iter()
                .map(|p| p.slot)
                .collect();
            for (index, slot) in slots.into_iter().enumerate() {
                let desired = source_data.patterns.iter().find(|p| p.slot == slot);
                let old = old_patterns.iter().find(|p| p.slot == slot);
                let Some(pattern) = desired.or(old) else {
                    continue;
                };
                let current = doc.project().groups[group].patterns[index].clone();
                let notes = if desired.is_some() {
                    pattern
                        .notes
                        .iter()
                        .map(|n| doc.carry_note(&source, *n))
                        .collect::<Vec<_>>()
                } else {
                    pattern.notes.clone()
                };
                doc.edit_notes("Restore preset notes", group, index, &current.notes, &notes)?;
                doc.edit_midi(
                    "Restore preset MIDI",
                    group,
                    index,
                    &current.midi,
                    &pattern.midi,
                )?;
                if desired.is_some() {
                    doc.set_pattern_span(group, index, pattern.start, pattern.length)?;
                    doc.rename(Target::Pattern(group, index), &pattern.name)?;
                    doc.set_color(&[Target::Pattern(group, index)], pattern.color)?;
                    for sound in 0..SOUNDS {
                        let before = current
                            .audio
                            .iter()
                            .find(|cue| cue.sound == sound)
                            .map(|cue| cue.take);
                        let after = pattern
                            .audio
                            .iter()
                            .find(|cue| cue.sound == sound)
                            .map(|cue| cue.take);
                        if before != after {
                            doc.set_audio_in(
                                group,
                                sound,
                                &[index],
                                after,
                                "Restore preset audio",
                            )?;
                        }
                    }
                    let mut channels = std::collections::BTreeSet::new();
                    for point in current.mods.iter().chain(&pattern.mods) {
                        channels.insert(point.sound);
                    }
                    for sound in channels {
                        let chain = sound.map_or(Chain::Group(group), |s| Chain::Sound(group, s));
                        doc.clear_pattern_modulation(chain, index)?;
                    }
                    let mut lanes: std::collections::BTreeMap<
                        (Option<usize>, Option<usize>, String),
                        Vec<(i64, f64)>,
                    > = std::collections::BTreeMap::new();
                    for point in &pattern.mods {
                        lanes
                            .entry((point.sound, point.slot, point.field.clone()))
                            .or_default()
                            .push((point.position, point.value));
                    }
                    for ((sound, slot, field), points) in lanes {
                        let chain = sound.map_or(Chain::Group(group), |s| Chain::Sound(group, s));
                        let target = deckard_model::ModTarget { slot, field };
                        doc.set_modulation(chain, &target, index, (i64::MIN, i64::MAX), &points)?;
                    }
                }
            }
            if let Some(slot) = source_slot {
                let pattern = doc.project().groups[group]
                    .patterns
                    .iter()
                    .position(|p| p.slot == slot)
                    .ok_or("preset pattern is missing")?;
                doc.assign_pattern(scene, group, Some(pattern))?;
            }
            Ok(())
        })
    }
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

    /// Apply only the tracks actually present in an older Groove preset to the
    /// focused group. Other sounds, patterns and off-grid events stay intact.
    pub fn load_legacy_groove_preset(
        &self,
        group: usize,
        tracks: [[Option<Track>; crate::groove::ROWS]; crate::groove::GROOVES],
        swing: Option<f64>,
        kit: Option<usize>,
        settings: [LegacySoundSettings; crate::groove::ROWS],
    ) -> Result<(), String> {
        let kits = self.shared.kits.clone();
        let rate = self.shared.rate;
        self.edit(move |doc| {
            apply_legacy_sounds(
                doc,
                group,
                &kits,
                rate,
                kit,
                settings,
                [true; crate::groove::ROWS],
                [true; crate::groove::ROWS],
                [7; crate::groove::ROWS],
            )?;
            for (slot, rows) in tracks.into_iter().enumerate() {
                if rows.iter().all(Option::is_none) {
                    continue;
                }
                let pattern = pattern_in_slot(doc, group, slot)?;
                let g = doc.project().groups.get(group).ok_or("no such group")?;
                let roles = roles_of(Some(g));
                let mut next = tracks_of(&g.patterns[pattern], roles)?;
                let mut selected = [false; crate::groove::ROWS];
                for (row, track) in rows.into_iter().enumerate() {
                    if let Some(track) = track {
                        next[row] = track;
                        selected[row] = true;
                    }
                }
                load_tracks(doc, group, pattern, next, selected)?;
            }
            if let Some(swing) = swing {
                let old = doc
                    .project()
                    .groups
                    .get(group)
                    .ok_or("no such group")?
                    .swing;
                doc.set_swing(
                    Target::Group(group),
                    Swing {
                        amount: groove_swing(swing),
                        ..old
                    },
                )?;
            }
            Ok(())
        })
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

    /// Undo one sub-edit of a take without wrapping history changes in an edit transaction.
    pub fn undo_step(&self) -> Result<bool, String> {
        let changed = self.request(Request::UndoStep)?;
        if changed {
            self.shared.prepare()?;
        }
        Ok(changed)
    }

    /// Redo one sub-edit of a take without wrapping history changes in an edit transaction.
    pub fn redo_step(&self) -> Result<bool, String> {
        let changed = self.request(Request::RedoStep)?;
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
    groove_commands: rtrb::Producer<GrooveCommand>,
    shared: Arc<Shared>,
    groove_set_seen: u32,
    groove_swing_seen: u32,
    groove_slot_seen: u32,
    groove_chain_seen: u32,
    groove_tracks_seen: [[Track; crate::groove::ROWS]; crate::groove::GROOVES],
    groove_mix_seen: LegacyMixSnapshot,
    groove_mix_group: usize,
    groove_mix_pending: Option<GrooveCommand>,
    groove_mix_next_frame: u64,
    vary_habits: Arc<Habits>,
    vary_rng: groove_dice::Rng,
    leader_key: Option<u8>,
    kick_semitones: i64,
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
    legacy_sounds: [[bool; SOUNDS]; GROUPS],
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
        let (groove_commands, groove_commands_rx) = rtrb::RingBuffer::new(64);
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
        let vary_habits = Habits::shared();
        let _ = engine.set_sequence_transform(Some(Box::new(GrooveVariation {
            habits: vary_habits.clone(),
            rng: groove_dice::Rng(0x9e37_79b9 ^ ((deck as u32 + 1) * 7919)),
        })));
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
            groove_overflow: AtomicBool::new(false),
            groove_lost: AtomicBool::new(false),
            rate: rate as u32,
            kits: kits.clone(),
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
        let audio_shared = shared.clone();
        let groove_set_seen = legacy.load_set.version();
        let groove_swing_seen = legacy.swing.version();
        let groove_slot_seen = legacy.groove.version();
        let groove_chain_seen = legacy.chain.version();
        let groove_tracks_seen = std::array::from_fn(|slot| legacy.tracks(slot));
        let groove_mix_seen = legacy_mix(&legacy);
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
                    groove_commands: groove_commands_rx,
                    groove_dice: Dice::new(
                        Habits::shared(),
                        0x9e37_79b9 ^ ((deck as u32 + 1) * 7919),
                    ),
                    groove_undo: Vec::new(),
                    groove_solo_saved: [None; GROUPS],
                    autosave: autosave_path(deck),
                    project_lock: None,
                    backed_up: false,
                    dirty_since: None,
                    shared,
                };
                let mut initialized = false;
                let mut attempted = false;
                let mut groove_prepare_pending = false;
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
                    if editor.shared.groove_overflow.swap(false, Ordering::AcqRel) {
                        editor.shared.groove_lost.store(true, Ordering::Release);
                        editor
                            .shared
                            .set_error("Groove edit queue is full; repeat the last action".into());
                    }
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
                    match editor.take_groove_commands() {
                        Ok(true) => groove_prepare_pending = true,
                        Ok(false) => {}
                        Err(error) => editor.shared.set_error(error),
                    }
                    if groove_prepare_pending {
                        match editor.shared.prepare() {
                            Ok(()) => groove_prepare_pending = false,
                            Err(error) if error == "Maschine audio update queue is full" => {}
                            Err(error) => {
                                groove_prepare_pending = false;
                                editor.shared.set_error(error);
                            }
                        }
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
                        let group = worker_controls.group.get().round() as usize;
                        let pattern = worker_controls.pattern.get().round() as usize;
                        if group < GROUPS {
                            selected_patterns[group] = pattern;
                        }
                        // Editor focus is independent of the scene's playback assignment.
                        // Launches use SelectPattern or an explicit document assignment.
                    }
                    if scene_changed {
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
                groove_commands,
                shared: audio_shared,
                groove_set_seen,
                groove_swing_seen,
                groove_slot_seen,
                groove_chain_seen,
                groove_tracks_seen,
                groove_mix_seen,
                groove_mix_group: 0,
                groove_mix_pending: None,
                groove_mix_next_frame: 0,
                vary_habits,
                vary_rng: groove_dice::Rng(0x85eb_ca6b ^ ((deck as u32 + 1) * 3571)),
                leader_key: None,
                kick_semitones: 0,
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
                legacy_sounds: [[false; SOUNDS]; GROUPS],
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

    /// Establish a baseline without copying saved legacy settings over an
    /// already existing DECKARD project on activation.
    pub fn begin_groove_bridge(&mut self, controls: &GrooveControls) {
        self.groove_swing_seen = controls.swing.version();
        self.groove_slot_seen = controls.groove.version();
        self.groove_chain_seen = controls.chain.version();
        self.groove_tracks_seen = std::array::from_fn(|slot| controls.tracks(slot));
        self.groove_mix_seen = legacy_mix(controls);
        self.groove_mix_group = (self.controls.group.get().round() as usize).min(GROUPS - 1);
    }

    pub fn set_groove_key(&mut self, key: Option<u8>) {
        self.leader_key = key;
    }

    pub fn observe_inactive_groove(&mut self, controls: &GrooveControls) {
        self.groove_set_seen = controls.load_set.version();
        self.groove_swing_seen = controls.swing.version();
        self.groove_slot_seen = controls.groove.version();
        self.groove_chain_seen = controls.chain.version();
        self.groove_tracks_seen = std::array::from_fn(|slot| controls.tracks(slot));
        self.groove_mix_seen = legacy_mix(controls);
        self.groove_mix_group = (self.controls.group.get().round() as usize).min(GROUPS - 1);
    }

    fn send_groove_action(&mut self, action: GrooveAction) {
        let command = GrooveCommand {
            generation: self.shared.project_generation.load(Ordering::Acquire),
            group: (self.controls.group.get().round() as usize).min(GROUPS - 1),
            scene: (self.controls.scene.get().round() as usize).min(127),
            action,
        };
        if self.groove_commands.push(command).is_err() {
            self.controls.error.set_bool(true);
            self.shared.groove_overflow.store(true, Ordering::Release);
        }
    }

    fn flush_legacy_mix(&mut self, force: bool) {
        let now = self.engine.output_frame();
        if !force && now < self.groove_mix_next_frame {
            return;
        }
        if let Some(command) = self.groove_mix_pending.take() {
            match self.groove_commands.push(command) {
                Ok(()) => self.groove_mix_next_frame = now.saturating_add((self.rate * 0.1) as u64),
                Err(_) => self.groove_mix_pending = Some(command),
            }
        }
    }

    fn capture_legacy_mix(&mut self, controls: &GrooveControls) {
        let group = (self.controls.group.get().round() as usize).min(GROUPS - 1);
        let now = legacy_mix(controls);
        if group != self.groove_mix_group {
            self.flush_legacy_mix(true);
            self.groove_mix_group = group;
            self.groove_mix_seen = now;
        } else if let Some(action) = changed_mix(self.groove_mix_seen, now) {
            let generation = self.shared.project_generation.load(Ordering::Acquire);
            if self
                .groove_mix_pending
                .as_ref()
                .is_some_and(|p| p.group != group || p.generation != generation)
            {
                self.flush_legacy_mix(true);
            }
            if let Some(pending) = self.groove_mix_pending.as_mut() {
                if pending.group == group && pending.generation == generation {
                    merge_mix(&mut pending.action, action);
                } else {
                    self.controls.error.set_bool(true);
                    self.shared.groove_overflow.store(true, Ordering::Release);
                }
            } else {
                self.groove_mix_pending = Some(GrooveCommand {
                    generation,
                    group,
                    scene: (self.controls.scene.get().round() as usize).min(127),
                    action,
                });
            }
        }
        self.flush_legacy_mix(false);
    }

    /// Capture the explicit actions before the legacy scheduler consumes the
    /// push controls. The worker prepares their timeline for the next bar.
    pub fn capture_groove_actions(&mut self, c: &GrooveControls) {
        self.capture_legacy_mix(c);
        for slot in 0..crate::groove::GROOVES {
            let tracks = c.tracks(slot);
            for (row, &after) in tracks.iter().enumerate() {
                let before = self.groove_tracks_seen[slot][row];
                if before != after {
                    self.send_groove_action(GrooveAction::TrackDelta {
                        slot,
                        row,
                        before,
                        after,
                    });
                }
            }
        }
        let genre = c.genre.get().round() as usize;
        if c.load.get_bool() {
            self.send_groove_action(GrooveAction::Load {
                genre,
                index: c.pattern.get().round() as usize,
                slot: None,
            });
        }
        if c.load_chain.get_bool() {
            let chosen = (c.groove.get().round() as usize).min(crate::groove::GROOVES - 1);
            self.send_groove_action(GrooveAction::Load {
                genre,
                index: c.chain.get().round() as usize,
                slot: Some(chosen ^ 1),
            });
        }
        if c.load_set.get_bool() || c.load_set.version() != self.groove_set_seen {
            self.send_groove_action(GrooveAction::LoadSet {
                genre,
                set: c.library_set.get().round() as usize,
                tone: std::array::from_fn(|row| c.tone[row].get()),
            });
        }
        if let Some(movement) = Move::from_number(c.dice.get().round() as u8) {
            self.send_groove_action(GrooveAction::Dice {
                movement,
                row: (c.dice_row.get().round() as usize)
                    .min(crate::groove::ROWS)
                    .checked_sub(1),
                locked: std::array::from_fn(|r| c.lock[r].get_bool()),
                genre,
            });
        }
        if c.dice_undo.get_bool() {
            self.send_groove_action(GrooveAction::Undo);
        }
    }

    pub fn capture_groove_changes(
        &mut self,
        c: &GrooveControls,
        signals: &[GrooveStep],
        edits: impl Iterator<Item = GroovePadEdit>,
    ) {
        self.groove_set_seen = c.load_set.version();
        self.groove_tracks_seen = std::array::from_fn(|slot| c.tracks(slot));
        self.groove_mix_seen = legacy_mix(c);
        let version = c.swing.version();
        if version != self.groove_swing_seen {
            self.send_groove_action(GrooveAction::Swing(c.swing.get()));
            self.groove_swing_seen = version;
        }
        for edit in edits {
            if !matches!(edit, GroovePadEdit::Record { .. }) || !self.controls.record.get_bool() {
                self.send_groove_action(GrooveAction::Pad(edit));
            }
        }
        let slot_changed = c.groove.version() != self.groove_slot_seen;
        let chain_changed = c.chain.version() != self.groove_chain_seen;
        self.groove_slot_seen = c.groove.version();
        self.groove_chain_seen = c.chain.version();
        let chosen = (c.groove.get().round() as usize).min(crate::groove::GROOVES - 1);
        if slot_changed {
            self.send_groove_action(GrooveAction::SelectSlot(chosen));
        } else if c.chain.get() >= 0.5 && (chain_changed || signals.iter().any(|s| s.step == 0)) {
            let playing = (c.groove_playing.get().round() as usize).min(crate::groove::GROOVES - 1);
            self.send_groove_action(GrooveAction::SelectSlot(if playing == chosen {
                chosen ^ 1
            } else {
                chosen
            }));
        } else if chain_changed {
            self.send_groove_action(GrooveAction::SelectSlot(chosen));
        }
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
            std::mem::swap(&mut self.legacy_sounds, &mut next.legacy_sounds);
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
        manual: &[GrooveManualHit],
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
        let kit =
            (controls.kit.get().round() as usize).min(self.shared.kits.len().saturating_sub(1));
        let sound = controls.sound[0].get().round() as usize;
        let hz = if self.legacy_sounds[group][self.roles[group][0]] {
            self.shared
                .kits
                .get(kit)
                .and_then(|k| k.sounds.get(sound))
                .map_or(0.0, |s| s.pitch)
        } else {
            0.0
        };
        let tuning =
            crate::groove::kick_shift(hz, self.leader_key, controls.kick_key.get().round() as u8);
        self.kick_semitones = tuning.map_or(0, |(shift, _)| i64::from(shift));
        controls
            .kick_note
            .set(tuning.map_or(0.0, |(_, note)| f64::from(note) + 1.0));
        self.engine.set_sequence_pitch_shift(
            Some(group),
            self.roles[group][0],
            self.kick_semitones,
        );
        let step = controls.position.get().round().max(0.0) as usize;
        self.update_vary_context(controls, group, step);
        for hit in manual {
            let sound = self.roles[group][hit.row.min(crate::groove::ROWS - 1)];
            let velocity = ((hit.velocity.clamp(0.0, 1.25) as f64 / 1.25).cbrt() * 127.0)
                .round()
                .clamp(1.0, 127.0) as i64;
            self.engine.schedule(
                0,
                Live::NoteOn {
                    group,
                    sound,
                    pitch: (self.base_keys[group][sound]
                        + if hit.row == 0 { self.kick_semitones } else { 0 })
                    .clamp(0, 127),
                    velocity,
                    length: PPQ / 4,
                },
            );
        }
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
                        + if sound == self.roles[group][0] {
                            self.kick_semitones
                        } else {
                            0
                        }
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
        self.update_vary_context(controls, group, signal.step);
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
        let vary = controls.vary.get() as f32;
        let genre = controls.genre.get().round() as usize;
        for row in 0..crate::groove::ROWS {
            let role = if row == crate::groove::ROWS - 1
                && controls.sound[row].get().round() as usize == 8
            {
                8
            } else {
                row
            };
            if plan.gate & (1 << roles[row]) != 0
                || normal[row] > 0.0
                || written[row] == 0
                || !groove_dice::ghost(
                    vary,
                    role,
                    signal.step,
                    genre,
                    &self.vary_habits,
                    &mut self.vary_rng,
                )
            {
                continue;
            }
            let gain = groove_dice::humanize(0.35, vary, &mut self.vary_rng);
            let velocity = ((gain.clamp(0.0, 1.25) as f64 / 1.25).cbrt() * 127.0)
                .round()
                .clamp(1.0, 127.0) as i64;
            let sound = roles[row];
            self.engine.note_on_generated(
                group,
                sound,
                (self.base_keys[group][sound] + if row == 0 { self.kick_semitones } else { 0 })
                    .clamp(0, 127),
                velocity,
                PPQ / 4,
            );
        }
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
                (self.base_keys[group][sound] + if row == 0 { self.kick_semitones } else { 0 })
                    .clamp(0, 127),
                velocity,
                PPQ / 4,
            );
        }
    }

    fn update_vary_context(&mut self, controls: &GrooveControls, group: usize, step: usize) {
        let mut context = [0u64; 16];
        context[0] = group as u64;
        context[1] = controls.genre.get().round() as u64;
        context[2] = (controls.vary.get() as f32).to_bits() as u64;
        context[3] = (step % crate::groove::STEPS) as u64;
        for (row, sound) in self.roles[group].iter().enumerate() {
            context[4 + row] = *sound as u64;
        }
        context[12] = controls.sound[crate::groove::ROWS - 1].get().round() as u64;
        self.engine.set_sequence_transform_context(context);
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

fn groove_swing(value: f64) -> f64 {
    ((1.0 + 80.0 * value.clamp(0.0, 0.75) / 9.0).sqrt() - 1.0) * 50.0
}

fn legacy_mix(c: &GrooveControls) -> LegacyMixSnapshot {
    LegacyMixSnapshot {
        kit: c.kit.get().round() as usize,
        rows: std::array::from_fn(|r| LegacySoundSettings {
            sound: Some(c.sound[r].get().round() as usize),
            level: Some(c.level[r].get()),
            pitch: Some(c.pitch[r].get()),
            decay: Some(c.decay[r].get()),
            tone: Some(c.tone[r].get()),
            pan: Some(c.pan[r].get()),
            mute: Some(c.mute[r].get_bool()),
            solo: Some(c.solo[r].get_bool()),
        }),
    }
}

fn changed_mix(before: LegacyMixSnapshot, after: LegacyMixSnapshot) -> Option<GrooveAction> {
    let kit_changed = before.kit != after.kit;
    let mute_changed: [bool; crate::groove::ROWS] =
        std::array::from_fn(|r| before.rows[r].mute != after.rows[r].mute);
    let solo_changed = (0..crate::groove::ROWS).any(|r| before.rows[r].solo != after.rows[r].solo);
    let solo_active = after.rows.iter().any(|s| s.solo == Some(true));
    let mut settings = [LegacySoundSettings::default(); crate::groove::ROWS];
    let mut rebuild = [false; crate::groove::ROWS];
    let mut replace = [false; crate::groove::ROWS];
    let mut parameters = [0u8; crate::groove::ROWS];
    for row in 0..crate::groove::ROWS {
        let (a, b) = (before.rows[row], after.rows[row]);
        replace[row] = kit_changed || a.sound != b.sound;
        parameters[row] = u8::from(a.pitch != b.pitch)
            | (u8::from(a.decay != b.decay) << 1)
            | (u8::from(a.tone != b.tone) << 2);
        rebuild[row] = replace[row] || parameters[row] != 0;
        if rebuild[row] {
            settings[row].sound = b.sound;
            settings[row].pitch = b.pitch;
            settings[row].decay = b.decay;
            settings[row].tone = b.tone;
        }
        if a.level != b.level {
            settings[row].level = b.level;
        }
        if a.pan != b.pan {
            settings[row].pan = b.pan;
        }
        if solo_changed || (solo_active && mute_changed.iter().any(|v| *v)) {
            settings[row].mute = b.mute;
            settings[row].solo = b.solo;
        } else if a.mute != b.mute {
            settings[row].mute = b.mute;
        }
    }
    (rebuild.iter().any(|v| *v)
        || settings
            .iter()
            .any(|s| *s != LegacySoundSettings::default()))
    .then_some(GrooveAction::Mix {
        kit: after.kit,
        settings,
        rebuild,
        replace,
        parameters,
        mute_changed,
    })
}

fn merge_mix(previous: &mut GrooveAction, next: GrooveAction) {
    let (
        GrooveAction::Mix {
            kit,
            settings,
            rebuild,
            replace,
            parameters,
            mute_changed,
        },
        GrooveAction::Mix {
            kit: next_kit,
            settings: next_settings,
            rebuild: next_rebuild,
            replace: next_replace,
            parameters: next_parameters,
            mute_changed: next_mute_changed,
        },
    ) = (previous, next)
    else {
        return;
    };
    *kit = next_kit;
    for row in 0..crate::groove::ROWS {
        rebuild[row] |= next_rebuild[row];
        replace[row] |= next_replace[row];
        parameters[row] |= next_parameters[row];
        mute_changed[row] |= next_mute_changed[row];
        let (a, b) = (&mut settings[row], next_settings[row]);
        if b.sound.is_some() {
            a.sound = b.sound;
        }
        if b.level.is_some() {
            a.level = b.level;
        }
        if b.pitch.is_some() {
            a.pitch = b.pitch;
        }
        if b.decay.is_some() {
            a.decay = b.decay;
        }
        if b.tone.is_some() {
            a.tone = b.tone;
        }
        if b.pan.is_some() {
            a.pan = b.pan;
        }
        if b.mute.is_some() {
            a.mute = b.mute;
        }
        if b.solo.is_some() {
            a.solo = b.solo;
        }
    }
}

fn scene_pattern(project: &Project, scene: usize, group: usize) -> Result<usize, String> {
    project
        .scenes
        .get(scene)
        .and_then(|s| s.patterns.iter().find(|(g, _)| *g == group))
        .map(|(_, p)| *p)
        .filter(|p| {
            project
                .groups
                .get(group)
                .is_some_and(|g| *p < g.patterns.len())
        })
        .ok_or_else(|| "the focused group has no pattern in this scene".into())
}

fn pattern_in_slot(doc: &mut Document, group: usize, slot: usize) -> Result<usize, String> {
    let patterns = &doc
        .project()
        .groups
        .get(group)
        .ok_or("no such group")?
        .patterns;
    if let Some(index) = patterns.iter().position(|p| p.slot == slot) {
        return Ok(index);
    }
    doc.add_pattern_in_slot(group, 0, slot, &format!("Pattern {}", slot + 1))
}

fn grid_step(pattern: &Pattern, note: &Note) -> Option<usize> {
    let relative = note.position - pattern.start;
    let grid = PPQ / 4;
    (relative >= 0 && relative < PPQ * 4 && relative % grid == 0)
        .then_some((relative / grid) as usize)
}

fn tracks_of(
    pattern: &Pattern,
    roles: [usize; crate::groove::ROWS],
) -> Result<[Track; crate::groove::ROWS], String> {
    if pattern.length < PPQ * 4 {
        return Err("Groove actions need a pattern at least one bar long".into());
    }
    let mut tracks = [Track::default(); crate::groove::ROWS];
    for note in &pattern.notes {
        let Some(step) = grid_step(pattern, note) else {
            continue;
        };
        let Some(row) = roles.iter().position(|sound| *sound == note.sound) else {
            continue;
        };
        let bit = 1 << step;
        tracks[row].hits |= bit;
        if note.velocity <= 105 {
            tracks[row].soft |= bit;
        }
        if note.velocity >= 123 {
            tracks[row].accent |= bit;
        }
        if note.probability <= 50 {
            tracks[row].maybe |= bit;
        }
    }
    Ok(tracks)
}

fn note_for_track(
    pattern: &Pattern,
    sound: usize,
    step: usize,
    track: Track,
    group: &deckard_model::Group,
) -> Note {
    let bit = 1 << step;
    let gain: f64 = if track.accent & bit != 0 {
        1.25
    } else if track.soft & bit != 0 {
        0.5
    } else {
        1.0
    };
    Note {
        sound,
        position: pattern.start + step as i64 * PPQ / 4,
        length: PPQ / 4,
        pitch: group
            .sounds
            .get(sound)
            .map_or(60, |s| s.channel.base_key.clamp(0, 127)),
        velocity: ((gain / 1.25).cbrt() * 127.0).round() as i64,
        probability: if track.maybe & bit != 0 { 50 } else { 100 },
        expr: None,
    }
}

fn dice_notes(
    pattern: &Pattern,
    roles: [usize; crate::groove::ROWS],
    rows: [bool; crate::groove::ROWS],
    before: [Track; crate::groove::ROWS],
    after: [Track; crate::groove::ROWS],
    movement: Move,
    group: &deckard_model::Group,
) -> Result<(Vec<Note>, Vec<Note>), String> {
    // An explicit rotation moves the actual notes, including their expression,
    // pitches and probabilities, even when every grid cell is occupied.
    let rotation = match movement {
        Move::RotateRight => Some(1),
        Move::RotateLeft => Some(15),
        _ => None,
    };
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for (row, sound) in roles.into_iter().enumerate() {
        if !rows[row] {
            continue;
        }
        if let Some(by) = rotation {
            for note in pattern.notes.iter().filter(|n| n.sound == sound) {
                if let Some(step) = grid_step(pattern, note) {
                    removed.push(*note);
                    added.push(Note {
                        position: pattern.start + ((step + by) % 16) as i64 * PPQ / 4,
                        ..*note
                    });
                }
            }
            continue;
        }
        let lost = before[row].hits & !after[row].hits;
        let gained = after[row].hits & !before[row].hits;
        for note in pattern.notes.iter().filter(|n| n.sound == sound) {
            let Some(step) = grid_step(pattern, note) else {
                continue;
            };
            if lost & (1 << step) != 0 {
                removed.push(*note);
            } else if matches!(movement, Move::New | Move::Even)
                && after[row].hits & (1 << step) != 0
            {
                let template = note_for_track(pattern, sound, step, after[row], group);
                if note.velocity != template.velocity || note.probability != template.probability {
                    removed.push(*note);
                    added.push(Note {
                        velocity: template.velocity,
                        probability: template.probability,
                        ..*note
                    });
                }
            }
        }
        let mut used = 0u32;
        for step in 0..crate::groove::STEPS {
            if gained & (1 << step) != 0 {
                let from = if matches!(movement, Move::Mutate | Move::Even) {
                    (0..crate::groove::STEPS)
                        .filter(|&old| lost & !used & (1 << old) != 0)
                        .filter(|&old| {
                            movement == Move::Even
                                || [before[row].soft, before[row].accent, before[row].maybe]
                                    .into_iter()
                                    .zip([after[row].soft, after[row].accent, after[row].maybe])
                                    .all(|(a, b)| (a & (1 << old) != 0) == (b & (1 << step) != 0))
                        })
                        .min_by_key(|&old| {
                            let d = old.abs_diff(step);
                            d.min(crate::groove::STEPS - d)
                        })
                } else {
                    None
                };
                if let Some(from) = from {
                    used |= 1 << from;
                    let template = note_for_track(pattern, sound, step, after[row], group);
                    for note in pattern
                        .notes
                        .iter()
                        .filter(|n| n.sound == sound && grid_step(pattern, n) == Some(from))
                    {
                        let mut moved = Note {
                            position: template.position,
                            ..*note
                        };
                        if movement == Move::Even {
                            moved.velocity = template.velocity;
                            moved.probability = template.probability;
                        }
                        added.push(moved);
                    }
                } else {
                    added.push(note_for_track(pattern, sound, step, after[row], group));
                }
            }
        }
    }
    Ok((removed, added))
}

fn library_tracks(genre: usize, index: usize) -> Result<[Track; crate::groove::ROWS], String> {
    let p = index
        .checked_sub(1)
        .and_then(|i| crate::patterns::groove(genre, i))
        .ok_or("no such library groove")?;
    let cymbal = if p.rows[8].hits != 0 { 8 } else { 7 };
    Ok(std::array::from_fn(|row| {
        let s = p.rows[if row == crate::groove::ROWS - 1 {
            cymbal
        } else {
            row
        }];
        Track {
            hits: s.hits,
            soft: s.soft,
            accent: s.accent,
            maybe: s.maybe,
        }
    }))
}

fn load_tracks(
    doc: &mut Document,
    group: usize,
    pattern: usize,
    tracks: [Track; crate::groove::ROWS],
    selected: [bool; crate::groove::ROWS],
) -> Result<(), String> {
    let g = doc.project().groups.get(group).ok_or("no such group")?;
    let p = g.patterns.get(pattern).ok_or("no such pattern")?;
    let roles = roles_of(Some(g));
    let _ = tracks_of(p, roles)?;
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for (row, sound) in roles.into_iter().enumerate() {
        if !selected[row] {
            continue;
        }
        for step in 0..crate::groove::STEPS {
            let present = tracks[row].hits & (1 << step) != 0;
            let template = note_for_track(p, sound, step, tracks[row], g);
            let existing: Vec<_> = p
                .notes
                .iter()
                .filter(|n| n.sound == sound && grid_step(p, n) == Some(step))
                .copied()
                .collect();
            for n in &existing {
                if !present {
                    removed.push(*n);
                } else if n.velocity != template.velocity || n.probability != template.probability {
                    removed.push(*n);
                    added.push(Note {
                        velocity: template.velocity,
                        probability: template.probability,
                        ..*n
                    });
                }
            }
            if present && existing.is_empty() {
                added.push(template);
            }
        }
    }
    doc.edit_notes("Load Groove", group, pattern, &removed, &added)
}

fn apply_pad_edit(
    doc: &mut Document,
    group: usize,
    pattern: usize,
    edit: GroovePadEdit,
) -> Result<bool, String> {
    let g = doc.project().groups.get(group).ok_or("no such group")?;
    let p = g.patterns.get(pattern).ok_or("no such pattern")?;
    let roles = roles_of(Some(g));
    let _ = tracks_of(p, roles)?;
    let (row, step, clear, record) = match edit {
        GroovePadEdit::Toggle { row, step } => (row, Some(step), false, false),
        GroovePadEdit::Clear { row } => (row, None, true, false),
        GroovePadEdit::Record { row, step } => (row, Some(step), false, true),
        GroovePadEdit::Load { .. } => return Err("not a pad step edit".into()),
    };
    let sound = *roles.get(row).ok_or("no such Groove row")?;
    let removed: Vec<_> = p
        .notes
        .iter()
        .filter(|n| n.sound == sound && grid_step(p, n).is_some_and(|s| clear || Some(s) == step))
        .copied()
        .collect();
    if clear {
        if removed.is_empty() {
            return Ok(false);
        }
        doc.edit_notes("Clear Groove row", group, pattern, &removed, &[])?;
    } else if !removed.is_empty() {
        if record {
            return Ok(false);
        }
        doc.edit_notes("Toggle Groove step", group, pattern, &removed, &[])?;
    } else {
        let step = step
            .filter(|s| *s < crate::groove::STEPS)
            .ok_or("no such Groove step")?;
        let note = note_for_track(p, sound, step, Track::default(), g);
        doc.edit_notes("Add Groove step", group, pattern, &[], &[note])?;
    }
    Ok(true)
}

fn apply_track_delta(
    doc: &mut Document,
    group: usize,
    pattern: usize,
    row: usize,
    before: Track,
    after: Track,
) -> Result<(), String> {
    let g = doc.project().groups.get(group).ok_or("no such group")?;
    let p = g.patterns.get(pattern).ok_or("no such pattern")?;
    let roles = roles_of(Some(g));
    let sound = *roles.get(row).ok_or("no such Groove row")?;
    let current = tracks_of(p, roles)?[row];
    let hit_delta = before.hits ^ after.hits;
    let soft_delta = before.soft ^ after.soft;
    let accent_delta = before.accent ^ after.accent;
    let maybe_delta = before.maybe ^ after.maybe;
    let merged = Track {
        hits: (current.hits & !hit_delta) | (after.hits & hit_delta),
        soft: (current.soft & !soft_delta) | (after.soft & soft_delta),
        accent: (current.accent & !accent_delta) | (after.accent & accent_delta),
        maybe: (current.maybe & !maybe_delta) | (after.maybe & maybe_delta),
    };
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for step in 0..crate::groove::STEPS {
        let bit = 1 << step;
        if (hit_delta | soft_delta | accent_delta | maybe_delta) & bit == 0 {
            continue;
        }
        let existing: Vec<_> = p
            .notes
            .iter()
            .filter(|n| n.sound == sound && grid_step(p, n) == Some(step))
            .copied()
            .collect();
        let present = if hit_delta & bit != 0 {
            after.hits & bit != 0
        } else {
            !existing.is_empty()
        };
        if !present {
            removed.extend(existing);
        } else if existing.is_empty() {
            // A newly enabled step uses the explicitly supplied masks.
            added.push(note_for_track(p, sound, step, after, g));
        } else {
            let template = note_for_track(p, sound, step, merged, g);
            for note in existing {
                let velocity = if (soft_delta | accent_delta) & bit != 0 {
                    template.velocity
                } else {
                    note.velocity
                };
                let probability = if maybe_delta & bit != 0 {
                    template.probability
                } else {
                    note.probability
                };
                if velocity != note.velocity || probability != note.probability {
                    removed.push(note);
                    added.push(Note {
                        velocity,
                        probability,
                        ..note
                    });
                }
            }
        }
    }
    doc.edit_notes("Edit Groove step", group, pattern, &removed, &added)
}

fn apply_legacy_sounds(
    doc: &mut Document,
    group: usize,
    kits: &[Kit],
    rate: u32,
    kit: Option<usize>,
    settings: [LegacySoundSettings; crate::groove::ROWS],
    rebuild: [bool; crate::groove::ROWS],
    replace: [bool; crate::groove::ROWS],
    parameters: [u8; crate::groove::ROWS],
) -> Result<(), String> {
    let roles = roles_of(doc.project().groups.get(group));
    let selected_kit = kit
        .map(|i| kits.get(i).ok_or("no such Groove kit"))
        .transpose()?;
    let any_solo = settings.iter().any(|s| s.solo == Some(true));
    for (row, sound) in roles.into_iter().enumerate() {
        let setting = settings[row];
        let current = doc
            .project()
            .groups
            .get(group)
            .and_then(|g| g.sounds.get(sound))
            .ok_or("no such sound")?;
        let managed = legacy_samples_root().is_ok_and(|root| is_managed_sound(current, &root));
        if rebuild[row] && !replace[row] && !managed {
            if current.sampler.is_none() {
                return Err(format!(
                    "Groove pitch, decay and tone cannot modify this group's non-Sampler sound {}",
                    sound + 1
                ));
            }
            apply_native_sampler_settings(doc, group, sound, setting, parameters[row])?;
        } else if let Some(kit) = selected_kit.filter(|_| rebuild[row]) {
            let sample = kit
                .sounds
                .get(setting.sound.unwrap_or(row))
                .ok_or("no such Groove kit sound")?;
            let (file, frames) = persist_processed_sample(
                &sample.sample,
                rate,
                setting.pitch.unwrap_or(0.0),
                setting.decay.unwrap_or(1.0),
                setting.tone.unwrap_or(0.0),
            )?;
            if managed && !replace[row] {
                let zone = &current.sampler.as_ref().expect("managed sampler").zones[0];
                let mut set = Vec::new();
                if parameters[row] & 1 != 0 {
                    let bytes = std::fs::metadata(&zone.sample.path)
                        .map_err(|e| format!("{}: {e}", zone.sample.path))?
                        .len();
                    let old_frames = bytes
                        .checked_sub(44)
                        .filter(|n| n % 4 == 0)
                        .ok_or("invalid managed Groove sample")?
                        / 4;
                    let old_last = old_frames.saturating_sub(1).max(1) as f64;
                    let new_last = frames.saturating_sub(1) as f64;
                    let scale = |n: i64| {
                        ((n.max(0) as f64 / old_last) * new_last)
                            .round()
                            .clamp(0.0, new_last)
                    };
                    set.extend([
                        ("play_range/low", scale(zone.play_range.0)),
                        ("play_range/high", scale(zone.play_range.1)),
                        ("loop_range/low", scale(zone.loop_range.0)),
                        ("loop_range/high", scale(zone.loop_range.1)),
                    ]);
                }
                doc.set_zones(
                    group,
                    sound,
                    &[ZoneSpec {
                        from: ZoneFrom::Zone(0),
                        file: Some(file),
                        set,
                    }],
                    "Update Groove sample",
                )?;
            } else {
                let first_is_instrument = doc.project().groups[group].sounds[sound]
                    .slots
                    .first()
                    .is_some_and(|s| s.is_instrument());
                if first_is_instrument {
                    doc.remove_slot(group, sound, 0)?;
                }
                doc.add_slot(group, sound, 0, &NewModule::Sampler)?;
                doc.set(
                    "Set Groove polyphony",
                    Target::Sampler(group, sound),
                    "polyphony",
                    Value::Int(1),
                )?;
                // The processed file already includes pitch, decay and tone.
                d_set_legacy_zone(doc, group, sound, file, frames)?;
                doc.rename(Target::Sound(group, sound), sample.name)?;
            }
        }
        if let Some(level) = setting.level {
            let gain = (level * 1.25).max(0.0);
            let level = if gain > 0.0 {
                deckard_model::level_from_db(20.0 * gain.log10())
            } else {
                0.0
            };
            doc.set_level(Target::Sound(group, sound), level)?;
        }
        if let Some(pan) = setting.pan {
            doc.set_pan(Target::Sound(group, sound), pan)?;
        }
        if any_solo || setting.mute.is_some() || setting.solo.is_some() {
            doc.set_mute(
                Target::Sound(group, sound),
                if any_solo {
                    setting.solo != Some(true)
                } else {
                    setting.mute.unwrap_or(false)
                },
            )?;
        }
    }
    Ok(())
}

fn is_managed_sound(sound: &Sound, root: &Path) -> bool {
    sound
        .sampler
        .as_ref()
        .is_some_and(|s| s.zones.len() == 1 && Path::new(&s.zones[0].sample.path).starts_with(root))
}

fn apply_native_sampler_settings(
    doc: &mut Document,
    group: usize,
    sound: usize,
    settings: LegacySoundSettings,
    parameters: u8,
) -> Result<(), String> {
    let target = Target::Sampler(group, sound);
    if parameters & 1 != 0 {
        doc.set(
            "Groove pitch",
            target,
            "tune",
            Value::Float(settings.pitch.unwrap_or(0.0) / 36.0),
        )?;
    }
    if parameters & 2 != 0 {
        let decay = settings.decay.unwrap_or(1.0).clamp(0.0, 1.0);
        doc.set(
            "Groove decay mode",
            target,
            "envelope_type",
            Value::Int(if decay >= 0.999 { 0 } else { 1 }),
        )?;
        if decay < 0.999 {
            let wanted_ms = (0.02 + decay * decay * 1.2) * 5000.0;
            let (mut low, mut high) = (0.0, 1.0);
            for _ in 0..32 {
                let mid = (low + high) / 2.0;
                if deckard_engine::sampler::envelope_ms(0.0, 0.0, mid).2 < wanted_ms {
                    low = mid;
                } else {
                    high = mid;
                }
            }
            doc.set(
                "Groove decay",
                target,
                "volume_envelope/decay",
                Value::Float((low + high) / 2.0),
            )?;
        }
    }
    if parameters & 4 != 0 {
        let tone = settings.tone.unwrap_or(0.0).clamp(-1.0, 1.0);
        let mode = if tone.abs() < 0.02 {
            0
        } else if tone < 0.0 {
            1
        } else {
            3
        };
        doc.set("Groove tone mode", target, "filter", Value::Int(mode))?;
        if mode != 0 {
            let hz = if tone < 0.0 {
                20_000.0 * (0.01f64).powf(-tone)
            } else {
                20.0 * 200f64.powf(tone)
            };
            let (mut low, mut high) = (0.0, 1.0);
            for _ in 0..32 {
                let mid = (low + high) / 2.0;
                if deckard_engine::sampler::cutoff_hz(mid, mode) < hz {
                    low = mid;
                } else {
                    high = mid;
                }
            }
            doc.set(
                "Groove tone",
                target,
                "cutoff",
                Value::Float((low + high) / 2.0),
            )?;
        }
    }
    Ok(())
}

fn d_set_legacy_zone(
    doc: &mut Document,
    group: usize,
    sound: usize,
    file: PathBuf,
    frames: usize,
) -> Result<(), String> {
    let frames = frames.saturating_sub(1) as f64;
    let base_key = doc.project().groups[group].sounds[sound]
        .channel
        .base_key
        .clamp(0, 127) as f64;
    doc.set_zones(
        group,
        sound,
        &[ZoneSpec {
            from: ZoneFrom::Template,
            file: Some(file),
            set: vec![
                ("play_range/high", frames),
                ("loop_range/high", frames),
                ("root_key", base_key),
            ],
        }],
        "Load Groove sound",
    )
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
    persist_frames(sample.len(), rate, |i| sample.frame(i))
}

fn persist_processed_sample(
    sample: &crate::sampler::Sample,
    rate: u32,
    pitch: f64,
    decay: f64,
    tone: f64,
) -> Result<(PathBuf, usize), String> {
    let speed = 2f64.powf(pitch.clamp(-12.0, 12.0) / 12.0);
    let frames = ((sample.len().saturating_sub(1) as f64 / speed).ceil() as usize).max(1);
    let decay = decay.clamp(0.0, 1.0);
    let fall = if decay >= 0.999 {
        1.0
    } else {
        (-1.0 / ((0.02 + decay * decay * 1.2) * rate as f64)).exp() as f32
    };
    let (tone, high) = crate::groove::tone_filter(tone, rate as f64);
    let mut env = 1.0f32;
    let mut z = [0.0f32; 2];
    let path = persist_frames(frames, rate, |i| {
        let pos = i as f64 * speed;
        let a = sample.frame(pos as usize);
        let b = sample.frame(pos as usize + 1);
        let t = pos.fract() as f32;
        let mut result = [0.0; 2];
        for channel in 0..2 {
            let x = (a[channel] + (b[channel] - a[channel]) * t) * env;
            z[channel] += tone * (x - z[channel]);
            result[channel] = if tone == 0.0 {
                x
            } else if high {
                x - z[channel]
            } else {
                z[channel]
            };
        }
        env *= fall;
        result
    })?;
    Ok((path, frames))
}

fn persist_frames(
    frames: usize,
    rate: u32,
    mut frame: impl FnMut(usize) -> [f32; 2],
) -> Result<PathBuf, String> {
    let length = frames
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
    for i in 0..frames {
        for v in frame(i) {
            wav.extend_from_slice(&((v * i16::MAX as f32).round() as i16).to_le_bytes());
        }
    }
    let root = legacy_samples_root()?;
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

fn legacy_samples_root() -> Result<PathBuf, String> {
    #[cfg(not(test))]
    let root = crate::paths::config_base()
        .ok_or("no configuration directory")?
        .join(crate::paths::app_folder_name())
        .join("maschine")
        .join("samples");
    #[cfg(test)]
    let root = std::env::temp_dir().join(format!("opendj-maschine-tests-{}", std::process::id()));
    Ok(root)
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

    #[test]
    fn groove_grid_edits_keep_unselected_notes_and_rotation_expression() {
        let mut doc = empty_project().unwrap();
        let expression = doc.add_expression(deckard_model::Expression {
            release_velocity: Some(9),
            ..Default::default()
        });
        let selected = Note {
            sound: 0,
            position: 0,
            length: 271,
            pitch: 53,
            velocity: 97,
            probability: 37,
            expr: Some(expression),
        };
        let off_grid = Note {
            position: 123,
            pitch: 65,
            ..selected
        };
        let other_sound = Note {
            sound: 12,
            ..selected
        };
        let other_group = Note {
            sound: 0,
            pitch: 71,
            ..selected
        };
        for note in [selected, off_grid, other_sound] {
            doc.add_note(0, 0, note).unwrap();
        }
        doc.add_note(1, 0, other_group).unwrap();
        let roles = roles_of(doc.project().groups.first());
        let group = &doc.project().groups[0];
        let pattern = &group.patterns[0];
        let before = tracks_of(pattern, roles).unwrap();
        let mut after = before;
        groove_dice::apply(
            Move::RotateRight,
            &mut after[0],
            0,
            0,
            &Habits::shared(),
            &mut groove_dice::Rng(1),
        );
        let (remove, add) = dice_notes(
            pattern,
            roles,
            [true, false, false, false, false, false, false, false],
            before,
            after,
            Move::RotateRight,
            group,
        )
        .unwrap();
        assert_eq!(remove, [selected]);
        assert_eq!(
            add,
            [Note {
                position: PPQ / 4,
                ..selected
            }]
        );
        doc.edit_notes("Dice", 0, 0, &remove, &add).unwrap();
        assert!(doc.project().groups[0].patterns[0]
            .notes
            .contains(&off_grid));
        assert!(doc.project().groups[0].patterns[0]
            .notes
            .contains(&other_sound));
        assert!(doc.project().groups[1].patterns[0]
            .notes
            .contains(&other_group));
        doc.edit_notes("Undo Dice", 0, 0, &add, &remove).unwrap();
        assert!(doc.project().groups[0].patterns[0]
            .notes
            .contains(&selected));

        let mut tracks = [Track::default(); crate::groove::ROWS];
        tracks[0].hits = 1;
        load_tracks(&mut doc, 0, 0, tracks, [true; crate::groove::ROWS]).unwrap();
        let notes = &doc.project().groups[0].patterns[0].notes;
        assert!(notes.contains(&off_grid));
        assert!(notes.contains(&other_sound));
        assert!(notes.iter().any(|n| n.sound == 0
            && n.position == 0
            && n.pitch == 53
            && n.length == 271
            && n.expr == Some(expression)));
        assert!(doc.project().groups[1].patterns[0]
            .notes
            .contains(&other_group));
    }

    #[test]
    fn groove_swing_maps_the_original_sixteenth_delay() {
        for value in [0.0, 0.1, 0.4, 0.75] {
            let swing = Swing {
                amount: groove_swing(value),
                cycle: 0,
                invert: false,
            };
            assert!((swing.apply(0.25) - (0.25 + value * 0.25)).abs() < 1e-9);
        }
    }

    #[test]
    fn legacy_step_delta_keeps_other_cells_and_note_properties() {
        let mut doc = empty_project().unwrap();
        let expression = doc.add_expression(deckard_model::Expression {
            release_velocity: Some(7),
            ..Default::default()
        });
        let note = Note {
            sound: 0,
            position: 0,
            length: 271,
            pitch: 53,
            velocity: 97,
            probability: 37,
            expr: Some(expression),
        };
        let other = Note { sound: 12, ..note };
        doc.add_note(0, 0, note).unwrap();
        doc.add_note(0, 0, other).unwrap();
        let before = Track::default();
        let after = Track {
            hits: 1 << 3,
            ..before
        };
        apply_track_delta(&mut doc, 0, 0, 0, before, after).unwrap();
        assert!(doc.project().groups[0].patterns[0].notes.contains(&note));
        let before = Track {
            hits: 1,
            ..Track::default()
        };
        let after = Track { maybe: 1, ..before };
        apply_track_delta(&mut doc, 0, 0, 0, before, after).unwrap();
        let pattern = &doc.project().groups[0].patterns[0];
        let got = pattern
            .notes
            .iter()
            .find(|n| n.sound == 0 && n.position == 0)
            .unwrap();
        assert_eq!(
            (
                got.pitch,
                got.velocity,
                got.length,
                got.expr,
                got.probability
            ),
            (53, 97, 271, Some(expression), 50)
        );
        assert!(pattern.notes.contains(&other));
        assert!(pattern
            .notes
            .iter()
            .any(|n| n.sound == 0 && n.position == 3 * PPQ / 4));
    }

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
    fn step_undo_and_redo_keep_editor_alive_and_preserve_take() {
        let (mut machine, handle, _) = fixture(109);
        handle.snapshot().unwrap();
        let first = Note {
            sound: 1,
            position: 120,
            length: 120,
            pitch: 61,
            velocity: 90,
            probability: 100,
            expr: None,
        };
        let second = Note {
            sound: 1,
            position: 360,
            pitch: 62,
            ..first
        };
        handle
            .edit(|d| {
                d.begin_take("Test take");
                Ok(())
            })
            .unwrap();
        machine.poll();
        handle.edit(move |d| d.add_note(0, 0, first)).unwrap();
        machine.poll();
        handle.edit(move |d| d.add_note(0, 0, second)).unwrap();
        machine.poll();
        assert!(handle.undo_step().unwrap());
        machine.poll();
        let undone = handle.snapshot().unwrap();
        assert!(undone.groups[0].patterns[0].notes.contains(&first));
        assert!(!undone.groups[0].patterns[0].notes.contains(&second));
        assert!(handle.redo_step().unwrap());
        machine.poll();
        let restored = handle.snapshot().unwrap();
        assert!(restored.groups[0].patterns[0].notes.contains(&first));
        assert!(restored.groups[0].patterns[0].notes.contains(&second));
        handle
            .edit(|d| d.rename(Target::Group(0), "Editor still alive"))
            .unwrap();
        assert_eq!(
            handle.snapshot().unwrap().groups[0].name,
            "Editor still alive"
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
                &[],
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
