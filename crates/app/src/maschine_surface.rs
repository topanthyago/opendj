//! The embedded DECKARD document shared by the screen and the MK2's Lua surface.
//! MIDI and LCD work keep running on their existing controller thread. Document
//! edits are serialized here, independently of Slint rendering.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use opendj_control::ControlRegistry;
use opendj_controllers::{maschine_surface as surface, MaschineProvider};
use opendj_engine::maschine::{
    level_db, level_from_db, note_tools, Chain, Color, Document, MaschineHandle, Note, Project,
    Sample, Target, Value, ZoneFrom, ZoneSpec,
};
use surface::{Action, ControlView, KnobView, View};

use crate::maschine_projects::{recent_name, recent_path, Projects};

/// Requests that need the app's editor, an existing panel or a native dialog.
#[derive(Clone, Debug)]
pub enum UiRequest {
    Instrument(usize),
    Sampling(usize),
    Browse(usize),
    PluginEditor(usize, Chain, usize),
    File(usize, &'static str),
    LoadProject(usize, PathBuf),
}

enum Command {
    Actions(usize, Vec<Action>, Vec<PathBuf>),
    Parameter(usize, usize, f64),
    Page(usize, i32),
    BrowserFocus(usize, usize, bool),
}

#[derive(Clone, Copy)]
struct Focus {
    group: usize,
    sound: usize,
    scene: usize,
    pattern: usize,
}
struct Queued {
    command: Command,
    focus: Focus,
}

pub struct MaschineSurfaceProvider {
    sender: mpsc::SyncSender<Queued>,
    views: Arc<RwLock<[Option<View>; 4]>>,
    errors: Arc<RwLock<[Option<String>; 4]>>,
    ui: Arc<Mutex<Vec<UiRequest>>>,
    registry: Arc<ControlRegistry>,
    projects: Arc<Projects>,
    // The GUI's polling must not replace a controller thread's displayed order.
    shown_projects: Mutex<HashMap<(ThreadId, usize), Vec<PathBuf>>>,
}

impl MaschineSurfaceProvider {
    pub fn new(registry: Arc<ControlRegistry>, handles: [MaschineHandle; 4]) -> Arc<Self> {
        let (sender, receiver) = mpsc::sync_channel(128);
        let views = Arc::new(RwLock::new(std::array::from_fn(|_| None)));
        let errors = Arc::new(RwLock::new(std::array::from_fn(|_| None)));
        let ui = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Self {
            sender,
            views: views.clone(),
            errors: errors.clone(),
            ui: ui.clone(),
            registry: registry.clone(),
            projects: Arc::new(Projects::new()),
            shown_projects: Mutex::new(HashMap::new()),
        });
        std::thread::Builder::new()
            .name("maschine-surface".into())
            .spawn(move || {
                let mut decks: [Deck; 4] = std::array::from_fn(|i| {
                    Deck::new(i + 1, registry.clone(), handles[i].clone(), ui.clone())
                });
                loop {
                    match receiver.recv_timeout(Duration::from_millis(25)) {
                        Ok(command) => process(command, &mut decks, &errors),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    for command in receiver.try_iter() {
                        process(command, &mut decks, &errors);
                    }
                    let mut next = std::array::from_fn(|_| None);
                    for (index, deck) in decks.iter_mut().enumerate() {
                        if deck.restart.is_some_and(|at| Instant::now() >= at) {
                            deck.set_channel("play", 1.0);
                            deck.restart = None;
                        }
                        next[index] = deck.view();
                    }
                    *views.write().unwrap() = next;
                }
            })
            .expect("start Maschine surface worker");
        provider
    }

    fn send(&self, command: Command) {
        let deck = match &command {
            Command::Actions(d, _, _)
            | Command::Parameter(d, _, _)
            | Command::Page(d, _)
            | Command::BrowserFocus(d, _, _) => *d,
        };
        let read = |key| {
            value(&self.registry, &format!("[Maschine{deck}]"), key)
                .round()
                .max(0.0) as usize
        };
        let focus = Focus {
            group: read("group").min(7),
            sound: read("sound").min(15),
            scene: read("scene"),
            pattern: read("pattern"),
        };
        if self.sender.try_send(Queued { command, focus }).is_err() && (1..=4).contains(&deck) {
            self.errors.write().unwrap()[deck - 1] = Some("Maschine edit queue is full".into());
        }
    }

    pub fn set_parameter(&self, deck: usize, index: usize, value: f64) {
        if value.is_finite() {
            self.send(Command::Parameter(deck, index, value.clamp(0.0, 1.0)));
        }
    }

    pub fn parameter_page_step(&self, deck: usize, by: i32) {
        self.send(Command::Page(deck, by));
    }

    pub fn browser_focus(&self, deck: usize, index: usize, load: bool) {
        self.send(Command::BrowserFocus(deck, index, load));
    }

    pub fn last_error(&self, deck: usize) -> Option<String> {
        self.errors
            .read()
            .ok()?
            .get(deck.checked_sub(1)?)?
            .clone()
            .or_else(|| self.projects.error())
    }

    pub fn report_result(&self, deck: usize, result: &Result<(), String>) {
        let mut errors = self.errors.write().unwrap();
        if let Some(error) = deck.checked_sub(1).and_then(|i| errors.get_mut(i)) {
            *error = result.as_ref().err().cloned();
        }
    }

    pub fn projects(&self) -> Arc<Projects> {
        self.projects.clone()
    }

    pub fn take_ui_requests(&self) -> Vec<UiRequest> {
        std::mem::take(&mut *self.ui.lock().unwrap())
    }
}

impl MaschineProvider for MaschineSurfaceProvider {
    fn view(&self, deck: usize) -> Option<View> {
        let index = deck.checked_sub(1)?;
        let mut view = self.views.read().ok()?.get(index)?.clone()?;
        let recent = self.projects.recent();
        view.recent = recent.iter().map(|p| recent_name(p)).collect();
        self.shown_projects
            .lock()
            .ok()?
            .insert((std::thread::current().id(), deck), recent);
        update_meters(&mut view, &self.registry, deck);
        Some(view)
    }

    fn dispatch(&self, deck: usize, actions: Vec<Action>) {
        let recent = self
            .shown_projects
            .lock()
            .ok()
            .and_then(|shown| shown.get(&(std::thread::current().id(), deck)).cloned())
            .unwrap_or_default();
        self.send(Command::Actions(deck, actions, recent));
    }

    fn contextual_wheel(
        &self,
        deck: usize,
        action: Action,
        group: usize,
        sound: Option<usize>,
    ) -> Action {
        let views = self.views.read().unwrap();
        let Some(view) = deck
            .checked_sub(1)
            .and_then(|d| views.get(d))
            .and_then(|v| v.as_ref())
        else {
            return action;
        };
        let mix = view.groups.get(group).and_then(|g| match sound {
            Some(s) => g.sounds.get(s).map(|s| &s.mix),
            None => Some(&g.mix),
        });
        let Some(mix) = mix else { return action };
        match action {
            Action::Volume(level) => Action::ChannelLevel {
                group,
                sound,
                db: level_db(
                    (level_from_db(mix.level) + level - view.master_volume).clamp(0.0, 1.0),
                ),
            },
            Action::Swing(amount) => Action::SetChannelSwing {
                group: Some(group),
                sound,
                amount: (mix.swing + amount - view.swing).clamp(0.0, 100.0),
            },
            Action::Tempo(bpm) => Action::SetTune {
                group,
                sound,
                semitones: mix.tune.unwrap_or(0.0) + bpm - view.tempo,
            },
            _ => action,
        }
    }

    fn error(&self, deck: usize) -> Option<String> {
        self.last_error(deck)
    }
}

fn process(queued: Queued, decks: &mut [Deck; 4], errors: &RwLock<[Option<String>; 4]>) {
    let Queued { command, focus } = queued;
    let deck = match &command {
        Command::Actions(d, _, _)
        | Command::Parameter(d, _, _)
        | Command::Page(d, _)
        | Command::BrowserFocus(d, _, _) => *d,
    };
    let Some(state) = deck.checked_sub(1).and_then(|index| decks.get_mut(index)) else {
        return;
    };
    state.focus = Some(focus);
    let result = match command {
        Command::Actions(_, actions, recent) => state.actions(actions, &recent),
        Command::Parameter(_, index, value) => state.set_parameter(index, value),
        Command::Page(_, by) => {
            state.page = (state.page as i32 + by).max(0) as usize;
            Ok(())
        }
        Command::BrowserFocus(_, index, load) => {
            let shown = state.browser.view().focus;
            state
                .action(Action::Browse(surface::Browse::Focus(
                    index as i64 - shown as i64,
                )))
                .and_then(|_| {
                    if load {
                        state.action(Action::Browse(surface::Browse::Load))
                    } else {
                        Ok(())
                    }
                })
        }
    };
    state.focus = None;
    errors.write().unwrap()[deck - 1] = result.err();
}

struct Deck {
    focus: Option<Focus>,
    deck: usize,
    registry: Arc<ControlRegistry>,
    handle: MaschineHandle,
    ui: Arc<Mutex<Vec<UiRequest>>>,
    level: usize,
    modules: bool,
    page: usize,
    slot: usize,
    follow: bool,
    fixed: bool,
    velocity: i64,
    grid: surface::GridView,
    selected: Vec<Note>,
    clipboard: Vec<Note>,
    solo_groups: Option<Vec<bool>>,
    solo_sounds: Vec<Option<Vec<bool>>>,
    sampling_tab: usize,
    sampling_zone: usize,
    sample_slices: Vec<usize>,
    slice_mode: usize,
    slice_count: usize,
    slice_grid: u32,
    slice_sensitivity: f64,
    sample_view: Option<(f64, f64)>,
    sample_data: Option<Arc<Sample>>,
    sample_overview: Vec<(f32, f32)>,
    sample_detail: Vec<(f32, f32)>,
    sample_wave_range: Option<(usize, usize)>,
    sampling: bool,
    song_view: bool,
    restart: Option<Instant>,
    last_group: usize,
    last_scene: usize,
    last_sound: usize,
    browser: crate::maschine_browser::MaschineBrowser,
}

impl Deck {
    fn actions(&mut self, actions: Vec<Action>, recent: &[PathBuf]) -> Result<(), String> {
        if let [Action::NewPatternAt { group, slot }, Action::CopyPattern {
            group: copy_group,
            from,
            to,
        }] = actions.as_slice()
        {
            if group == copy_group {
                let (group, slot, from, to, scene) = (*group, *slot, *from, *to, self.scene());
                let original = from - usize::from(to < from);
                self.edit(move |d| {
                    let name = d
                        .project()
                        .groups
                        .get(group)
                        .and_then(|g| g.patterns.get(original))
                        .ok_or("Pattern unavailable")?
                        .name
                        .clone();
                    let target = d.add_pattern_in_slot(group, original, slot, &name)?;
                    d.copy_pattern(group, original + usize::from(target <= original), target)?;
                    d.assign_pattern(scene, group, Some(target))
                })?;
                return Ok(());
            }
        }
        actions.into_iter().try_for_each(|action| {
            if let Action::File("load", index) = action {
                self.ui.lock().unwrap().push(UiRequest::LoadProject(
                    self.deck,
                    recent_path(recent, index)?,
                ));
                Ok(())
            } else {
                self.action(action)
            }
        })
    }

    fn new(
        deck: usize,
        registry: Arc<ControlRegistry>,
        handle: MaschineHandle,
        ui: Arc<Mutex<Vec<UiRequest>>>,
    ) -> Self {
        Self {
            focus: None,
            deck,
            registry,
            handle,
            ui,
            level: 2,
            modules: false,
            page: 0,
            slot: 0,
            follow: true,
            fixed: false,
            velocity: 100,
            grid: surface::GridView {
                step: 4,
                step_snap: true,
                step_ticks: 240,
                arrange_snap: true,
                ..Default::default()
            },
            selected: Vec::new(),
            clipboard: Vec::new(),
            solo_groups: None,
            solo_sounds: vec![None; 8],
            sampling_tab: 1,
            sampling_zone: 0,
            sample_slices: Vec::new(),
            slice_mode: 0,
            slice_count: 16,
            slice_grid: 16,
            slice_sensitivity: 0.5,
            sample_view: None,
            sample_data: None,
            sample_overview: Vec::new(),
            sample_detail: Vec::new(),
            sample_wave_range: None,
            sampling: false,
            song_view: false,
            restart: None,
            last_group: 0,
            last_scene: 0,
            last_sound: 0,
            browser: crate::maschine_browser::MaschineBrowser::new(),
        }
    }

    fn get(&self, key: &str) -> f64 {
        value(&self.registry, &format!("[Maschine{}]", self.deck), key)
    }
    fn set(&self, key: &str, value: f64) {
        set(
            &self.registry,
            &format!("[Maschine{}]", self.deck),
            key,
            value,
        );
    }
    fn set_channel(&self, key: &str, value: f64) {
        set(
            &self.registry,
            &format!("[Channel{}]", self.deck),
            key,
            value,
        );
    }
    fn group(&self) -> usize {
        if let Some(focus) = self.focus {
            return focus.group;
        }
        (self.get("group").round().max(0.0) as usize).min(7)
    }
    fn sound(&self) -> usize {
        if let Some(focus) = self.focus {
            return focus.sound;
        }
        (self.get("sound").round().max(0.0) as usize).min(15)
    }
    fn pattern(&self) -> usize {
        if let Some(focus) = self.focus {
            return focus.pattern;
        }
        self.get("pattern").round().max(0.0) as usize
    }
    fn scene(&self) -> usize {
        if let Some(focus) = self.focus {
            return focus.scene;
        }
        self.get("scene").round().max(0.0) as usize
    }

    fn project(&self) -> Option<Arc<Project>> {
        self.handle.snapshot_cached()
    }

    fn edit(
        &self,
        f: impl FnOnce(&mut Document) -> Result<(), String> + Send + 'static,
    ) -> Result<(), String> {
        self.handle.edit(f)
    }

    fn sync_focus(&mut self) {
        let (group, scene, sound) = (self.group(), self.scene(), self.sound());
        if group != self.last_group || sound != self.last_sound {
            self.sample_slices.clear();
            self.sample_view = None;
            self.sample_data = None;
            self.sample_wave_range = None;
            self.sampling_zone = 0;
            self.last_sound = sound;
        }
        if group != self.last_group || scene != self.last_scene {
            self.last_group = group;
            self.last_scene = scene;
            self.selected.clear();
            self.page = 0;
            self.slot = 0;
            self.sampling_zone = 0;
            self.sample_slices.clear();
            self.sample_view = None;
        }
    }

    fn view(&mut self) -> Option<View> {
        if value(&self.registry, &format!("[Groove{}]", self.deck), "backend") <= 0.0 {
            return None;
        }
        let project = self.project()?;
        self.sync_focus();
        let (group, sound, scene, pattern) =
            (self.group(), self.sound(), self.scene(), self.pattern());
        let playing = value(&self.registry, &format!("[Channel{}]", self.deck), "play") > 0.0;
        let position = playing.then_some(self.get("position").round() as i64);
        let mut view = View {
            project: project.name.clone(),
            tempo: self.get("tempo"),
            swing: project.swing.amount,
            master_volume: project.master_level,
            master_db: level_db(project.master_level),
            playing,
            recording: self.get("record") > 0.0,
            looping: true,
            metronome: self.get("metronome") > 0.0,
            follow: self.follow,
            fixed_velocity: self.fixed,
            velocity: self.velocity,
            grid: self.grid,
            group,
            sound,
            scene,
            pattern,
            position,
            numerator: project.numerator,
            denominator: project.denominator,
            song_view: self.song_view,
            scale_root: project
                .groups
                .get(group)
                .and_then(|g| g.sounds.get(sound))
                .map_or(60, |s| s.channel.base_key),
            keep_focus: true,
            ..Default::default()
        };
        view.scene_name = project
            .scenes
            .get(scene)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        view.scene_color = project
            .scenes
            .get(scene)
            .map_or(0, |s| color_index(s.color, scene * 2));
        view.scenes = project
            .scenes
            .iter()
            .enumerate()
            .map(|(index, scene)| surface::SceneView {
                name: scene.name.clone(),
                color: color_index(scene.color, index * 2),
                rgb: rgb(scene.color, index * 2),
                default_color: scene.color == Color::Default,
                patterns: (0..project.groups.len())
                    .map(|group| {
                        scene
                            .patterns
                            .iter()
                            .find(|(g, _)| *g == group)
                            .map(|(_, p)| *p)
                    })
                    .collect(),
            })
            .collect();
        view.playing_scene =
            (self.get("playing_scene") >= 0.0).then_some(self.get("playing_scene") as usize);
        view.queued_scene =
            (self.get("queued_scene") >= 0.0).then_some(self.get("queued_scene") as usize);
        view.sections = project
            .sections
            .iter()
            .enumerate()
            .map(|(index, section)| surface::SectionView {
                scene: section.scene,
                start: project.section_start(index),
                length: section.length,
                auto: section.auto,
            })
            .collect();
        view.groups = project
            .groups
            .iter()
            .enumerate()
            .map(|(gi, group)| {
                let group_color = color_index(group.color, gi * 2);
                surface::GroupView {
                    name: group.name.clone(),
                    color: group_color,
                    rgb: rgb(group.color, gi * 2),
                    muted: group.muted,
                    default_color: group.color == Color::Default,
                    mix: surface::MixView {
                        level: level_db(group.level),
                        pan: group.pan,
                        aux: group.aux,
                        swing: group.swing.amount,
                        tune: group
                            .sounds
                            .iter()
                            .find_map(|s| s.sampler.as_ref().map(|s| s.tune)),
                        ..Default::default()
                    },
                    scene_pattern: project
                        .scenes
                        .get(scene)
                        .and_then(|s| s.patterns.iter().find(|(g, _)| *g == gi).map(|(_, p)| *p)),
                    patterns: group
                        .patterns
                        .iter()
                        .map(|pattern| surface::PatternView {
                            name: pattern.name.clone(),
                            color: color_index(pattern.color, group_color),
                            rgb: rgb(pattern.color, group_color),
                            default_color: pattern.color == Color::Default,
                            bars: pattern.length as f64 / project.bar_length().max(1) as f64,
                            slot: pattern.slot,
                        })
                        .collect(),
                    sounds: group
                        .sounds
                        .iter()
                        .enumerate()
                        .map(|(si, sound)| surface::SoundView {
                            name: sound.name.clone(),
                            color: color_index(sound.color, group_color),
                            rgb: rgb(sound.color, group_color),
                            muted: sound.muted,
                            used: !sound.slots.is_empty()
                                || !sound.name.is_empty()
                                || group
                                    .patterns
                                    .iter()
                                    .any(|p| p.notes.iter().any(|n| n.sound == si)),
                            mix: surface::MixView {
                                level: level_db(sound.level),
                                pan: sound.pan,
                                aux: sound.aux,
                                swing: sound.swing.amount,
                                tune: sound.sampler.as_ref().map(|s| s.tune),
                                ..Default::default()
                            },
                            base_key: sound.channel.base_key,
                            choke_group: sound.channel.choke_group,
                            choke_mode: sound.channel.choke_mode,
                            link_group: sound.channel.link_group,
                            link_mode: sound.channel.link_mode,
                            default_color: sound.color == Color::Default,
                            touch_mode: sound.pad_touch.mode.index(),
                            touch_cc: i64::from(sound.pad_touch.cc),
                            touch_curve: i64::from(sound.pad_touch.curve),
                            touch_min: i64::from(sound.pad_touch.min),
                            touch_max: i64::from(sound.pad_touch.max),
                            prob_locked: sound.probability_locked,
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }
            })
            .collect();
        if let Some(pattern) = project
            .groups
            .get(group)
            .and_then(|g| g.patterns.get(pattern))
        {
            view.pattern_start = pattern.start;
            view.pattern_length = pattern.length.max(1);
            view.notes = pattern
                .notes
                .iter()
                .map(|note| surface::NoteView {
                    sound: note.sound,
                    position: note.position,
                    length: note.length,
                    pitch: note.pitch,
                    velocity: note.velocity,
                    probability: note.probability,
                    selected: self.selected.contains(note),
                })
                .collect();
            let assigned = project.scenes.get(scene).and_then(|s| {
                s.patterns
                    .iter()
                    .find(|(g, _)| *g == group)
                    .map(|(_, p)| *p)
            });
            view.pattern_position =
                if view.playing_scene == Some(scene) && assigned == Some(self.pattern()) {
                    position.map(|p| pattern.start + p.rem_euclid(pattern.length.max(1)))
                } else {
                    None
                };
            view.progress = view
                .pattern_position
                .map(|p| (p - pattern.start) as f64 / pattern.length.max(1) as f64);
        } else {
            view.pattern_length = project.bar_length().max(1);
        }
        view.control = self.control(&project);
        view.browser = self.browser.view();
        view.note_repeat.active = self.get("note_repeat") > 0.0;
        view.note_repeat.settings = vec![surface::RepeatSetting {
            key: "rate".into(),
            name: "RATE".into(),
            text: format!("{} ticks", self.get("repeat_rate").round() as i64),
            value: self.get("repeat_rate"),
            min: 15.0,
            max: 3840.0,
            step: 15.0,
            turn: 1.0 / 127.0,
        }];
        view.note_repeat.presets = ["1/8", "1/16", "1/16T", "1/32"]
            .iter()
            .map(|s| (*s).into())
            .collect();
        view.sampling.tab = self.sampling_tab;
        if self.sampling {
            self.sampling_view(&project, &mut view);
        }
        Some(view)
    }

    fn parameters(&self, project: &Project) -> Vec<Parameter> {
        let group = self.group();
        let sound = self.sound();
        let Some(grp) = project.groups.get(group) else {
            return Vec::new();
        };
        if !self.modules {
            let (target, level, pan, swing, aux) = match self.level {
                0 => (
                    Target::Song,
                    project.master_level,
                    project.master_pan,
                    project.swing.amount,
                    [0.0; 2],
                ),
                1 => (
                    Target::GroupStrip(group),
                    grp.level,
                    grp.pan,
                    grp.swing.amount,
                    grp.aux,
                ),
                _ => match grp.sounds.get(sound) {
                    Some(s) => (
                        Target::Sound(group, sound),
                        s.level,
                        s.pan,
                        s.swing.amount,
                        s.aux,
                    ),
                    None => return Vec::new(),
                },
            };
            let swing_target = match self.level {
                0 => Target::Song,
                1 => Target::Group(group),
                _ => Target::Sound(group, sound),
            };
            let mut params = vec![
                Parameter::new(target, "level", "LEVEL", level, 0.0, 1.0),
                Parameter::new(target, "pan", "PAN", pan, -1.0, 1.0),
                Parameter::new(swing_target, "swing_amount", "SWING", swing, 0.0, 100.0),
            ];
            if self.level > 0 {
                params.push(Parameter::new(
                    target,
                    "aux1_level",
                    "AUX 1",
                    aux[0],
                    0.0,
                    1.0,
                ));
                params.push(Parameter::new(
                    target,
                    "aux2_level",
                    "AUX 2",
                    aux[1],
                    0.0,
                    1.0,
                ));
            }
            return params;
        }
        let (chain, slots) = self.chain(project);
        let Some(slot) = slots.get(self.slot) else {
            return Vec::new();
        };
        let target = Target::ChainModule(chain, self.slot);
        slot.params
            .iter()
            .filter_map(|(field, value)| {
                parameter_range(&slot.module, field).map(|(min, max)| {
                    Parameter::new(
                        target,
                        field,
                        &field.replace('_', " ").replace('/', " ").to_uppercase(),
                        *value,
                        min,
                        max,
                    )
                })
            })
            .collect()
    }

    fn chain<'a>(&self, project: &'a Project) -> (Chain, &'a [opendj_engine::maschine::SlotInfo]) {
        match self.level {
            0 => (Chain::Master, &project.master_slots),
            1 => (
                Chain::Group(self.group()),
                project
                    .groups
                    .get(self.group())
                    .map_or(&[], |g| g.slots.as_slice()),
            ),
            _ => (
                Chain::Sound(self.group(), self.sound()),
                project
                    .groups
                    .get(self.group())
                    .and_then(|g| g.sounds.get(self.sound()))
                    .map_or(&[], |s| s.slots.as_slice()),
            ),
        }
    }

    fn control(&mut self, project: &Project) -> ControlView {
        let parameters = self.parameters(project);
        let pages = parameters.len().div_ceil(8).max(1);
        self.page = self.page.min(pages - 1);
        let (_, slots) = self.chain(project);
        let slot = slots.get(self.slot);
        ControlView {
            level: self.level,
            modules_visible: self.modules,
            slot: slot.map(|s| s.name.clone()),
            slot_count: slots.len(),
            slot_index: (!slots.is_empty()).then_some(self.slot.min(slots.len() - 1)),
            chain: slots
                .iter()
                .map(|slot| surface::SlotView {
                    name: slot.name.clone(),
                    instrument: slot.module == "Sampler" || slot.module.ends_with("Synth"),
                    active: slot.active,
                    plugin: slot.plugin.is_some(),
                    picture: None,
                })
                .collect(),
            pages: (0..pages)
                .map(|page| {
                    format!(
                        "{} {}",
                        if self.modules {
                            "PARAMETERS"
                        } else {
                            "CHANNEL"
                        },
                        page + 1
                    )
                })
                .collect(),
            page: self.page,
            knobs: (0..8)
                .map(|index| parameters.get(self.page * 8 + index).map(Parameter::view))
                .collect(),
            modules: if self.level == 2 {
                crate::maschine_ui::NATIVE_INSTRUMENTS
                    .iter()
                    .map(|(name, _)| surface::ModuleView {
                        name: (*name).into(),
                        instrument: true,
                        format: "Internal".into(),
                        vendor: "DECKARD".into(),
                    })
                    .collect()
            } else {
                Vec::new()
            },
            ..Default::default()
        }
    }

    fn set_parameter(&mut self, index: usize, normalized: f64) -> Result<(), String> {
        let project = self.project().ok_or("Maschine document unavailable")?;
        let parameters = self.parameters(&project);
        let Some(parameter) = parameters.get(self.page * 8 + index).cloned() else {
            return Ok(());
        };
        let value = parameter.min + normalized.clamp(0.0, 1.0) * (parameter.max - parameter.min);
        self.edit(move |d| parameter.set(d, value))
    }

    fn turn(&mut self, index: usize, increment: f64, fine: bool) -> Result<(), String> {
        let project = self.project().ok_or("Maschine document unavailable")?;
        let parameters = self.parameters(&project);
        let Some(parameter) = parameters.get(self.page * 8 + index) else {
            return Ok(());
        };
        self.set_parameter(
            index,
            parameter.normalized() + increment * if fine { 0.1 } else { 1.0 },
        )
    }

    fn action(&mut self, action: Action) -> Result<(), String> {
        let (group, sound, pattern, scene) =
            (self.group(), self.sound(), self.pattern(), self.scene());
        match action {
            Action::Play => self.set_channel("play", 1.0),
            Action::Stop => self.set_channel("play", 0.0),
            Action::Restart => {
                self.set_channel("play", 0.0);
                self.restart = Some(Instant::now() + Duration::from_millis(30));
            }
            Action::Record { on, count_in } => {
                self.set("record", if on { 1.0 } else { 0.0 });
                if on && count_in {
                    self.set_channel("play", 1.0);
                    self.set("count_in", 1.0);
                    self.set("count_in", 0.0);
                }
            }
            Action::Metronome(on) => self.set("metronome", if on { 1.0 } else { 0.0 }),
            Action::Tempo(bpm) => {
                set(
                    &self.registry,
                    &format!("[Groove{}]", self.deck),
                    "bpm",
                    bpm.clamp(10.0, 400.0),
                );
                self.edit(move |d| d.set_tempo(bpm))?;
            }
            Action::Swing(amount) => self.edit(move |d| {
                let mut swing = d.project().swing;
                swing.amount = amount;
                d.set_swing(Target::Song, swing)
            })?,
            Action::Volume(value) => self.edit(move |d| d.set_level(Target::Song, value))?,
            Action::VolumeDb(db) => {
                self.edit(move |d| d.set_level(Target::Song, level_from_db(db)))?
            }
            Action::FocusGroup(index) if index < 8 => self.set("group", index as f64),
            Action::FocusSound(index) if index < 16 => self.set("sound", index as f64),
            Action::FocusScene(index) | Action::LaunchScene(index) => {
                self.set("scene", index as f64)
            }
            Action::PickPattern(index) => {
                let current_group = self.get("group").round() as usize;
                if current_group == group {
                    self.handle.select_pattern(scene, group, Some(index))?;
                } else {
                    self.edit(move |d| d.assign_pattern(scene, group, Some(index)))?;
                }
                self.selected.clear();
            }
            Action::MuteGroup(index) => {
                let p = self.project().ok_or("Maschine document unavailable")?;
                let muted = !p.groups.get(index).ok_or("Group unavailable")?.muted;
                self.edit(move |d| d.set_mute(Target::Group(index), muted))?;
            }
            Action::MuteSound(index) => {
                let p = self.project().ok_or("Maschine document unavailable")?;
                let muted = !p
                    .groups
                    .get(group)
                    .and_then(|g| g.sounds.get(index))
                    .ok_or("Sound unavailable")?
                    .muted;
                self.edit(move |d| d.set_mute(Target::Sound(group, index), muted))?;
            }
            Action::SoloGroup(index) => self.solo(None, index)?,
            Action::SoloSound(index) => self.solo(Some(group), index)?,
            Action::EraseSound(index) => self.edit(move |d| {
                let notes = d
                    .project()
                    .groups
                    .get(group)
                    .and_then(|g| g.patterns.get(pattern))
                    .ok_or("Pattern unavailable")?
                    .notes
                    .iter()
                    .copied()
                    .filter(|n| n.sound == index)
                    .collect::<Vec<_>>();
                d.delete_notes(group, pattern, &notes)
            })?,
            Action::ResetSound(index) => self.edit(move |d| d.reset_sound(group, index))?,
            Action::CopySound { from, to } => self.edit(move |d| {
                let clip = d.copy_sound(group, from)?;
                d.paste_sounds(group, to, &[clip])
            })?,
            Action::CopyGroup { from, to } => self.edit(move |d| {
                if from == to {
                    return Ok(());
                }
                let bytes = d.file_bytes(
                    opendj_engine::maschine::FileItem::Group(from),
                    &opendj_engine::maschine::SoundInfo::default(),
                )?;
                d.load_group_file(to, &bytes)
            })?,
            Action::Turn { knob, inc, fine } => self.turn(knob, inc, fine)?,
            Action::Page(next) => {
                self.page = if next {
                    self.page + 1
                } else {
                    self.page.saturating_sub(1)
                }
            }
            Action::Level(level) => {
                self.level = level.min(2);
                self.page = 0;
                self.slot = 0;
            }
            Action::Slot(by) => {
                self.slot = (self.slot as i64 + by).max(0) as usize;
                self.page = 0;
            }
            Action::ModulesVisible(on) => {
                self.modules = on;
                self.page = 0;
            }
            Action::LoadModule(Some(index)) => {
                let class = crate::maschine_ui::NATIVE_INSTRUMENTS
                    .get(index)
                    .ok_or("Instrument unavailable")?
                    .1
                    .to_string();
                self.edit(move |d| crate::maschine_ui::load_native(d, group, sound, &class))?;
                self.level = 2;
                self.modules = true;
                self.page = 0;
                self.slot = 0;
            }
            Action::LoadModule(None) => {
                let project = self.project().ok_or("Maschine document unavailable")?;
                let (chain, _) = self.chain(&project);
                let index = self.slot;
                self.edit(move |d| d.remove_slot_in(chain, index))?;
            }
            Action::FixedVelocity(on) => self.fixed = on,
            Action::VelocityValue(value) => self.velocity = value.clamp(1, 127),
            Action::KeyboardMode(on) => self.set("keyboard", if on { 1.0 } else { 0.0 }),
            Action::Scale { root, .. } => {
                let root = root.clamp(0, 127);
                self.set("octave", ((root - 60) / 12) as f64);
                self.set("semitone", ((root - 60) % 12) as f64);
                self.edit(move |d| {
                    d.set(
                        "Base key",
                        Target::Sound(group, sound),
                        "base_key",
                        Value::Int(root),
                    )
                })?;
            }
            Action::SoundSetting(field, value) => self.edit(move |d| {
                d.set(
                    "Pad mode",
                    Target::Sound(group, sound),
                    field,
                    Value::Int(value),
                )
            })?,
            Action::ChannelLevel { group, sound, db } => self.edit(move |d| {
                d.set_level(
                    sound.map_or(Target::Group(group), |s| Target::Sound(group, s)),
                    level_from_db(db),
                )
            })?,
            Action::ChannelPan { group, sound, pan } => self.edit(move |d| {
                d.set_pan(
                    sound.map_or(Target::Group(group), |s| Target::Sound(group, s)),
                    pan,
                )
            })?,
            Action::SetChannelSwing {
                group,
                sound,
                amount,
            } => self.edit(move |d| {
                let (target, mut swing) = match (group, sound) {
                    (Some(g), Some(s)) => (
                        Target::Sound(g, s),
                        d.project()
                            .groups
                            .get(g)
                            .and_then(|g| g.sounds.get(s))
                            .ok_or("Sound unavailable")?
                            .swing,
                    ),
                    (Some(g), None) => (
                        Target::Group(g),
                        d.project().groups.get(g).ok_or("Group unavailable")?.swing,
                    ),
                    _ => (Target::Song, d.project().swing),
                };
                swing.amount = amount.clamp(0.0, 100.0);
                d.set_swing(target, swing)
            })?,
            Action::SetTune {
                group,
                sound,
                semitones,
            } => self.edit(move |d| {
                let grp = d.project().groups.get(group).ok_or("Group unavailable")?;
                let tunes: Vec<_> = grp
                    .sounds
                    .iter()
                    .enumerate()
                    .filter(|(s, _)| sound.is_none_or(|selected| selected == *s))
                    .filter_map(|(s, sound)| {
                        sound.sampler.as_ref().map(|sampler| (s, sampler.tune))
                    })
                    .collect();
                let first = tunes
                    .first()
                    .ok_or("The selected channel has no Sampler tune parameter")?
                    .1;
                for (sound, tune) in tunes {
                    d.set(
                        "Tune",
                        Target::Sampler(group, sound),
                        "tune",
                        Value::Float((tune + semitones - first).clamp(-36.0, 36.0)),
                    )?;
                }
                Ok(())
            })?,
            Action::Follow(on) => self.follow = on,
            Action::Grid(key, value) => self.set_grid(key, value),
            Action::Setting("quantize", _, _) => self.set(
                "quantize",
                if self.get("quantize") > 0.0 { 0.0 } else { 1.0 },
            ),
            Action::PatternSpan { start, length } => {
                self.edit(move |d| d.set_pattern_span(group, pattern, start, length))?
            }
            Action::PatternLength(length) => {
                self.edit(move |d| d.set_pattern_length(group, pattern, length))?
            }
            Action::StepToggle {
                at,
                length,
                velocity,
            } => {
                self.edit(move |d| {
                    let pitch = d
                        .project()
                        .groups
                        .get(group)
                        .and_then(|g| g.sounds.get(sound))
                        .ok_or("Sound unavailable")?
                        .channel
                        .base_key;
                    let p = d
                        .project()
                        .groups
                        .get(group)
                        .and_then(|g| g.patterns.get(pattern))
                        .ok_or("Pattern unavailable")?;
                    let notes: Vec<Note> = p
                        .notes
                        .iter()
                        .copied()
                        .filter(|n| {
                            n.sound == sound && n.position >= at && n.position < at + length
                        })
                        .collect();
                    if notes.is_empty() {
                        d.add_note(
                            group,
                            pattern,
                            Note {
                                expr: None,
                                sound,
                                position: at,
                                length,
                                pitch,
                                velocity: velocity.clamp(1, 127),
                                probability: 100,
                            },
                        )
                    } else {
                        d.delete_notes(group, pattern, &notes)
                    }
                })?;
            }
            Action::StepNotes {
                at,
                length,
                velocity,
                pitches,
            } => self.edit(move |d| {
                let p = d
                    .project()
                    .groups
                    .get(group)
                    .and_then(|g| g.patterns.get(pattern))
                    .ok_or("Pattern unavailable")?;
                let notes: Vec<Note> = p
                    .notes
                    .iter()
                    .copied()
                    .filter(|n| {
                        n.sound == sound
                            && n.position >= at
                            && n.position < at + length
                            && pitches.contains(&n.pitch)
                    })
                    .collect();
                let pitches: Vec<i64> = pitches
                    .into_iter()
                    .filter(|p| (0..=127).contains(p))
                    .collect();
                let add: Vec<Note> = if pitches
                    .iter()
                    .all(|pitch| notes.iter().any(|n| n.pitch == *pitch))
                {
                    Vec::new()
                } else {
                    pitches
                        .iter()
                        .map(|pitch| Note {
                            expr: None,
                            sound,
                            position: at,
                            length,
                            pitch: *pitch,
                            velocity: velocity.clamp(1, 127),
                            probability: 100,
                        })
                        .collect()
                };
                d.edit_notes("Step notes", group, pattern, &notes, &add)
            })?,
            Action::Edit(command) => self.edit_command(command)?,
            Action::Events(command, amount) => self.events(command, amount)?,
            Action::EventsRange {
                start,
                end,
                low,
                high,
            } => {
                let p = self.project().ok_or("Maschine document unavailable")?;
                self.selected = p
                    .groups
                    .get(group)
                    .and_then(|g| g.patterns.get(pattern))
                    .map(|p| {
                        p.notes
                            .iter()
                            .copied()
                            .filter(|n| {
                                n.sound == sound
                                    && n.position >= start
                                    && n.position < end
                                    && n.pitch >= low
                                    && n.pitch <= high
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
            Action::PatternEdit(command) => self.pattern_edit(command)?,
            Action::NewPatternIn(index) => self.new_pattern(index, None)?,
            Action::NewPatternAt { group, slot } => self.new_pattern(group, Some(slot))?,
            Action::ScenePattern { group, pattern } => {
                self.edit(move |d| d.assign_pattern(scene, group, pattern))?
            }
            Action::DeletePattern { group, pattern } => {
                self.edit(move |d| d.delete_pattern(group, pattern))?
            }
            Action::CopyPattern { group, from, to } => {
                self.edit(move |d| d.copy_pattern(group, from, to).map(|_| ()))?
            }
            Action::SceneEdit(command) => self.scene_edit(command)?,
            Action::DeleteScene(index) => self.edit(move |d| d.delete_scene(index))?,
            Action::CopyScene { from, to } => {
                self.edit(move |d| d.copy_scene(from, to).map(|_| ()))?
            }
            Action::Color { of, color } => {
                let target = match of {
                    surface::ColorOf::Pattern => Target::Pattern(group, pattern),
                    surface::ColorOf::Scene => Target::Scene(scene),
                    surface::ColorOf::Group => Target::Group(group),
                    surface::ColorOf::Sound => Target::Sound(group, sound),
                };
                self.edit(move |d| {
                    d.set_color(&[target], color.map_or(Color::Default, Color::Palette))
                })?;
            }
            Action::Repeat(key, value) => match key {
                "on" | "locked" => self.set("note_repeat", value),
                "rate" => self.set("repeat_rate", value),
                "preset" => self.set(
                    "repeat_rate",
                    [480.0, 240.0, 160.0, 120.0][(value as usize).saturating_sub(1).min(3)],
                ),
                _ => return Err(format!("Note Repeat setting {key} is unavailable")),
            },
            Action::ChokeAll => self.set("choke", 1.0),
            Action::Sampling(on) => {
                self.sampling = on;
                if on {
                    self.ui.lock().unwrap().push(UiRequest::Sampling(self.deck));
                } else {
                    self.handle.stop_preview()?;
                }
            }
            Action::SamplingTab(tab) => self.sampling_tab = tab.min(3),
            Action::SamplingCall(call) => self.sample_call(call)?,
            Action::SamplingKnob { knob, value, .. } => self.sample_knob(knob, value)?,
            Action::RecordSample(press) => {
                let busy =
                    self.get("sampling_recording") > 0.0 || self.get("sampling_waiting") > 0.0;
                let key = if press == surface::RecordPress::Cancel || busy {
                    "sampling_cancel"
                } else {
                    "sampling_capture"
                };
                self.set(key, 1.0);
            }
            Action::SongView(on) => self.song_view = on,
            Action::Library(open) => {
                if open {
                    self.ui.lock().unwrap().push(UiRequest::Browse(self.deck));
                }
            }
            Action::Browse(action) => self.browser.action(action, &self.handle, group, sound)?,
            Action::File(command, _) => self
                .ui
                .lock()
                .unwrap()
                .push(UiRequest::File(self.deck, command)),
            Action::SlotEdit("edit") => {
                let project = self.project().ok_or("Maschine document unavailable")?;
                let (chain, _) = self.chain(&project);
                self.ui
                    .lock()
                    .unwrap()
                    .push(UiRequest::PluginEditor(self.deck, chain, self.slot));
            }
            Action::SlotEdit("add") => self
                .ui
                .lock()
                .unwrap()
                .push(UiRequest::Instrument(self.deck)),
            Action::SlotEdit("bypass") => {
                let p = self.project().ok_or("Maschine document unavailable")?;
                let (chain, slots) = self.chain(&p);
                let index = self.slot;
                let active = !slots.get(index).ok_or("Plug-in slot unavailable")?.active;
                self.edit(move |d| d.set_slot_active_in(chain, index, active))?;
            }
            Action::SlotEdit("remove") => {
                let p = self.project().ok_or("Maschine document unavailable")?;
                let (chain, _) = self.chain(&p);
                let index = self.slot;
                self.edit(move |d| d.remove_slot_in(chain, index))?;
            }
            Action::ShowScenes(_) | Action::StepModAt(_) | Action::ModSlotsClosed => {}
            other => return Err(format!("Embedded Maschine action unavailable: {other:?}")),
        }
        Ok(())
    }

    fn solo(&mut self, group: Option<usize>, index: usize) -> Result<(), String> {
        let p = self.project().ok_or("Maschine document unavailable")?;
        let current: Vec<bool> = match group {
            Some(group) => p
                .groups
                .get(group)
                .ok_or("Group unavailable")?
                .sounds
                .iter()
                .map(|s| s.muted)
                .collect(),
            None => p.groups.iter().map(|g| g.muted).collect(),
        };
        if index >= current.len() {
            return Err("Solo target unavailable".into());
        }
        let saved = match group {
            Some(group) => &mut self.solo_sounds[group],
            None => &mut self.solo_groups,
        };
        let already = current
            .iter()
            .enumerate()
            .all(|(i, muted)| *muted == (i != index));
        let count = current.len();
        let target = if already && saved.is_some() {
            saved.take().unwrap()
        } else {
            if saved.is_none() {
                *saved = Some(current);
            }
            (0..count).map(|i| i != index).collect()
        };
        let mutes: Vec<_> = target
            .iter()
            .enumerate()
            .map(|(i, muted)| {
                (
                    group.map_or(Target::Group(i), |g| Target::Sound(g, i)),
                    *muted,
                )
            })
            .collect();
        self.edit(move |d| d.set_mutes("Solo", &mutes))
    }

    fn set_grid(&mut self, key: &str, value: i64) {
        match key {
            "step" => {
                self.grid.step = value.clamp(0, 15);
                self.grid.step_ticks = grid_ticks(self.grid.step);
                self.set("grid", self.grid.step_ticks as f64);
            }
            "step_snap" => self.grid.step_snap = value > 0,
            "perform" => self.grid.perform = value,
            "arrange" => self.grid.arrange = value,
            "arrange_snap" => self.grid.arrange_snap = value > 0,
            "nudge" => self.grid.nudge = value.clamp(0, 4),
            "quick" => self.grid.quick = value > 0,
            _ => {}
        }
    }

    fn focused_notes(&self) -> Result<(Arc<Project>, Vec<Note>), String> {
        let project = self.project().ok_or("Maschine document unavailable")?;
        let pattern = project
            .groups
            .get(self.group())
            .and_then(|g| g.patterns.get(self.pattern()))
            .ok_or("Pattern unavailable")?;
        let notes = if self.selected.is_empty() {
            pattern
                .notes
                .iter()
                .copied()
                .filter(|n| n.sound == self.sound())
                .collect()
        } else {
            pattern
                .notes
                .iter()
                .copied()
                .filter(|n| self.selected.contains(n))
                .collect()
        };
        Ok((project, notes))
    }

    fn edit_command(&mut self, command: &str) -> Result<(), String> {
        match command {
            "undo" => {
                self.handle.undo()?;
                self.selected.clear();
            }
            "redo" => {
                self.handle.redo()?;
                self.selected.clear();
            }
            "undo_step" => {
                self.handle.undo_step()?;
                self.selected.clear();
            }
            "redo_step" => {
                self.handle.redo_step()?;
                self.selected.clear();
            }
            "copy" => self.clipboard = self.focused_notes()?.1,
            "paste" => {
                let (g, s, p) = (self.group(), self.sound(), self.pattern());
                let notes: Vec<_> = self
                    .clipboard
                    .iter()
                    .map(|n| Note { sound: s, ..*n })
                    .collect();
                self.edit(move |d| d.edit_notes("Paste notes", g, p, &[], &notes))?;
            }
            "clear_modulation" => {
                let (g, p) = (self.group(), self.pattern());
                self.edit(move |d| d.clear_pattern_modulation(Chain::Group(g), p))?;
            }
            _ => return Err(format!("Edit {command} unavailable")),
        }
        Ok(())
    }

    fn events(&mut self, command: &str, amount: i64) -> Result<(), String> {
        if matches!(
            command,
            "erase_step" | "erase_sound_window" | "erase_group_window"
        ) {
            let project = self.project().ok_or("Maschine document unavailable")?;
            let (group, sound, pattern, at) = if command == "erase_step" {
                (self.group(), Some(self.sound()), self.pattern(), amount)
            } else {
                let group = if command == "erase_sound_window" {
                    amount.max(0) as usize / 16
                } else {
                    amount.max(0) as usize
                };
                let sound =
                    (command == "erase_sound_window").then_some(amount.max(0) as usize % 16);
                let scene = self.get("playing_scene").max(0.0) as usize;
                let Some(pattern) = project.scenes.get(scene).and_then(|s| {
                    s.patterns
                        .iter()
                        .find(|(g, _)| *g == group)
                        .map(|(_, p)| *p)
                }) else {
                    return Ok(());
                };
                let Some(p) = project
                    .groups
                    .get(group)
                    .and_then(|g| g.patterns.get(pattern))
                else {
                    return Ok(());
                };
                let at = p.start + (self.get("position") as i64).rem_euclid(p.length.max(1));
                (
                    group,
                    sound,
                    pattern,
                    at - (at - p.start).rem_euclid(self.grid.step_ticks.max(1)),
                )
            };
            let Some(p) = project
                .groups
                .get(group)
                .and_then(|g| g.patterns.get(pattern))
            else {
                return Ok(());
            };
            let end = at + self.grid.step_ticks.max(1);
            let notes: Vec<_> = p
                .notes
                .iter()
                .copied()
                .filter(|n| {
                    sound.is_none_or(|sound| n.sound == sound)
                        && n.position >= at
                        && n.position < end
                })
                .collect();
            if notes.is_empty() {
                return Ok(());
            }
            return self.edit(move |d| d.delete_notes(group, pattern, &notes));
        }
        let (project, notes) = self.focused_notes()?;
        let (group, pattern, sound) = (self.group(), self.pattern(), self.sound());
        let focused = project
            .groups
            .get(group)
            .and_then(|g| g.patterns.get(pattern))
            .ok_or("Pattern unavailable")?;
        match command {
            "select_none" => {
                self.selected.clear();
                return Ok(());
            }
            "select_all" => {
                self.selected = focused.notes.clone();
                return Ok(());
            }
            "select_sound" => {
                self.selected = focused
                    .notes
                    .iter()
                    .copied()
                    .filter(|n| n.sound == amount as usize)
                    .collect();
                return Ok(());
            }
            "select_sound_toggle" => {
                let notes: Vec<_> = focused
                    .notes
                    .iter()
                    .copied()
                    .filter(|n| n.sound == amount as usize)
                    .collect();
                if notes.iter().all(|n| self.selected.contains(n)) {
                    self.selected.retain(|n| !notes.contains(n));
                } else {
                    self.selected.extend(
                        notes
                            .into_iter()
                            .filter(|n| !self.selected.contains(n))
                            .collect::<Vec<_>>(),
                    );
                }
                return Ok(());
            }
            "select_pitch" => {
                self.selected = focused
                    .notes
                    .iter()
                    .copied()
                    .filter(|n| n.sound == sound && n.pitch == amount)
                    .collect();
                return Ok(());
            }
            _ => {}
        }
        let (gone, made) = match command {
            "clear_all" => (
                if amount > 0 {
                    focused
                        .notes
                        .iter()
                        .copied()
                        .filter(|n| n.sound == sound)
                        .collect()
                } else {
                    focused.notes.clone()
                },
                Vec::new(),
            ),
            "erase_pitch" => (
                focused
                    .notes
                    .iter()
                    .copied()
                    .filter(|n| n.sound == sound && n.pitch == amount)
                    .collect(),
                Vec::new(),
            ),
            "nudge" | "nudge_shown" | "nudge_fine" | "nudge_shown_fine" => {
                let step = if command.ends_with("fine") {
                    1
                } else {
                    (self.grid.step_ticks / (1 << self.grid.nudge)).max(1)
                };
                note_tools::nudge(
                    &notes,
                    amount * step,
                    focused.start,
                    focused.start + focused.length,
                )
            }
            "quantize" | "quantize_half" => {
                let step = self.grid.step_ticks.max(1);
                let made = notes
                    .iter()
                    .map(|n| {
                        let snapped = ((n.position - focused.start) as f64 / step as f64).round()
                            as i64
                            * step
                            + focused.start;
                        Note {
                            position: if command == "quantize_half" {
                                (n.position + snapped) / 2
                            } else {
                                snapped
                            },
                            ..*n
                        }
                    })
                    .collect();
                (notes, made)
            }
            "transpose" | "pitch" => {
                let made = notes
                    .iter()
                    .map(|n| Note {
                        pitch: (n.pitch + amount).clamp(0, 127),
                        ..*n
                    })
                    .collect();
                (notes, made)
            }
            "velocity" => {
                let made = notes
                    .iter()
                    .map(|n| Note {
                        velocity: (n.velocity + amount).clamp(1, 127),
                        ..*n
                    })
                    .collect();
                (notes, made)
            }
            "length" | "length_fine" => {
                let step = if command.ends_with("fine") {
                    1
                } else {
                    self.grid.step_ticks
                };
                let made = notes
                    .iter()
                    .map(|n| Note {
                        length: (n.length + amount * step).max(1),
                        ..*n
                    })
                    .collect();
                (notes, made)
            }
            "probability" => {
                let made = notes
                    .iter()
                    .map(|n| Note {
                        probability: (n.probability + amount).clamp(0, 100),
                        ..*n
                    })
                    .collect();
                (notes, made)
            }
            _ => return Err(format!("Events action {command} unavailable")),
        };
        self.selected = made.clone();
        self.edit(move |d| d.edit_notes("Edit events", group, pattern, &gone, &made))
    }

    fn new_pattern(&mut self, group: usize, slot: Option<usize>) -> Result<(), String> {
        let scene = self.scene();
        self.edit(move |d| {
            let index = match slot {
                Some(slot) => {
                    d.add_pattern_in_slot(group, 0, slot, &format!("Pattern {}", slot + 1))?
                }
                None => d.add_pattern(group, 0, "Pattern")?,
            };
            d.clear_pattern(group, index)?;
            d.assign_pattern(scene, group, Some(index))
        })?;
        if let Some(p) = self.project().and_then(|p| {
            p.scenes.get(scene).and_then(|s| {
                s.patterns
                    .iter()
                    .find(|(g, _)| *g == group)
                    .map(|(_, p)| *p)
            })
        }) {
            if group == self.get("group").round() as usize {
                self.set("pattern", p as f64);
            }
        }
        Ok(())
    }

    fn pattern_edit(&mut self, command: &str) -> Result<(), String> {
        let (g, p) = (self.group(), self.pattern());
        match command {
            "create" => self.new_pattern(g, None),
            "double" => self.edit(move |d| d.double_pattern(g, p)),
            "duplicate" => self.edit(move |d| d.duplicate_pattern(g, p).map(|_| ())),
            "delete" => self.edit(move |d| d.delete_pattern(g, p)),
            _ => Err(format!("Pattern action {command} unavailable")),
        }
    }

    fn scene_edit(&mut self, command: &str) -> Result<(), String> {
        let scene = self.scene();
        match command {
            "create" | "insert" => self.edit(|d| d.add_scene().map(|_| ())),
            "duplicate" => self.edit(move |d| d.duplicate_scene(scene).map(|_| ())),
            "delete" => self.edit(move |d| d.delete_scene(scene)),
            "clear" => self.edit(move |d| {
                for group in 0..d.project().groups.len() {
                    d.assign_pattern(scene, group, None)?;
                }
                Ok(())
            }),
            "append" => self.edit(move |d| d.add_section(scene).map(|_| ())),
            _ => Err(format!("Scene action {command} unavailable")),
        }
    }

    fn sampling_view(&mut self, project: &Project, view: &mut View) {
        view.sampling.record.recording = self.get("sampling_recording") > 0.0;
        view.sampling.record.waiting = self.get("sampling_waiting") > 0.0;
        let choice = |key: &str, name: &str, max: f64| {
            let value = self.get(key);
            surface::SamplingKnob {
                key: key.into(),
                name: name.into(),
                text: format!("{value:.0}"),
                value,
                min: if key == "sampling_bars" { 1.0 } else { 0.0 },
                max,
                step: 1.0,
                turn: 0.1,
            }
        };
        view.sampling.pages[0] = vec![surface::KnobPage {
            name: "CAPTURE".into(),
            knobs: vec![
                choice("sampling_source", "DECK", 3.0),
                choice("sampling_stem", "STEM", 4.0),
                choice("sampling_bars", "BARS", 16.0),
                choice("sampling_slice", "SLICES", 2.0),
            ],
        }];
        let (g, s, z) = (self.group(), self.sound(), self.sampling_zone);
        let Some(sound) = project.groups.get(g).and_then(|g| g.sounds.get(s)) else {
            return;
        };
        let Some(sampler) = &sound.sampler else {
            view.sampling.no_sample = Some(surface::NoSample {
                title: "NO SAMPLE".into(),
                detail: sound.instrument.clone(),
            });
            return;
        };
        let Some(zone) = sampler.zones.get(z) else {
            return;
        };
        let Ok(Some(sample)) = self.handle.sample(g, s, z) else {
            view.sampling.no_sample = Some(surface::NoSample {
                title: "SAMPLE UNAVAILABLE".into(),
                detail: zone.sample.path.clone(),
            });
            return;
        };
        let frames = sample.frames.len();
        let play = (
            zone.play_range.0.max(0) as usize,
            (zone.play_range.1.max(0) as usize).min(frames),
        );
        let play = if play.1 > play.0 { play } else { (0, frames) };
        let shown = self.sample_view.unwrap_or((0.0, frames as f64));
        if self
            .sample_data
            .as_ref()
            .is_none_or(|old| !Arc::ptr_eq(old, &sample))
        {
            self.sample_overview = peaks(&sample.frames, (0, frames), 512);
            self.sample_data = Some(sample.clone());
            self.sample_wave_range = None;
        }
        let wave_range = (
            shown.0.max(0.0) as usize,
            (shown.1.max(0.0) as usize).min(frames),
        );
        if self.sample_wave_range != Some(wave_range) {
            self.sample_detail = peaks(&sample.frames, wave_range, 512);
            self.sample_wave_range = Some(wave_range);
        }
        if self.sample_slices.is_empty() {
            self.sample_slices.push(play.0);
        }
        view.sampling.sample = Some(surface::SampleView {
            name: std::path::Path::new(&zone.sample.path)
                .file_name()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: zone.sample.path.clone(),
            frames,
            rate: sample.rate,
            overview: self.sample_overview.clone(),
            detail: self.sample_detail.clone(),
            view: shown,
            play,
            looped: zone.loop_active.then_some((
                zone.loop_range.0.max(0) as usize,
                zone.loop_range.1.max(0) as usize,
            )),
            selection: (zone.selection_range.1 > zone.selection_range.0).then_some((
                zone.selection_range.0.max(0) as usize,
                zone.selection_range.1.max(0) as usize,
            )),
            slices: self.sample_slices.clone(),
            focus_zone: z,
            zones: sampler
                .zones
                .iter()
                .map(|zone| (zone.key_range, zone.velocity_range))
                .collect(),
            ..Default::default()
        });
        let knob = |key: &str, name: &str, value: f64, max: f64| surface::SamplingKnob {
            key: key.into(),
            name: name.into(),
            text: format!("{value:.0}"),
            value,
            min: 0.0,
            max,
            step: 1.0,
            turn: 1.0 / 127.0,
        };
        view.sampling.pages[1] = vec![surface::KnobPage {
            name: "RANGE".into(),
            knobs: vec![
                knob("start", "START", play.0 as f64, frames as f64),
                knob("end", "END", play.1 as f64, frames as f64),
                knob(
                    "loop_start",
                    "LOOP START",
                    zone.loop_range.0 as f64,
                    frames as f64,
                ),
                knob(
                    "loop_end",
                    "LOOP END",
                    zone.loop_range.1 as f64,
                    frames as f64,
                ),
            ],
        }];
        view.sampling.slice = vec![
            surface::SamplingKnob {
                key: "mode".into(),
                name: "MODE".into(),
                text: ["SPLIT", "GRID", "DETECT", "MANUAL"][self.slice_mode].into(),
                value: self.slice_mode as f64,
                min: 0.0,
                max: 3.0,
                step: 1.0,
                turn: 0.1,
            },
            surface::SamplingKnob {
                key: "count".into(),
                name: "SLICES".into(),
                text: self.slice_count.to_string(),
                value: self.slice_count as f64,
                min: 1.0,
                max: 64.0,
                step: 1.0,
                turn: 1.0 / 32.0,
            },
            surface::SamplingKnob {
                key: "grid".into(),
                name: "GRID".into(),
                text: format!("1/{}", self.slice_grid),
                value: self.slice_grid as f64,
                min: 4.0,
                max: 32.0,
                step: 4.0,
                turn: 0.1,
            },
            surface::SamplingKnob {
                key: "sensitivity".into(),
                name: "SENS".into(),
                text: format!("{:.0}%", self.slice_sensitivity * 100.0),
                value: self.slice_sensitivity,
                min: 0.0,
                max: 1.0,
                step: 0.01,
                turn: 0.01,
            },
        ];
        view.sampling.pages[3] = vec![surface::KnobPage {
            name: "ZONE".into(),
            knobs: vec![
                knob("low", "LOW KEY", zone.key_range.0 as f64, 127.0),
                knob("high", "HIGH KEY", zone.key_range.1 as f64, 127.0),
                knob("root", "ROOT", zone.root_key as f64, 127.0),
                knob(
                    "zone",
                    "ZONE",
                    z as f64,
                    sampler.zones.len().saturating_sub(1) as f64,
                ),
            ],
        }];
    }

    fn sample_knob(&mut self, index: usize, value: f64) -> Result<(), String> {
        if !value.is_finite() {
            return Err("Invalid sample parameter".into());
        }
        if self.sampling_tab == 0 {
            if let Some(key) = [
                "sampling_source",
                "sampling_stem",
                "sampling_bars",
                "sampling_slice",
            ]
            .get(index)
            {
                self.set(key, value.round());
            }
            return Ok(());
        }
        if self.sampling_tab == 2 {
            match index {
                0 => self.slice_mode = value.round().clamp(0.0, 3.0) as usize,
                1 => self.slice_count = value.round().clamp(1.0, 64.0) as usize,
                2 => self.slice_grid = value.round().clamp(4.0, 32.0) as u32,
                3 => self.slice_sensitivity = value.clamp(0.0, 1.0),
                _ => return Ok(()),
            }
            return self.regenerate_slices();
        }
        let (g, s, z) = (self.group(), self.sound(), self.sampling_zone);
        let field = match (self.sampling_tab, index) {
            (1, 0) => "play_range/low",
            (1, 1) => "play_range/high",
            (1, 2) => "loop_range/low",
            (1, 3) => "loop_range/high",
            (3, 0) => "key_range/low",
            (3, 1) => "key_range/high",
            (3, 2) => "root_key",
            (3, 3) => {
                self.sampling_zone = value.max(0.0) as usize;
                self.sample_slices.clear();
                return Ok(());
            }
            _ => return Ok(()),
        };
        self.edit(move |d| {
            d.set(
                "Sample range",
                Target::Zone(g, s, z),
                field,
                Value::Int(value.round() as i64),
            )
        })
    }

    fn regenerate_slices(&mut self) -> Result<(), String> {
        let project = self.project().ok_or("Maschine document unavailable")?;
        let (g, s, z) = (self.group(), self.sound(), self.sampling_zone);
        let zone = project
            .groups
            .get(g)
            .and_then(|g| g.sounds.get(s))
            .and_then(|s| s.sampler.as_ref())
            .and_then(|s| s.zones.get(z))
            .ok_or("Sample unavailable")?;
        let sample = self.handle.sample(g, s, z)?.ok_or("Sample unavailable")?;
        let range = (
            zone.play_range.0.max(0) as usize,
            (zone.play_range.1.max(0) as usize).min(sample.frames.len()),
        );
        let range = if range.1 > range.0 {
            range
        } else {
            (0, sample.frames.len())
        };
        self.sample_slices = match self.slice_mode {
            0 => opendj_engine::maschine::sample_edit::split_positions(range, self.slice_count),
            1 => opendj_engine::maschine::sample_edit::grid_positions(
                sample.rate,
                range,
                project.tempo,
                self.slice_grid,
            ),
            2 => opendj_engine::maschine::sample_edit::detect_positions(
                &sample,
                range,
                self.slice_sensitivity,
                true,
            ),
            _ => return Ok(()),
        };
        Ok(())
    }

    fn sample_call(&mut self, call: surface::SamplingCall) -> Result<(), String> {
        use surface::SamplingCall;
        let (g, s, z) = (self.group(), self.sound(), self.sampling_zone);
        match call {
            SamplingCall::View(start, end) => {
                self.sample_view = Some((start, end.max(start + 1.0)))
            }
            SamplingCall::SplitSlice(index) => {
                let sample = self.handle.sample(g, s, z)?.ok_or("Sample unavailable")?;
                let a = *self.sample_slices.get(index).ok_or("Slice unavailable")?;
                let b = self
                    .sample_slices
                    .get(index + 1)
                    .copied()
                    .unwrap_or(sample.frames.len());
                if b > a + 1 {
                    self.sample_slices.insert(index + 1, (a + b) / 2);
                }
            }
            SamplingCall::RemoveSlice(index) => {
                if index > 0 && index < self.sample_slices.len() {
                    self.sample_slices.remove(index);
                }
            }
            SamplingCall::ClearSlices => {
                self.sample_slices.truncate(1);
            }
            SamplingCall::MoveSlice(index, at) => {
                if index < self.sample_slices.len() {
                    let lower = index
                        .checked_sub(1)
                        .and_then(|i| self.sample_slices.get(i))
                        .map_or(0, |v| v + 1);
                    let upper = self
                        .sample_slices
                        .get(index + 1)
                        .map_or(usize::MAX, |v| v.saturating_sub(1));
                    self.sample_slices[index] = at.clamp(lower, upper.max(lower));
                }
            }
            SamplingCall::ApplySlices => {
                let sample = self.handle.sample(g, s, z)?.ok_or("Sample unavailable")?;
                let starts = self.sample_slices.clone();
                let specs = starts
                    .iter()
                    .enumerate()
                    .map(|(index, start)| ZoneSpec {
                        from: ZoneFrom::Zone(z),
                        file: None,
                        set: vec![
                            ("play_range/low", *start as f64),
                            (
                                "play_range/high",
                                starts
                                    .get(index + 1)
                                    .copied()
                                    .unwrap_or(sample.frames.len())
                                    as f64,
                            ),
                            ("root_key", (60 + index).min(127) as f64),
                            ("key_range/low", (60 + index).min(127) as f64),
                            ("key_range/high", (60 + index).min(127) as f64),
                        ],
                    })
                    .collect::<Vec<_>>();
                self.edit(move |d| d.set_zones(g, s, &specs, "Apply slices"))?;
                self.sampling_zone = 0;
            }
            SamplingCall::FocusZone(index) => {
                self.sampling_zone = index;
                self.sample_slices.clear();
                self.sample_view = None;
            }
            SamplingCall::RemoveZone(index) => {
                let p = self.project().ok_or("Maschine document unavailable")?;
                let count = p
                    .groups
                    .get(g)
                    .and_then(|g| g.sounds.get(s))
                    .and_then(|s| s.sampler.as_ref())
                    .map_or(0, |s| s.zones.len());
                let specs: Vec<_> = (0..count)
                    .filter(|i| *i != index)
                    .map(|i| ZoneSpec {
                        from: ZoneFrom::Zone(i),
                        file: None,
                        set: Vec::new(),
                    })
                    .collect();
                self.edit(move |d| d.set_zones(g, s, &specs, "Remove zone"))?;
                self.sampling_zone = self.sampling_zone.min(count.saturating_sub(2));
                self.sample_slices.clear();
            }
            SamplingCall::AutoLoop => {
                let sample = self.handle.sample(g, s, z)?.ok_or("Sample unavailable")?;
                let range = self.sample_view.map_or((0, sample.frames.len()), |(a, b)| {
                    (
                        a.max(0.0) as usize,
                        (b.max(0.0) as usize).min(sample.frames.len()),
                    )
                });
                let (a, b) = opendj_engine::maschine::sample_edit::find_loop(&sample, range)
                    .ok_or("No loop found")?;
                self.edit(move |d| {
                    let target = Target::Zone(g, s, z);
                    d.set("Auto loop", target, "loop_range/low", Value::Int(a as i64))?;
                    d.set("Auto loop", target, "loop_range/high", Value::Int(b as i64))?;
                    d.set("Auto loop", target, "loop_active", Value::Int(1))
                })?;
            }
            SamplingCall::Edit(function) => self.sample_edit(function)?,
            SamplingCall::Prehear(start, end) => self.handle.preview(g, s, z, start, end)?,
            other => return Err(format!("Sampling action unavailable: {other:?}")),
        }
        Ok(())
    }

    fn sample_edit(&mut self, function: usize) -> Result<(), String> {
        use opendj_engine::maschine::sample_edit as edit;
        let (g, s, z) = (self.group(), self.sound(), self.sampling_zone);
        let p = self.project().ok_or("Maschine document unavailable")?;
        let zone = p
            .groups
            .get(g)
            .and_then(|g| g.sounds.get(s))
            .and_then(|s| s.sampler.as_ref())
            .and_then(|s| s.zones.get(z))
            .ok_or("Sample unavailable")?;
        let sample = self.handle.sample(g, s, z)?.ok_or("Sample unavailable")?;
        let range = if zone.selection_range.1 > zone.selection_range.0 {
            (
                zone.selection_range.0.max(0) as usize,
                zone.selection_range.1.max(0) as usize,
            )
        } else {
            (
                zone.play_range.0.max(0) as usize,
                (zone.play_range.1.max(0) as usize).min(sample.frames.len()),
            )
        };
        let edited = match function {
            1 => edit::truncate(&sample, range),
            2 => edit::normalize(&sample, range),
            3 => edit::reverse(&sample, range),
            4 => edit::fade(&sample, range, true),
            5 => edit::fade(&sample, range, false),
            6 => edit::remove_dc(&sample, range),
            7 => edit::silence(&sample, range),
            11 => edit::duplicate(&sample, range),
            _ => return Err("Choose a supported sample edit".into()),
        };
        let root = std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|p| std::path::PathBuf::from(p).join(".local/share"))
            })
            .ok_or("No sample edit directory")?;
        let directory = root.join("opendj/maschine/edited-samples");
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let output = edit::edited_path(&directory, std::path::Path::new(&zone.sample.path));
        edit::write_wav(&output, &edited)?;
        let frames = edited.frames.len();
        self.edit(move |d| d.set_zone_sample(g, s, z, &output, frames, "Sample edit"))?;
        self.sample_view = None;
        self.sample_slices.clear();
        Ok(())
    }
}

#[derive(Clone)]
struct Parameter {
    target: Target,
    field: String,
    name: String,
    value: f64,
    min: f64,
    max: f64,
}

impl Parameter {
    fn new(target: Target, field: &str, name: &str, value: f64, min: f64, max: f64) -> Self {
        Self {
            target,
            field: field.into(),
            name: name.into(),
            value,
            min,
            max,
        }
    }
    fn normalized(&self) -> f64 {
        ((self.value - self.min) / (self.max - self.min).max(f64::EPSILON)).clamp(0.0, 1.0)
    }
    fn view(&self) -> KnobView {
        KnobView {
            name: self.name.clone(),
            text: format!("{:.2}", self.value),
            value: self.normalized(),
            ..Default::default()
        }
    }
    fn set(&self, document: &mut Document, value: f64) -> Result<(), String> {
        let value = value.clamp(self.min, self.max);
        let discrete = matches!(
            self.field.as_str(),
            "engine"
                | "mode"
                | "polyphony"
                | "envelope_type"
                | "filter"
                | "lfo/type"
                | "reverse"
                | "cymbal_sample"
                | "shaker_fill"
                | "shaker_rate"
                | "shaker_length"
        ) || self.field.ends_with("_mode")
            || self.field.ends_with("_sync");
        document.set(
            "Controller parameter",
            self.target,
            &self.field,
            if discrete {
                Value::Int(value.round() as i64)
            } else {
                Value::Float(value)
            },
        )
    }
}

fn parameter_range(module: &str, field: &str) -> Option<(f64, f64)> {
    if matches!(field, "editor_tab" | "focused_zone" | "ui_page") {
        return None;
    }
    Some(match (module, field) {
        ("Kick", "engine") => (0.0, 8.0),
        ("Snare", "mode") => (0.0, 9.0),
        ("Hihat", "mode") | ("Tom", "engine") => (0.0, 3.0),
        ("Perc", "mode") => (0.0, 2.0),
        ("Cymbal", "engine") => (0.0, 1.0),
        (_, "cymbal_sample") => (0.0, 5.0),
        (_, "shaker_env_mode" | "shaker_fill" | "crash_mode") => (0.0, 2.0),
        (_, "shaker_rate") => (0.0, 6.0),
        (_, "shaker_length") => (0.0, 7.0),
        ("Sampler" | "Audio", "tune") => (-36.0, 36.0),
        (_, "polyphony") => (0.0, 9.0),
        (_, "envelope_type") => (0.0, 2.0),
        ("Sampler", "filter") => (0.0, 8.0),
        (_, "lfo/type") => (0.0, 4.0),
        ("Filter", "amount") => (-1.0, 1.0),
        (
            _,
            "pan"
            | "eq_gain"
            | "output_gain"
            | "velocity"
            | "velocity_scaling"
            | "mod_envelope/pitch"
            | "mod_envelope/cutoff"
            | "mod_envelope/drive"
            | "mod_envelope/pan",
        ) => (-1.0, 1.0),
        _ => (0.0, 1.0),
    })
}

fn value(registry: &ControlRegistry, group: &str, key: &str) -> f64 {
    registry.get(group, key).map_or(0.0, |c| c.get())
}
fn set(registry: &ControlRegistry, group: &str, key: &str, value: f64) {
    if let Some(control) = registry.get(group, key) {
        control.set(value);
    }
}
fn grid_ticks(index: i64) -> i64 {
    const TICKS: [i64; 16] = [
        3840, 1920, 960, 480, 240, 120, 60, 30, 1280, 640, 320, 160, 80, 40, 20, 10,
    ];
    TICKS[index.clamp(0, 15) as usize]
}

// DECKARD's project palette (viewer/layout.rs), shared by groups and sounds.
const PALETTE: [[u8; 3]; 17] = [
    [255, 66, 62],
    [255, 116, 66],
    [252, 152, 36],
    [255, 221, 26],
    [255, 255, 25],
    [163, 255, 24],
    [83, 255, 83],
    [25, 234, 163],
    [25, 234, 235],
    [24, 206, 255],
    [77, 148, 255],
    [132, 114, 255],
    [182, 110, 255],
    [223, 95, 255],
    [255, 83, 240],
    [255, 72, 128],
    [255, 255, 255],
];
fn color_index(color: Color, fallback: usize) -> usize {
    match color {
        Color::Palette(index) => usize::from(index).min(16),
        Color::Default => fallback % 16,
    }
}
fn rgb(color: Color, fallback: usize) -> [u8; 3] {
    PALETTE[color_index(color, fallback)]
}

fn update_meters(view: &mut View, registry: &ControlRegistry, deck: usize) {
    for (gi, group) in view.groups.iter_mut().enumerate() {
        for (si, sound) in group.sounds.iter_mut().enumerate() {
            let meter = value(
                registry,
                &format!("[Maschine{deck}]"),
                &format!("group{}_sound{}_meter", gi + 1, si + 1),
            )
            .clamp(0.0, 1.0) as f32;
            sound.mix.meter = (meter, meter);
            sound.playing = meter > 0.0001;
        }
        let peak = group
            .sounds
            .iter()
            .map(|s| s.mix.meter.0)
            .fold(0.0f32, f32::max);
        group.mix.meter = (peak, peak);
        group.playing = peak > 0.0001;
    }
}

fn peaks(frames: &[[f32; 2]], range: (usize, usize), columns: usize) -> Vec<(f32, f32)> {
    let start = range.0.min(frames.len());
    let end = range.1.clamp(start, frames.len());
    (0..columns)
        .map(|column| {
            let a = start + (end - start) * column / columns;
            let b = (start + (end - start) * (column + 1) / columns)
                .max(a + 1)
                .min(end);
            frames[a..b]
                .iter()
                .flat_map(|frame| frame.iter().copied())
                .fold((0.0f32, 0.0f32), |(lo, hi), value| {
                    (lo.min(value), hi.max(value))
                })
        })
        .collect()
}
