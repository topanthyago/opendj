//! What a controller script can reach: controls (with Mixxx's `_set_default`
//! style shortcuts and soft takeover), MIDI output, timers and the
//! scratch/brake filters. Script natives and MIDI input both go through here.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use opendj_control::{Behavior, ConfigKey, ControlHandle, ControlRegistry};

use crate::pickup::{self, Takeover};
use opendj_control::maschine_encoders as encoder_response;

/// The embedded Maschine document, as the existing controller thread sees it.
/// The app supplies snapshots and queues actions onto its document-owning thread.
/// This interface never opens a MIDI or audio device.
pub trait MaschineProvider: Send + Sync {
    /// Decks count from one, matching `[MaschineN]` and `[GrooveN]`.
    fn view(&self, deck: usize) -> Option<deckard_surface::View>;
    fn dispatch(&self, deck: usize, actions: Vec<deckard_surface::Action>);
    fn contextual_wheel(
        &self,
        _deck: usize,
        action: deckard_surface::Action,
        _group: usize,
        _sound: Option<usize>,
    ) -> deckard_surface::Action {
        action
    }
    fn error(&self, _deck: usize) -> Option<String> {
        None
    }
}

pub(crate) type MaschineSource = Arc<RwLock<Option<Arc<dyn MaschineProvider>>>>;

struct MaschineSurface {
    device: deckard_surface::device::Device,
    mode: u8,
    sampling: bool,
    held: HashSet<u8>,
    physical: HashSet<u8>,
    pinned: HashSet<u8>,
    encoders: HashMap<u8, u8>,
    encoder_turns: HashMap<String, f64>,
    wheel_target: Option<u8>,
    input_encoders: HashMap<u8, u8>,
    encoder_delta: i32,
    measuring_encoder: bool,
    taps: Vec<Instant>,
    browsing: bool,
    multi_select: bool,
    duplicate_from: Option<(u8, usize)>,
    duplicate_sent: Option<(u8, usize, Instant)>,
    feedback_epoch: Instant,
    feedback_base: HashMap<u8, Vec<u8>>,
    feedback_active: HashSet<u8>,
    pattern_bank: usize,
    pattern_pinned: bool,
    scene_bank: usize,
    step_page: usize,
    erase_sounds: HashMap<u8, usize>,
    erase_groups: HashSet<usize>,
    pads: HashSet<u8>,
}

impl MaschineSurface {
    fn sampling_response(view: &mut deckard_surface::View, counts: f64) {
        for knob in view
            .sampling
            .pages
            .iter_mut()
            .flatten()
            .flat_map(|page| page.knobs.iter_mut())
            .chain(view.sampling.slice.iter_mut())
        {
            if knob.key == "sampling_bars" {
                let bars = [1.0_f64, 2.0, 4.0, 8.0, 16.0];
                knob.value = bars
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        (*a - knob.value).abs().total_cmp(&(*b - knob.value).abs())
                    })
                    .map_or(0.0, |(index, _)| index as f64);
                knob.min = 0.0;
                knob.max = 4.0;
                knob.step = 1.0;
            }
            let range = knob.max - knob.min;
            if range > 0.0 && knob.step > 0.0 {
                // Canonical metadata carries choice indices. Large frame ranges use continuous travel.
                let discrete = matches!(
                    knob.key.as_str(),
                    "sampling_source"
                        | "sampling_stem"
                        | "sampling_bars"
                        | "sampling_slice"
                        | "mode"
                        | "count"
                        | "grid"
                        | "zone"
                );
                let musical = matches!(knob.key.as_str(), "low" | "high" | "root");
                if matches!(
                    knob.key.as_str(),
                    "start" | "end" | "loop_start" | "loop_end"
                ) {
                    // Frame quantization remains practical even for multi-minute recordings.
                    knob.step = knob.step.max((range / (counts * 0.75)).ceil());
                }
                knob.turn = counts / 127.0
                    * if discrete {
                        45.0 / 360.0
                    } else if musical {
                        15.0 / 360.0
                    } else {
                        270.0 / 360.0 * knob.step / range
                    };
            }
        }
    }

    fn angle_action(
        mut action: deckard_surface::Action,
        view: &deckard_surface::View,
        counts: f64,
    ) -> deckard_surface::Action {
        if let deckard_surface::Action::SamplingKnob { page, knob, value } = &mut action {
            if view.sampling.tab == 0
                && view.sampling.pages[0]
                    .get(*page)
                    .and_then(|p| p.knobs.get(*knob))
                    .is_some_and(|k| k.key == "sampling_bars")
            {
                *value = [1.0, 2.0, 4.0, 8.0, 16.0][value.round().clamp(0.0, 4.0) as usize];
            }
        }
        if let deckard_surface::Action::Turn { inc, .. } = &mut action {
            *inc *= 127.0 / (counts * 0.75);
        }
        action
    }

    fn new() -> Self {
        Self {
            device: deckard_surface::device::Device::new(
                "maschine_mk2",
                std::env::var_os("OPENDJ_MASCHINE_CONTROLLER").map(Into::into),
            ),
            mode: 0,
            sampling: false,
            held: HashSet::new(),
            physical: HashSet::new(),
            pinned: HashSet::new(),
            encoders: HashMap::new(),
            encoder_turns: HashMap::new(),
            wheel_target: None,
            input_encoders: HashMap::new(),
            encoder_delta: 0,
            measuring_encoder: false,
            taps: Vec::new(),
            browsing: false,
            multi_select: false,
            duplicate_from: None,
            duplicate_sent: None,
            feedback_epoch: Instant::now(),
            feedback_base: HashMap::new(),
            feedback_active: HashSet::new(),
            pattern_bank: 0,
            pattern_pinned: false,
            scene_bank: 0,
            step_page: 0,
            erase_sounds: HashMap::new(),
            erase_groups: HashSet::new(),
            pads: HashSet::new(),
        }
    }

    /// Adapt the pinned script's PATTERN/SHIFT+PATTERN mapping without changing
    /// its step sequencer, pin button, display or STEP follow implementation.
    fn input(
        &mut self,
        bytes: &[u8],
        view: &deckard_surface::View,
        now: Instant,
    ) -> Vec<deckard_surface::device::Out> {
        let mut normalized = [0u8; 3];
        let bytes = if bytes.len() == 3 && bytes[0] & 0xf0 == 0xb0 && (15..=23).contains(&bytes[1])
        {
            let virtual_value = self.input_encoders.entry(bytes[1]).or_insert(bytes[2]);
            if !self.measuring_encoder {
                *virtual_value =
                    (i32::from(*virtual_value) + self.encoder_delta).rem_euclid(128) as u8;
            }
            normalized.copy_from_slice(bytes);
            normalized[2] = *virtual_value;
            &normalized[..]
        } else {
            bytes
        };
        if bytes.len() != 3 || bytes[0] & 0xf0 != 0xb0 || bytes[1] != 26 {
            return self.device.midi("controls", bytes, view, now);
        }
        let shift = self.held.contains(&49);
        let mut events = Vec::new();
        if bytes[2] > 0 {
            self.pattern_pinned = false;
            events.extend(self.device.midi(
                "controls",
                &[0xb0, 49, if shift { 0 } else { 127 }],
                view,
                now,
            ));
        }
        events.extend(self.device.midi("controls", bytes, view, now));
        if bytes[2] > 0 {
            events.extend(self.device.midi(
                "controls",
                &[0xb0, 49, if shift { 127 } else { 0 }],
                view,
                now,
            ));
            if !shift && self.mode != 2 {
                // Explicitly choose the unpinned state by toggling twice.
                // This disables the script's short-tap auto-pin on release.
                for _ in 0..2 {
                    events.extend(self.device.midi("controls", &[0xb0, 39, 127], view, now));
                    events.extend(self.device.midi("controls", &[0xb0, 39, 0], view, now));
                }
            }
        }
        events
    }

    fn pattern_state(&self, view: &deckard_surface::View) -> Option<serde_json::Value> {
        (self.mode == 2).then(|| {
            let group = view.groups.get(view.group);
            let assigned = group.and_then(|g| g.scene_pattern);
            serde_json::json!({
                "group": view.group,
                "pinned": self.pattern_pinned,
                "assigned": assigned,
                "assigned_name": group.and_then(|g| assigned.and_then(|p| g.patterns.get(p))).map(|p| p.name.as_str()),
                "editor": view.pattern,
                "editor_name": group.and_then(|g| g.patterns.get(view.pattern)).map(|p| p.name.as_str()),
                "playing": view.playing,
                "playing_scene": view.playing_scene,
                "queued_scene": view.queued_scene,
                "scene": view.scene,
                "button": if assigned.is_some() { "STOP GROUP" } else { "PLAY GROUP" },
            })
        })
    }

    /// Add MK2 secondary gestures around the existing DECKARD device script.
    /// Returning actions replaces that message's app requests while preserving
    /// the script's internal encoder counters and display-page state.
    fn gesture(
        &mut self,
        bytes: &[u8],
        view: &deckard_surface::View,
        keyboard: bool,
        now: Instant,
        registry: &ControlRegistry,
    ) -> Option<Vec<deckard_surface::Action>> {
        use deckard_surface::Action as A;
        if bytes.len() != 3 {
            return None;
        }
        let (status, key, value) = (bytes[0] & 0xf0, bytes[1], bytes[2]);
        let shift = self.held.contains(&49);
        if (status == 0x80 || (status == 0x90 && value == 0)) && (36..=51).contains(&key) {
            self.erase_sounds.remove(&(key - 35));
        }
        if status == 0x90 && value > 0 && (36..=51).contains(&key) {
            let pad = key - 35;
            if self.held.contains(&2) && self.mode == 1 && !self.sampling {
                let step = view.grid.step_ticks.max(1);
                let first = ((view.pattern_start + step - 1) / step) * step;
                let at = first + (self.step_page * 16 + usize::from(pad - 1)) as i64 * step;
                return Some(vec![A::Events("erase_step", at)]);
            }
            if self.held.contains(&2) && view.playing && !self.sampling {
                let target = view.group * 16 + usize::from(pad - 1);
                self.erase_sounds.insert(pad, target);
                return Some(vec![A::Events("erase_sound_window", target as i64)]);
            }
            if self.held.contains(&29)
                && self.mode == 0
                && !self.sampling
                && ![30, 31, 33].iter().any(|key| self.held.contains(key))
            {
                let to = usize::from(pad - 1);
                let valid = view
                    .groups
                    .get(view.group)
                    .is_some_and(|g| to < g.sounds.len());
                if !valid {
                    return Some(Vec::new());
                }
                if let Some((0, from)) = self.duplicate_from.take() {
                    return Some(vec![A::CopySound { from, to }]);
                }
                self.duplicate_from = Some((0, to));
                self.duplicate_sent = None;
                return Some(Vec::new());
            }
            if self.held.contains(&29) && (self.mode == 2 || self.mode == 3) {
                let slot = if self.mode == 2 {
                    self.pattern_bank
                } else {
                    self.scene_bank
                } * 16
                    + usize::from(pad - 1);
                let index = if self.mode == 2 {
                    view.groups
                        .get(view.group)
                        .and_then(|g| g.patterns.iter().position(|p| p.slot == slot))
                } else {
                    (slot < view.scenes.len()).then_some(slot)
                };
                let Some((mode, from)) = self.duplicate_from else {
                    self.duplicate_sent = None;
                    self.duplicate_from = index.map(|index| (self.mode, index));
                    return Some(Vec::new());
                };
                if mode != self.mode {
                    self.duplicate_sent = None;
                    self.duplicate_from = index.map(|index| (self.mode, index));
                    return Some(Vec::new());
                }
                self.duplicate_from = None;
                if self.mode == 3 {
                    return Some(vec![A::CopyScene {
                        from,
                        to: slot.min(view.scenes.len()),
                    }]);
                }
                if let Some(to) = index {
                    return Some(vec![A::CopyPattern {
                        group: view.group,
                        from,
                        to,
                    }]);
                }
                let patterns = &view.groups[view.group].patterns;
                let to = patterns
                    .iter()
                    .position(|p| p.slot > slot)
                    .unwrap_or(patterns.len());
                return Some(vec![
                    A::NewPatternAt {
                        group: view.group,
                        slot,
                    },
                    A::CopyPattern {
                        group: view.group,
                        from: from + usize::from(to <= from),
                        to,
                    },
                ]);
            }
            if shift && self.held.contains(&30) {
                return Some(vec![
                    A::FocusSound(usize::from(pad - 1)),
                    A::Events("select_sound", i64::from(pad - 1)),
                ]);
            }
            if self.held.contains(&30) && self.multi_select {
                return Some(vec![
                    A::FocusSound(usize::from(pad - 1)),
                    A::Events("select_sound_toggle", i64::from(pad - 1)),
                ]);
            }
            if shift {
                let action = match pad {
                    1 => A::Edit("undo"),
                    2 => A::Edit("redo"),
                    3 => A::Edit("undo_step"),
                    4 => A::Edit("redo_step"),
                    5 => A::Events("quantize", 0),
                    6 => A::Events("quantize_half", 0),
                    7 => A::Events("nudge", -1),
                    8 => A::Events("nudge", 1),
                    9 => A::Events("clear_all", 1),
                    10 => A::Edit("clear_modulation"),
                    11 => A::Edit("copy"),
                    12 => A::Edit("paste"),
                    13..=16 => A::Scale {
                        root: view.scale_root
                            + match pad {
                                13 => -1,
                                14 => 1,
                                15 => -12,
                                _ => 12,
                            },
                        bank: view.scale_bank,
                        kind: view.scale_kind,
                    },
                    _ => return None,
                };
                return Some(vec![action]);
            }
            if keyboard
                && self.mode == 0
                && !self.sampling
                && ![2, 29, 30, 31, 33].iter().any(|k| self.held.contains(k))
            {
                return Some(Vec::new());
            }
        }
        if status != 0xb0 {
            return None;
        }
        if (15..=23).contains(&key) {
            self.encoder_delta = 0;
            self.measuring_encoder = false;
            let Some(old) = self.encoders.insert(key, value) else {
                return Some(Vec::new());
            };
            let delta = (i32::from(value) - i32::from(old) + 192) % 128 - 64;
            let wheel = key == 15;
            let measuring = registry
                .get(encoder_response::GROUP, "measure")
                .is_some_and(|c| c.get() == if wheel { 2.0 } else { 1.0 });
            self.encoder_delta = delta;
            self.measuring_encoder = measuring;
            if measuring {
                let measured_key = registry.get(encoder_response::GROUP, "measured_encoder");
                if measured_key.as_ref().is_some_and(|c| c.get() == 0.0) {
                    measured_key.as_ref().unwrap().set(f64::from(key));
                }
                if measured_key.is_some_and(|c| c.get() == f64::from(key)) {
                    if let Some(c) = registry.get(encoder_response::GROUP, "measured") {
                        c.set(c.get() + f64::from(delta));
                    }
                }
                return Some(Vec::new());
            }
            let context = format!(
                "{key}:{}:{}:{}:{}:{shift}",
                self.mode,
                self.sampling,
                self.browsing,
                [4, 27, 10]
                    .iter()
                    .find(|k| self.held.contains(k))
                    .unwrap_or(&0)
            );
            let discrete = encoder_response::steps(
                self.encoder_turns.entry(context).or_default(),
                f64::from(delta),
                encoder_response::counts(registry, wheel) / 8.0 * if shift { 10.0 } else { 1.0 },
            );
            if wheel && !self.browsing && self.mode != 2 {
                let fine = if shift { 0.1 } else { 1.0 };
                let turns = f64::from(delta) / encoder_response::counts(registry, true);
                let action = match self.wheel_target {
                    Some(51) => {
                        A::Volume((view.master_volume + turns / 0.75 * fine).clamp(0.0, 1.0))
                    }
                    Some(50) => {
                        A::Swing((view.swing + turns / 0.75 * 100.0 * fine).clamp(0.0, 100.0))
                    }
                    Some(35) => {
                        let movement = encoder_response::steps(
                            self.encoder_turns
                                .entry(format!("tempo:{shift}"))
                                .or_default(),
                            turns * 360.0 * fine,
                            15.0,
                        );
                        if movement == 0 {
                            return Some(Vec::new());
                        }
                        A::Tempo((view.tempo + movement as f64).clamp(10.0, 400.0))
                    }
                    _ if !self.sampling => {
                        if discrete == 0 {
                            return Some(Vec::new());
                        }
                        A::Slot(discrete)
                    }
                    _ => {
                        self.encoder_delta = (discrete * 8) as i32;
                        return None;
                    }
                };
                return Some(vec![action]);
            }
            if self.held.contains(&4) {
                if discrete == 0 {
                    return Some(Vec::new());
                }
                let action = match key {
                    16 => A::Grid("step", (view.grid.step + discrete).clamp(0, 15)),
                    17 => A::Grid("nudge", (view.grid.nudge + discrete).clamp(0, 4)),
                    18 => A::Grid("perform", (view.grid.perform + discrete).clamp(0, 6)),
                    _ => return Some(Vec::new()),
                };
                return Some(vec![action]);
            }
            if self.held.contains(&10) && key == 16 {
                let rate = view
                    .note_repeat
                    .settings
                    .iter()
                    .find(|s| s.key == "rate")
                    .map_or(240.0, |s| s.value);
                return Some(vec![A::Repeat(
                    "rate",
                    (rate * 2f64.powf(-f64::from(delta) / 32.0)).clamp(15.0, 3840.0),
                )]);
            }
            if self.held.contains(&27) {
                if discrete == 0 {
                    return Some(Vec::new());
                }
                let sound = view
                    .groups
                    .get(view.group)
                    .and_then(|g| g.sounds.get(view.sound));
                let Some(sound) = sound else {
                    return Some(Vec::new());
                };
                let (field, shown, max) = match key {
                    16 => ("choke_group", sound.choke_group, 8),
                    17 => ("choke_mode", sound.choke_mode, 1),
                    18 => ("link_group", sound.link_group, 8),
                    19 => ("link_mode", sound.link_mode, 1),
                    _ => return Some(Vec::new()),
                };
                return Some(vec![A::SoundSetting(
                    field,
                    (shown + discrete).clamp(0, max),
                )]);
            }
            // The pinned Lua pages use integer steps and a minimum of one per
            // packet. Feed whole angular steps through its own counters so local
            // browser/slice/page selection stays synchronized as well.
            if wheel {
                self.encoder_delta = (discrete * 8) as i32;
            } else if !self.sampling {
                let scale = if self.browsing {
                    Some(2)
                } else if self.mode == 2 && key == 16 {
                    Some(8)
                } else if self.mode == 1 && key == 17 {
                    Some(4)
                } else {
                    None
                };
                if let Some(scale) = scale {
                    self.encoder_delta = (discrete * scale) as i32;
                }
            }
            return None;
        }
        if value > 0 && [35, 50, 51].contains(&key) {
            self.wheel_target = if self.wheel_target == Some(key) {
                None
            } else {
                Some(key)
            };
        }
        if value == 0 {
            if key == 2 {
                self.erase_sounds.clear();
                self.erase_groups.clear();
            }
            if (80..=87).contains(&key) {
                self.erase_groups.remove(&usize::from(key - 80));
            }
            if key == 29 {
                self.duplicate_from = None;
            }
            return None;
        }
        if (80..=87).contains(&key) && self.held.contains(&2) && view.playing {
            let group = usize::from(key - 80);
            self.erase_groups.insert(group);
            return Some(vec![A::Events("erase_group_window", group as i64)]);
        }
        if self.mode == 1 {
            if key == 6 || key == 46 {
                self.step_page = self.step_page.saturating_add(1);
            }
            if key == 5 || key == 45 {
                self.step_page = self.step_page.saturating_sub(1);
            }
        }
        if (80..=87).contains(&key) && self.held.contains(&29) {
            let to = usize::from(key - 80);
            if let Some((4, from)) = self.duplicate_from {
                self.duplicate_from = None;
                return Some(vec![A::CopyGroup { from, to }]);
            }
            self.duplicate_sent = None;
            self.duplicate_from = Some((4, to));
            return Some(Vec::new());
        }
        if self.mode == 2 || self.mode == 3 {
            let bank = if self.mode == 2 {
                &mut self.pattern_bank
            } else {
                &mut self.scene_bank
            };
            if key == 47 || key == 46 {
                *bank = bank.saturating_add(1);
            }
            if key == 48 || key == 45 {
                *bank = bank.saturating_sub(1);
            }
        }
        let pattern_picker = self.mode == 2
            && ![4, 10, 27, 30, 31, 33]
                .iter()
                .any(|key| self.held.contains(key));
        if pattern_picker && key == 40 {
            let pattern = view.groups.get(view.group).and_then(|g| {
                if g.scene_pattern.is_some() {
                    None
                } else {
                    g.patterns.get(view.pattern).map(|_| view.pattern)
                }
            });
            return Some(vec![A::ScenePattern {
                group: view.group,
                pattern,
            }]);
        }
        if pattern_picker && key == 39 {
            self.pattern_pinned = !self.pattern_pinned;
        }
        if key == 39 {
            if let Some(mode) = [10, 4, 27, 30, 31, 33]
                .into_iter()
                .find(|k| self.held.contains(k))
            {
                if !self.pinned.insert(mode) {
                    self.pinned.remove(&mode);
                }
                return Some(Vec::new());
            }
        }
        if key == 33 && shift {
            return Some(vec![A::ChokeAll]);
        }
        if key == 11 && shift {
            return Some(vec![A::SlotEdit("add")]);
        }
        if key == 27 && shift {
            return Some(vec![A::KeyboardMode(!keyboard)]);
        }
        if key == 36 && !self.browsing && !self.sampling {
            if shift {
                return Some(vec![A::Metronome(!view.metronome)]);
            }
            if self
                .taps
                .last()
                .is_some_and(|at| now.saturating_duration_since(*at) > Duration::from_secs(2))
            {
                self.taps.clear();
            }
            self.taps.push(now);
            if self.taps.len() > 5 {
                self.taps.remove(0);
            }
            if self.taps.len() > 1 {
                let seconds = now.saturating_duration_since(self.taps[0]).as_secs_f64()
                    / (self.taps.len() - 1) as f64;
                if seconds > 0.0 {
                    return Some(vec![A::Tempo((60.0 / seconds).clamp(10.0, 400.0))]);
                }
            }
            return Some(Vec::new());
        }
        if self.held.contains(&10) && (43..=46).contains(&key) {
            return Some(vec![A::Repeat(
                "rate",
                [480.0, 240.0, 160.0, 120.0][usize::from(key - 43)],
            )]);
        }
        if self.held.contains(&4) {
            return Some(match key {
                40 => vec![A::Grid("step_snap", i64::from(!view.grid.step_snap))],
                41 => vec![A::Setting("quantize", 0, 1)],
                _ => Vec::new(),
            });
        }
        if self.held.contains(&27) {
            return Some(match key {
                40 => vec![A::KeyboardMode(false)],
                41 => vec![A::KeyboardMode(true)],
                42 => vec![A::FixedVelocity(!view.fixed_velocity)],
                43..=46 => vec![A::Scale {
                    root: view.scale_root + [-12, 12, -1, 1][usize::from(key - 43)],
                    bank: view.scale_bank,
                    kind: view.scale_kind,
                }],
                _ => Vec::new(),
            });
        }
        if self.held.contains(&30) && key == 46 {
            self.multi_select = !self.multi_select;
            return Some(Vec::new());
        }
        None
    }

    /// Feedback follows requests actually queued for the document thread.
    /// The provider has asynchronous dispatch, so this reports SENT, not completion.
    fn copy_feedback(
        &mut self,
        actions: &[deckard_surface::Action],
        view: &deckard_surface::View,
        now: Instant,
    ) {
        use deckard_surface::Action as A;
        for action in actions {
            let target = match action {
                A::CopySound { to, .. } => Some((0, *to)),
                A::CopyPattern { group, to, .. } => {
                    // NewPatternAt can insert a destination before the next snapshot.
                    let slot = actions
                        .iter()
                        .find_map(|a| match a {
                            A::NewPatternAt { group: g, slot } if g == group => Some(*slot),
                            _ => None,
                        })
                        .or_else(|| view.groups.get(*group)?.patterns.get(*to).map(|p| p.slot));
                    slot.map(|slot| (2, slot))
                }
                A::CopyScene { to, .. } => Some((3, *to)),
                A::CopyGroup { to, .. } => Some((4, *to)),
                _ => None,
            };
            if let Some((mode, to)) = target {
                self.duplicate_sent = Some((mode, to, now));
            }
        }
        if self.duplicate_sent.is_some_and(|(_, _, at)| {
            now.saturating_duration_since(at) >= Duration::from_millis(1000)
        }) {
            self.duplicate_sent = None;
        }
    }

    fn feedback_lights(
        &mut self,
        view: &deckard_surface::View,
        now: Instant,
        native: &[Vec<u8>],
    ) -> Vec<Vec<u8>> {
        for packet in native {
            if packet.len() == 9 && packet[..4] == [0xf0, 0x7d, 0x44, 0x4b] {
                self.feedback_base.insert(packet[4], packet.clone());
            }
        }
        let mut lights = Vec::new();
        if self.held.contains(&31) || self.held.contains(&33) {
            for (index, group) in view.groups.iter().take(8).enumerate() {
                let rgb = if group.muted {
                    group.rgb.map(|v| v / 16)
                } else {
                    group.rgb
                };
                lights.push(maschine_light(index as u8, rgb));
            }
            if let Some(group) = view.groups.get(view.group) {
                for (index, sound) in group.sounds.iter().take(16).enumerate() {
                    let rgb = if group.muted || sound.muted {
                        sound.rgb.map(|v| v / 16)
                    } else {
                        sound.rgb
                    };
                    lights.push(maschine_light(48 + index as u8, rgb));
                }
            }
        }
        let source = self.duplicate_from.and_then(|(mode, index)| {
            let slot = if mode == 2 {
                view.groups.get(view.group)?.patterns.get(index)?.slot
            } else {
                index
            };
            Some((mode, slot))
        });
        let target = self.duplicate_sent.map(|(mode, slot, _)| (mode, slot));
        // Only LEDs animate. LCD regions remain differential and acknowledged.
        let bright = now
            .saturating_duration_since(
                self.duplicate_sent
                    .map_or(self.feedback_epoch, |(_, _, at)| at),
            )
            .as_millis()
            / 200
            % 2
            == 0;
        for (mode, slot) in source.into_iter().chain(target) {
            let id = if mode == 4 {
                (slot < 8).then_some(slot as u8)
            } else {
                let bank = if mode == 2 {
                    self.pattern_bank
                } else if mode == 3 {
                    self.scene_bank
                } else {
                    0
                };
                (slot / 16 == bank && (mode == self.mode)).then_some(48 + (slot % 16) as u8)
            };
            if let Some(id) = id {
                let rgb = if mode == 0 {
                    view.groups
                        .get(view.group)
                        .and_then(|g| g.sounds.get(slot))
                        .map(|s| s.rgb)
                        .unwrap_or([127; 3])
                } else {
                    view.groups
                        .get(if mode == 4 { slot } else { view.group })
                        .map(|g| g.rgb)
                        .unwrap_or([127; 3])
                };
                lights.push(maschine_light(
                    id,
                    if bright {
                        rgb.map(|v| v.max(40))
                    } else {
                        [0; 3]
                    },
                ));
            }
        }
        let active = lights.iter().map(|p| p[4]).collect::<HashSet<_>>();
        for id in self.feedback_active.difference(&active) {
            if let Some(packet) = self.feedback_base.get(id) {
                lights.push(packet.clone());
            }
        }
        self.feedback_active = active;
        lights
    }

    fn overlay(
        &self,
        view: &deckard_surface::View,
        keyboard: bool,
        quantize: bool,
    ) -> serde_json::Value {
        let mode = [4, 10, 27, 30, 31, 33]
            .into_iter()
            .find(|key| self.held.contains(key))
            .or_else(|| (self.held.contains(&29) || self.duplicate_sent.is_some()).then_some(29));
        let Some(mode) = mode else {
            return serde_json::Value::Null;
        };
        let (name, mut buttons) = match mode {
            4 => ("GRID", vec!["PIN", "SNAP", "INPUT Q", "", "", "", "", ""]),
            10 => (
                "NOTE REPEAT",
                vec!["PIN", "", "", "", "1/8", "1/16", "1/16T", "1/32"],
            ),
            27 => (
                "PAD MODE",
                vec![
                    "PIN", "GROUP", "KEYBOARD", "FIXED", "OCT -", "OCT +", "SEMI -", "SEMI +",
                ],
            ),
            30 => ("SELECT", vec!["PIN", "", "", "", "", "", "MULTI"]),
            29 => ("DUPLICATE", vec!["", "", "", "", "", "", "", ""]),
            31 => ("SOLO", vec!["PIN", "", "", "", "", "", "", ""]),
            _ => ("MUTE", vec!["PIN", "", "", "", "", "", "", ""]),
        };
        if self.pinned.contains(&mode) {
            buttons[0] = "UNPIN";
        }
        let current = view
            .groups
            .get(view.group)
            .and_then(|g| g.sounds.get(view.sound));
        let parameter = |name: &str, text: String, value: f64| serde_json::json!({"name":name,"text":text,"value":value});
        let knobs = match mode {
            4 => vec![
                parameter(
                    "STEP GRID",
                    format!("{} ticks", view.grid.step_ticks),
                    view.grid.step as f64 / 15.0,
                ),
                parameter(
                    "NUDGE",
                    format!("1/{}", 1 << view.grid.nudge),
                    view.grid.nudge as f64 / 4.0,
                ),
                parameter(
                    "PERFORM",
                    view.grid.perform.to_string(),
                    view.grid.perform as f64 / 6.0,
                ),
                parameter(
                    "INPUT Q",
                    if quantize { "ON" } else { "OFF" }.into(),
                    if quantize { 1.0 } else { 0.0 },
                ),
            ],
            10 => view
                .note_repeat
                .settings
                .iter()
                .map(|s| {
                    parameter(
                        &s.name,
                        s.text.clone(),
                        (s.value - s.min) / (s.max - s.min).max(1.0),
                    )
                })
                .collect(),
            27 => current
                .map(|s| {
                    vec![
                        parameter(
                            "CHOKE GROUP",
                            s.choke_group.to_string(),
                            s.choke_group as f64 / 8.0,
                        ),
                        parameter(
                            "CHOKE MODE",
                            if s.choke_mode == 0 { "SEND" } else { "RECEIVE" }.into(),
                            s.choke_mode as f64,
                        ),
                        parameter(
                            "LINK GROUP",
                            s.link_group.to_string(),
                            s.link_group as f64 / 8.0,
                        ),
                        parameter(
                            "LINK MODE",
                            if s.link_mode == 0 { "SEND" } else { "RECEIVE" }.into(),
                            s.link_mode as f64,
                        ),
                    ]
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        serde_json::json!({"mode":mode,"title":name,"buttons":buttons,"knobs":knobs,
            "group":view.groups.get(view.group).map(|g| g.name.as_str()).unwrap_or(""),
            "sound":current.map(|s| s.name.as_str()).unwrap_or(""),"keyboard":keyboard,"multi":self.multi_select,"quantize":quantize,
            "focus_group":view.group,"focus_sound":view.sound,
            "sounds":view.groups.get(view.group).map(|g| g.sounds.iter().take(16).map(|s| serde_json::json!({"name":s.name,"muted":s.muted,"group_muted":g.muted})).collect::<Vec<_>>()).unwrap_or_default(),
            "groups":view.groups.iter().take(8).map(|g| serde_json::json!({"name":g.name,"muted":g.muted})).collect::<Vec<_>>(),
            "duplicate_source":self.duplicate_from.map(|(kind, index)| {
                let slot = if kind == 2 { view.groups.get(view.group).and_then(|g| g.patterns.get(index)).map_or(index, |p| p.slot) } else { index };
                serde_json::json!({"kind":kind,"index":slot})
            }),
            "duplicate_sent":self.duplicate_sent.map(|(kind, index, _)| serde_json::json!({"kind":kind,"index":index}))})
    }
}

/// Linear RGB addition preserves the contribution of simultaneous sounds.
/// A uniform scale fits an over-range sum into the MK2's output gamut.
pub fn maschine_group_rgb(group: &deckard_surface::GroupView) -> [u8; 3] {
    let mut sum = [0.0f32; 3];
    if !group.muted {
        for sound in &group.sounds {
            let meter = sound.mix.meter.0.max(sound.mix.meter.1);
            if sound.muted || !meter.is_finite() {
                continue;
            }
            for (channel, value) in sum.iter_mut().zip(sound.rgb) {
                *channel += f32::from(value) * meter.clamp(0.0, 1.0);
            }
        }
    }
    let peak = sum.iter().copied().fold(255.0f32, f32::max);
    sum.map(|value| (value * 255.0 / peak).round().clamp(0.0, 255.0) as u8)
}

fn maschine_light(id: u8, rgb: [u8; 3]) -> Vec<u8> {
    let [r, g, b] = rgb.map(|v| (f32::from(v) * 127.0 / 255.0).round() as u8);
    vec![0xf0, 0x7d, 0x44, 0x4b, id, r, g, b, 0xf7]
}

/// Shortcut controls Mixxx adds next to every knob, e.g. `pregain_set_default`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Sub {
    SetDefault,
    SetZero,
    SetOne,
    SetMinusOne,
    Up,
    Down,
    UpSmall,
    DownSmall,
    Toggle,
    MinusToggle,
}

const SUBS: [(&str, Sub); 10] = [
    ("_set_default", Sub::SetDefault),
    ("_set_zero", Sub::SetZero),
    ("_set_one", Sub::SetOne),
    ("_set_minus_one", Sub::SetMinusOne),
    ("_up_small", Sub::UpSmall),
    ("_down_small", Sub::DownSmall),
    ("_up", Sub::Up),
    ("_down", Sub::Down),
    ("_minus_toggle", Sub::MinusToggle),
    ("_toggle", Sub::Toggle),
];

/// Controls whose writes are steps to apply rather than positions: two
/// identical steps arriving between polls must both count, so writes add up
/// and whoever acts on them takes the total and sets it back to 0.
const RELATIVE: [&str; 11] = [
    "jog",
    "jog_bend",
    "beatjump",
    "loop_move",
    "loop_move_loops",
    "MoveVertical",
    "MoveHorizontal",
    "MoveFocus",
    "ScrollVertical",
    "SelectTrackKnob",
    "SelectPlaylist",
];

/// Whether writes to the control named `key` are steps rather than
/// positions (a jog wheel, the browse knob): see [`RELATIVE`].
pub(crate) fn is_relative(key: &str) -> bool {
    RELATIVE.contains(&key)
}

/// One-shot library actions. Each press is counted, so presses arriving
/// faster than the UI polls are not lost; the UI performs them and subtracts
/// what it handled. Only applies to the free controls the UI creates.
const ACTIONS: [&str; 18] = [
    "LoadSelectedTrack",
    "LoadSelectedTrackAndPlay",
    "LoadSelectedIntoFirstStopped",
    "SelectNextTrack",
    "SelectPrevTrack",
    "SelectNextPlaylist",
    "SelectPrevPlaylist",
    "MoveUp",
    "MoveDown",
    "MoveLeft",
    "MoveRight",
    "MoveFocusForward",
    "MoveFocusBackward",
    "GoToItem",
    "AutoDjAddBottom",
    "AutoDjAddTop",
    "ToggleSelectedSidebarItem",
    // openDJ's own: opens the Linked tab of the side panel.
    "ShowLinked",
];

fn is_action(h: &ControlHandle) -> bool {
    h.behavior() == Behavior::Free && ACTIONS.contains(&h.key().item.as_str())
}

#[derive(Clone)]
pub(crate) enum Target {
    Control(ControlHandle),
    Sub(ControlHandle, Sub),
}

impl Target {
    pub(crate) fn get(&self) -> f64 {
        match self {
            Target::Control(h) => h.get(),
            Target::Sub(..) => 0.0,
        }
    }

    pub(crate) fn parameter(&self) -> f64 {
        match self {
            Target::Control(h) => h.get_parameter(),
            Target::Sub(..) => 0.0,
        }
    }

    fn handle(&self) -> &ControlHandle {
        match self {
            Target::Control(h) | Target::Sub(h, _) => h,
        }
    }

    /// The value a 0..1 parameter stands for on this control.
    pub(crate) fn value_for_parameter(&self, p: f64) -> f64 {
        let h = self.handle();
        let (min, max) = h.range();
        let d = h.default_value();
        match h.behavior() {
            Behavior::Free => p,
            Behavior::CenteredOnDefault => {
                let p = p.clamp(0.0, 1.0);
                if p <= 0.5 {
                    min + (d - min) * p * 2.0
                } else {
                    d + (max - d) * (p - 0.5) * 2.0
                }
            }
            _ => min + (max - min) * p.clamp(0.0, 1.0),
        }
    }

    pub(crate) fn parameter_for_value(&self, v: f64) -> f64 {
        let h = self.handle();
        let (min, max) = h.range();
        let d = h.default_value();
        match h.behavior() {
            Behavior::Free => v,
            Behavior::CenteredOnDefault => {
                if v <= d {
                    if d > min {
                        0.5 * (v - min) / (d - min)
                    } else {
                        0.5
                    }
                } else if max > d {
                    0.5 + 0.5 * (v - d) / (max - d)
                } else {
                    0.5
                }
            }
            _ if max > min => (v - min) / (max - min),
            _ => 0.0,
        }
    }

    /// Writes a value as a script would with `engine.setValue`.
    pub(crate) fn set(&self, v: f64) {
        match self {
            Target::Control(h) => {
                if is_action(h) {
                    if v > 0.0 {
                        h.set(h.get() + 1.0);
                    }
                } else if RELATIVE.contains(&h.key().item.as_str()) {
                    h.set(h.get() + v);
                } else {
                    h.set(v);
                }
            }
            Target::Sub(h, sub) => {
                if v > 0.0 {
                    apply_sub(h, *sub);
                }
            }
        }
    }

    pub(crate) fn default_value(&self) -> f64 {
        match self {
            Target::Control(h) => h.default_value(),
            Target::Sub(..) => 0.0,
        }
    }

    pub(crate) fn is_potmeter(&self) -> bool {
        matches!(self, Target::Control(h) if matches!(h.behavior(), Behavior::Linear | Behavior::CenteredOnDefault))
    }

    /// Writes a value arriving from a mapped MIDI message, the way Mixxx's
    /// `setValueFromMidi` does for each kind of control.
    pub(crate) fn set_from_midi(&self, note_off: bool, midi: f64) {
        let pressed = !(note_off || midi == 0.0);
        match self {
            Target::Sub(h, sub) => {
                if pressed {
                    apply_sub(h, *sub);
                }
            }
            Target::Control(h) => match h.behavior() {
                Behavior::Toggle => {
                    if pressed {
                        h.toggle();
                    }
                }
                Behavior::Push => h.set_bool(pressed),
                Behavior::Linear | Behavior::CenteredOnDefault => {
                    h.set_parameter(potmeter_parameter(midi))
                }
                Behavior::Free if is_action(h) => {
                    if pressed {
                        h.set(h.get() + 1.0);
                    }
                }
                Behavior::Free if RELATIVE.contains(&h.key().item.as_str()) => {
                    h.set(h.get() + midi)
                }
                Behavior::Free => h.set(midi / 127.0),
                Behavior::Indicator => h.set_parameter(midi / 127.0),
            },
        }
    }

    /// The control's value as a 0..127 MIDI value, for relative options.
    pub(crate) fn midi_parameter(&self) -> f64 {
        match self {
            Target::Sub(..) => 0.0,
            Target::Control(h) => match h.behavior() {
                Behavior::Linear | Behavior::CenteredOnDefault => {
                    let p = h.get_parameter();
                    if p > 0.5 {
                        p * 126.0 + 1.0
                    } else {
                        p * 128.0
                    }
                }
                Behavior::Free => h.get(),
                _ => h.get_parameter() * 127.0,
            },
        }
    }
}

/// Mixxx's potmeter MIDI curve: 64 is exactly the centre.
fn potmeter_parameter(midi: f64) -> f64 {
    if midi > 64.0 {
        (midi - 1.0) / 126.0
    } else {
        midi / 128.0
    }
}

fn apply_sub(h: &ControlHandle, sub: Sub) {
    let step = |d: f64| h.set_parameter(h.get_parameter() + d);
    match sub {
        Sub::SetDefault => h.reset(),
        Sub::SetZero => h.set(0.0),
        Sub::SetOne => h.set(1.0),
        Sub::SetMinusOne => h.set(-1.0),
        Sub::Up => step(0.1),
        Sub::Down => step(-0.1),
        Sub::UpSmall => step(0.01),
        Sub::DownSmall => step(-0.01),
        Sub::Toggle => h.set(if h.get() > 0.0 { 0.0 } else { 1.0 }),
        Sub::MinusToggle => h.set(if h.get() < 0.0 { 0.0 } else { -1.0 }),
    }
}

/// Mixxx's alpha-beta filter, which turns jog wheel ticks into a smooth
/// playback speed.
#[derive(Default)]
struct AlphaBeta {
    dt: f64,
    x: f64,
    v: f64,
    alpha: f64,
    beta: f64,
}

impl Scratch {
    /// Pulls the smooth filter to its quick twin when they disagree by
    /// more than the twin's wobble: all the way at twice `trust`, not at
    /// all within it.
    fn follow_quick(&mut self) {
        let gap = self.quick.v - self.filter.v;
        let w = ((gap.abs() - self.trust) / self.trust).clamp(0.0, 1.0);
        if w > 0.0 {
            self.filter.v += gap * w;
            self.filter.x += (self.quick.x - self.filter.x) * w;
        }
    }
}

impl AlphaBeta {
    fn init(&mut self, dt: f64, v: f64, alpha: f64, beta: f64) {
        *self = Self {
            dt,
            x: 0.0,
            v,
            alpha,
            beta,
        };
    }

    fn observation(&mut self, dx: f64) {
        let predicted_x = self.x + self.dt * self.v;
        let residual = dx - predicted_x;
        self.x = predicted_x + residual * self.alpha;
        self.v += residual * self.beta / self.dt;
        self.x -= dx;
    }
}

const SCRATCH_DT: f64 = 0.001;
/// openDJ: how much faster the scratch filter's quick twin follows the
/// wheel than the mapping's own filter (alpha times this, beta times its
/// square). With Mixxx's usual 1/8 and 1/256 the deck heard a hand's
/// change of speed about 26 ms late and a fast baby scratch at two thirds
/// of its speed; the twin halves that lag.
const SCRATCH_QUICK: f64 = 3.0;
/// The quick twin's own wobble at normal speed, in multiples of normal
/// speed, times the wheel's ticks per second at normal speed (it wobbles
/// less the finer the wheel). Measured with the latency probe: 4% on the
/// DDJ-FLX4's 400 ticks a second.
const SCRATCH_QUICK_NOISE: f64 = 16.0;
/// The twins have to disagree by this many times that wobble before the
/// smooth filter is pulled to the quick one, so a steady hand still plays
/// as smoothly as before.
const SCRATCH_TRUST: f64 = 2.0;
const BRAKE_RAMP_TO: f64 = 0.01;
const MAX_DECKS: usize = 8;
/// Deck controls a controller holds on while a button is down, let go of
/// when the controller goes away (see [`Host::release_decks`]).
const HELD_KEYS: [&str; 6] = [
    "reverse",
    "reverseroll",
    "rate_temp_up",
    "rate_temp_down",
    "rate_temp_up_small",
    "rate_temp_down_small",
];

#[derive(Default)]
struct Scratch {
    /// Running at 1 kHz (Mixxx's scratch timer).
    active: bool,
    /// This controller turned the deck's `scratch2_enable` on and hasn't
    /// turned it off yet (see [`Host::release_decks`]).
    held: bool,
    dx: f64,
    accumulator: f64,
    ramp: bool,
    ramp_to: f64,
    brake: bool,
    spinback: bool,
    soft_start: bool,
    last_movement: Option<Instant>,
    filter: AlphaBeta,
    /// openDJ: a quicker filter on the same ticks. While the hand changes
    /// speed faster than the smooth filter follows, the smooth one is
    /// pulled to it (see [`SCRATCH_QUICK`]).
    quick: AlphaBeta,
    /// How far apart the twins may drift (speed, multiples of normal)
    /// before the quick one leads: [`SCRATCH_TRUST`] times its wobble.
    trust: f64,
}

struct Watch {
    group: String,
    key: String,
    control: ControlHandle,
    version: u32,
}

/// `[Controller],deck3` and `deck4`: while on, a two-deck controller's left
/// side plays deck 3 instead of 1, and its right side deck 4 instead of 2,
/// as a deck select button does on four-deck controllers.
pub const LAYER_GROUP: &str = "[Controller]";
pub const LAYER_KEYS: [&str; 2] = ["deck3", "deck4"];

struct Timer {
    due: Instant,
    interval: Duration,
    one_shot: bool,
}

pub(crate) struct Host {
    pub(crate) registry: Arc<ControlRegistry>,
    pub(crate) name: String,
    /// Script errors, kept for the UI and tests.
    pub(crate) errors: Vec<String>,
    /// MIDI messages waiting to be sent.
    pub(crate) sent: Vec<Vec<u8>>,
    watches: Vec<Option<Watch>>,
    /// Controls `engine.trigger` named since the last poll. As in Mixxx, their
    /// connections run later, not inside the script call that triggered them.
    triggered: Vec<ConfigKey>,
    /// Messages bound to a script function in the mapping XML; Mixxx gives
    /// these no `midi.makeInputHandler` callbacks.
    pub(crate) script_bound: HashSet<(u8, u8)>,
    /// How many `midi.makeInputHandler` callbacks each message has.
    input_handlers: HashMap<(u8, u8), u32>,
    timers: BTreeMap<u32, Timer>,
    next_timer: u32,
    soft: HashMap<(String, String), Takeover>,
    /// The message being handled, while a controller's MIDI is: writes it
    /// makes are the hardware moving.
    pub(crate) input: Option<(u8, u8)>,
    /// Whether hardware positions are shown on screen (a real controller,
    /// not the Controller Editor's probe).
    pub(crate) publish: bool,
    /// Messages the mapping also sends back, as motorised faders and LED
    /// rings do: the hardware follows the screen, so needs no marker.
    followed: HashSet<(u8, u8)>,
    scratch: Vec<Scratch>,
    scratch_tick: Instant,
    /// The deck 3 and 4 switches, for a mapping written for two decks; a
    /// mapping that names decks 3 or 4 itself has none.
    layers: Option<[ControlHandle; 2]>,
    /// The switches and deck shift as the watches last resolved them.
    layers_seen: RouteState,
    /// The deck this device plays (see [`crate::routing`]): its choice,
    /// shared with Settings, and the decks it can follow.
    route: Option<(Arc<std::sync::atomic::AtomicU8>, crate::routing::DeckWatch)>,
    /// How many decks along this device's decks are moved now.
    pub(crate) shift: usize,
    /// While set, every control a script writes and the value, by the name
    /// it acts on (`[Channel1],pregain_set_default` stays as written; layers
    /// and `[Main]` are resolved). Writes soft takeover drops still count.
    /// For the Controller Editor, which wants to know what a script does.
    pub(crate) trace: Option<Vec<(ConfigKey, f64)>>,
    /// openDJ: notes a script moved with `midi.routeNote`, by MIDI channel:
    /// a pad pressed on the device acts as the note it is routed to, and that
    /// note's lights show on it. For pad mode buttons that open a pad page
    /// by place, whatever page is in it.
    note_routes: HashMap<u8, [u8; 128]>,
    /// The last value sent to each note, by status and note, to show on a
    /// pad newly routed to it.
    note_values: HashMap<(u8, u8), u8>,
    routes_changed: bool,
    /// An HID controller's device and the output reports for it.
    pub(crate) hid: Option<HidLink>,
    pub(crate) maschine_source: MaschineSource,
    maschine_surfaces: HashMap<usize, MaschineSurface>,
}

/// What an HID mapping's `controller` object talks to.
pub(crate) struct HidLink {
    pub(crate) io: std::rc::Rc<dyn crate::hid::Io>,
    pub(crate) out: crate::hid::Outputs,
}

/// The deck 3 and 4 switches and the deck shift (see [`Host::route_state`]).
pub(crate) type RouteState = ([bool; 2], usize);

pub(crate) fn deck_group(deck: usize) -> String {
    format!("[Channel{deck}]")
}

impl Host {
    pub(crate) fn new(registry: Arc<ControlRegistry>, name: String) -> Self {
        Self {
            registry,
            name,
            errors: Vec::new(),
            sent: Vec::new(),
            note_routes: HashMap::new(),
            note_values: HashMap::new(),
            routes_changed: false,
            hid: None,
            maschine_source: Default::default(),
            maschine_surfaces: HashMap::new(),
            watches: Vec::new(),
            triggered: Vec::new(),
            script_bound: HashSet::new(),
            input_handlers: HashMap::new(),
            timers: BTreeMap::new(),
            next_timer: 1,
            soft: HashMap::new(),
            input: None,
            publish: false,
            followed: HashSet::new(),
            scratch: (0..=MAX_DECKS).map(|_| Scratch::default()).collect(),
            scratch_tick: Instant::now(),
            layers: None,
            layers_seen: ([false; 2], 0),
            route: None,
            shift: 0,
            trace: None,
        }
    }

    /// Runs DECKARD's MIDI device script on this mapping's existing thread.
    /// Graphics return to JavaScript so the MK2's acknowledged transport can
    /// atomically commit a complete display frame.
    pub(crate) fn maschine(&mut self, deck: usize, command: &str, bytes: &[u8]) -> String {
        let source = self.maschine_source.read().ok().and_then(|s| s.clone());
        let Some(source) = source else {
            return "{\"available\":false,\"midi\":[]}".into();
        };
        let Some(mut view) = source.view(deck) else {
            return "{\"available\":false,\"midi\":[]}".into();
        };
        let counts = encoder_response::counts(&self.registry, false);
        MaschineSurface::sampling_response(&mut view, counts);
        let now = Instant::now();
        let maschine_group = format!("[Maschine{deck}]");
        let keyboard = self
            .registry
            .get(&maschine_group, "keyboard")
            .is_some_and(|c| c.get_bool());
        if let Some(group) = self.registry.get(&maschine_group, "group") {
            view.group = group.get().round().clamp(0.0, 7.0) as usize;
        }
        if let Some(sound) = self.registry.get(&maschine_group, "sound") {
            view.sound = sound.get().round().clamp(0.0, 15.0) as usize;
        }
        let quantize = self
            .registry
            .get(&maschine_group, "quantize")
            .is_some_and(|c| c.get_bool());
        let surface = self
            .maschine_surfaces
            .entry(deck)
            .or_insert_with(MaschineSurface::new);
        let step = view.grid.step_ticks.max(1);
        if view.follow && view.playing {
            if let Some(at) = view.pattern_position {
                surface.step_page =
                    (at.saturating_sub(view.pattern_start).max(0) / (step * 16)) as usize;
            }
        }
        surface.step_page = surface.step_page.min(
            (view.pattern_length.max(1) as usize)
                .div_ceil((step * 16) as usize)
                .saturating_sub(1),
        );
        let opening_browser = command == "input"
            && bytes.len() == 3
            && bytes[0] == 0xb0
            && bytes[1] == 8
            && bytes[2] > 0;
        if !surface.browsing && !opening_browser {
            view.browser = Default::default();
        }
        let mut actions = Vec::new();
        let mut output = Vec::new();
        let mut overridden = None;
        let events = match command {
            "open" => surface.device.open(&view, now),
            "input" => {
                if bytes.len() == 3 && bytes[0] & 0xf0 == 0xb0 {
                    if bytes[2] > 0 {
                        surface.physical.insert(bytes[1]);
                        surface.held.insert(bytes[1]);
                    } else {
                        surface.physical.remove(&bytes[1]);
                        if !surface.pinned.contains(&bytes[1]) {
                            surface.held.remove(&bytes[1]);
                        }
                    }
                }
                overridden = surface.gesture(bytes, &view, keyboard, now, &self.registry);
                if bytes.len() == 3
                    && bytes[0] & 0xf0 == 0xb0
                    && bytes[2] == 0
                    && surface.pinned.contains(&bytes[1])
                {
                    Vec::new()
                } else {
                    let mut events = surface.input(bytes, &view, now);
                    for mode in [10, 4, 27, 30, 31, 33] {
                        if surface.held.contains(&mode)
                            && !surface.pinned.contains(&mode)
                            && !surface.physical.contains(&mode)
                        {
                            surface.held.remove(&mode);
                            events.extend(surface.device.midi(
                                "controls",
                                &[0xb0, mode, 0],
                                &view,
                                now,
                            ));
                            if mode == 10 {
                                overridden
                                    .get_or_insert_with(Vec::new)
                                    .push(deckard_surface::Action::Repeat("on", 0.0));
                            }
                        }
                    }
                    events
                }
            }
            "tick" => surface.device.tick(&view, now),
            "repaint" => {
                let mut events =
                    surface
                        .device
                        .midi("controls", &[0xf0, 0x7d, 0x44, 0x49, 0xf7], &view, now);
                events.extend(surface.device.tick(&view, now));
                events
            }
            "close" => surface.device.close(),
            _ => Vec::new(),
        };
        for event in events {
            match event {
                deckard_surface::device::Out::App(mut action) => {
                    action = MaschineSurface::angle_action(action, &view, counts);
                    if let deckard_surface::Action::Sampling(on) = action {
                        surface.sampling = on;
                    }
                    if let deckard_surface::Action::Library(on) = action {
                        surface.browsing = on;
                    }
                    if overridden.is_none() {
                        let held_group = (80..=87)
                            .find(|key| surface.held.contains(key))
                            .map(|key| usize::from(key - 80));
                        let contextual = matches!(
                            action,
                            deckard_surface::Action::Volume(_)
                                | deckard_surface::Action::Swing(_)
                                | deckard_surface::Action::Tempo(_)
                        ) && (held_group.is_some() || surface.held.contains(&30));
                        actions.push(if contextual {
                            source.contextual_wheel(
                                deck,
                                action,
                                held_group.unwrap_or(view.group),
                                held_group.is_none().then_some(view.sound),
                            )
                        } else {
                            action
                        });
                    }
                }
                deckard_surface::device::Out::Midi { role, bytes } if role == "controls" => {
                    if bytes.len() == 9 && bytes[..4] == [0xf0, 0x7d, 0x44, 0x4b] {
                        let id = bytes[4];
                        if id == 64 {
                            surface.mode = bytes[5];
                        }
                        // Group emitters use the actual sound meters below.
                        if id < 8 || (65..=72).contains(&id) {
                            continue;
                        }
                    }
                    output.push(bytes);
                }
                deckard_surface::device::Out::Message(_) => {}
                _ => {}
            }
        }
        if let Some(overridden) = overridden {
            actions.extend(overridden);
        }
        if command == "tick" && view.playing {
            actions.extend(surface.erase_sounds.values().map(|target| {
                deckard_surface::Action::Events("erase_sound_window", *target as i64)
            }));
            actions.extend(surface.erase_groups.iter().map(|target| {
                deckard_surface::Action::Events("erase_group_window", *target as i64)
            }));
        }
        let mut pad_value = None;
        if command == "input" && bytes.len() == 3 && (36..=51).contains(&bytes[1]) {
            let status = bytes[0] & 0xf0;
            let pad = bytes[1] - 35;
            if status == 0x80 || (status == 0x90 && bytes[2] == 0) {
                if surface.pads.remove(&pad) {
                    pad_value = Some((pad, 0.0));
                }
            } else if status == 0x90
                && surface.mode == 0
                && !surface.sampling
                && ![2, 29, 30, 31, 33, 49]
                    .iter()
                    .any(|k| surface.held.contains(k))
            {
                surface.pads.insert(pad);
                let velocity = if view.fixed_velocity {
                    view.velocity.clamp(1, 127) as u8
                } else {
                    bytes[2]
                };
                pad_value = Some((pad, f64::from(velocity) / 127.0));
            }
        }
        let error = surface.device.error().or_else(|| source.error(deck));
        surface.copy_feedback(&actions, &view, now);
        if error.is_some() {
            surface.duplicate_sent = None;
        }
        let owned_repeat = surface.held.contains(&10);
        let overlay = surface.overlay(&view, keyboard, quantize);
        let pattern_state = surface.pattern_state(&view);
        if keyboard && surface.mode == 0 && !surface.sampling {
            if let Some(sound) = view
                .groups
                .get(view.group)
                .and_then(|g| g.sounds.get(view.sound))
            {
                for pad in 1..=16 {
                    let level = if surface.pads.contains(&pad) {
                        1.0
                    } else {
                        0.25
                    };
                    output.push(maschine_light(
                        47 + pad,
                        sound.rgb.map(|v| (f64::from(v) * level).round() as u8),
                    ));
                }
            }
        }
        let group = format!("[Maschine{deck}]");
        for action in &actions {
            let target = match action {
                deckard_surface::Action::FocusGroup(n) => Some(("group", *n as f64)),
                deckard_surface::Action::FocusSound(n) => Some(("sound", *n as f64)),
                deckard_surface::Action::KeyboardMode(on) => {
                    Some(("keyboard", if *on { 1.0 } else { 0.0 }))
                }
                _ => None,
            };
            if let Some((key, value)) = target {
                if let Some(control) = self.registry.get(&group, key) {
                    control.set(value);
                }
            }
        }
        actions.retain(|action| {
            !matches!(
                action,
                deckard_surface::Action::FocusGroup(_)
                    | deckard_surface::Action::FocusSound(_)
                    | deckard_surface::Action::KeyboardMode(_)
            )
        });
        if command == "close" {
            if owned_repeat {
                if let Some(control) = self.registry.get(&group, "note_repeat") {
                    control.set(0.0);
                }
            }
            for pad in 1..=16 {
                if let Some(control) = self.registry.get(&group, &format!("pad{pad}")) {
                    control.set(0.0);
                }
            }
            self.maschine_surfaces.remove(&deck);
        } else {
            if let Some((pad, value)) = pad_value {
                if value > 0.0 {
                    if let Some(velocity) = self.registry.get(&group, &format!("pad{pad}_velocity"))
                    {
                        velocity.set(value);
                    }
                }
                if let Some(control) = self.registry.get(&group, &format!("pad{pad}")) {
                    control.set(value);
                }
            }
            for (index, group) in view.groups.iter().take(8).enumerate() {
                output.push(maschine_light(index as u8, group.rgb));
                output.push(maschine_light(65 + index as u8, maschine_group_rgb(group)));
            }
            let feedback = surface.feedback_lights(&view, now, &output);
            output.extend(feedback);
        }
        if !actions.is_empty() {
            source.dispatch(deck, actions);
        }
        serde_json::json!({"available":true, "midi":output, "error":error,"overlay":overlay,"pattern_state":pattern_state})
            .to_string()
    }

    /// Lets the deck 3 and 4 switches redirect this controller's decks.
    pub(crate) fn enable_layers(&mut self) {
        self.layers = Some(LAYER_KEYS.map(|k| self.registry.toggle(LAYER_GROUP, k)));
    }

    /// Which of the deck 3 and 4 switches are on.
    pub(crate) fn layer_state(&self) -> [bool; 2] {
        match &self.layers {
            Some(l) => [l[0].get_bool(), l[1].get_bool()],
            None => [false; 2],
        }
    }

    /// Lets the device's deck choice (Follow, a deck) move its decks.
    pub(crate) fn set_route(&mut self, route: Arc<std::sync::atomic::AtomicU8>) {
        let watch = crate::routing::DeckWatch::new(&self.registry);
        self.route = Some((route, watch));
        self.refresh_shift();
    }

    /// Works out again how far this device's decks move: the instrument it
    /// follows may have moved to another deck.
    pub(crate) fn refresh_shift(&mut self) {
        self.shift = match &self.route {
            Some((route, watch)) => {
                let choice = crate::routing::DeckChoice::from_u8(
                    route.load(std::sync::atomic::Ordering::Relaxed),
                );
                watch.shift(choice, false)
            }
            None => 0,
        };
    }

    /// The deck 3 and 4 switches and the deck shift: what moves this
    /// device's controls to other decks.
    pub(crate) fn route_state(&self) -> RouteState {
        (self.layer_state(), self.shift)
    }

    /// `group` with deck 1 or 2 swapped for 3 or 4 when that side is
    /// switched over (`[Channel1]`, its EQ and filter racks and its stems),
    /// then moved along to the deck the device follows.
    fn layered<'a>(&self, group: &'a str) -> std::borrow::Cow<'a, str> {
        let out = self.switched(group);
        if let std::borrow::Cow::Owned(s) = crate::routing::shift_group(&out, self.shift) {
            return std::borrow::Cow::Owned(s);
        }
        out
    }

    fn switched<'a>(&self, group: &'a str) -> std::borrow::Cow<'a, str> {
        let mut out = std::borrow::Cow::Borrowed(group);
        for (side, on) in self.layer_state().into_iter().enumerate() {
            if !on {
                continue;
            }
            let (from, to) = (side + 1, side + 3);
            for end in ["]", "_"] {
                let a = format!("[Channel{from}{end}");
                if out.contains(&a) {
                    out = std::borrow::Cow::Owned(out.replace(&a, &format!("[Channel{to}{end}")));
                }
            }
        }
        out
    }

    pub(crate) fn log(&self, message: &str) {
        eprintln!("[{}] {message}", self.name);
    }

    pub(crate) fn error(&mut self, message: &str) {
        eprintln!("[{}] error: {message}", self.name);
        if self.errors.len() < 100 {
            self.errors.push(message.to_string());
        }
    }

    /// Finds a control by name. Shortcut names (`rate_set_default`) resolve
    /// to their base control; names nothing implements become free controls
    /// so scripts can still keep state in them, as they do in Mixxx.
    pub(crate) fn resolve(&self, group: &str, key: &str) -> Target {
        let group = self.layered(group);
        let group = match group.as_ref() {
            "[Main]" => "[Master]",
            g => g,
        };
        if let Some((g, k)) = self.mixer_knob(group, key) {
            if let Some(h) = self.registry.get(&g, k) {
                return Target::Control(h);
            }
        }
        if let Some(h) = self.registry.get(group, key) {
            return Target::Control(h);
        }
        for (suffix, sub) in SUBS {
            if let Some(base) = key.strip_suffix(suffix) {
                if let Some(h) = self.registry.get(group, base) {
                    return Target::Sub(h, sub);
                }
            }
        }
        Target::Control(self.registry.free(group, key))
    }

    /// Where an EQ or filter knob goes while the mixer's side knobs are
    /// switched to stems or sculpt (`[Mixer],knob_mode` 1 or 2): the knob in
    /// the same place on screen, so a controller's high, mid, low and filter
    /// knobs work whatever the mixer shows.
    fn mixer_knob(&self, group: &str, key: &str) -> Option<(String, &'static str)> {
        let place = if let Some(rest) = group.strip_prefix("[EqualizerRack1_[Channel") {
            let deck = rest.strip_suffix("]_Effect1]")?;
            let place = match key {
                "parameter3" => 0,
                "parameter2" => 1,
                "parameter1" => 2,
                _ => return None,
            };
            (deck, place)
        } else {
            let rest = group.strip_prefix("[QuickEffectRack1_[Channel")?;
            if key != "super1" {
                return None;
            }
            (rest.strip_suffix("]]")?, 3)
        };
        let (deck, place) = place;
        deck.parse::<usize>().ok()?;
        let mode = self.registry.get("[Mixer]", "knob_mode")?.get();
        if mode == 1.0 {
            // Top to bottom: hi-hats, vocals, kick and bass, as on screen.
            let stem = [5, 4, 1, 2][place];
            Some((format!("[Channel{deck}_Stem{stem}]"), "volume"))
        } else if mode == 2.0 {
            let key = ["lpf", "freq", "gain", "hpf"][place];
            Some((format!("[Sculpt_[Channel{deck}]]"), key))
        } else {
            None
        }
    }

    /// Script writes. Values for knobs with soft takeover enabled may be
    /// dropped, or scaled.
    pub(crate) fn set_value(&mut self, group: &str, key: &str, v: f64) {
        let t = self.resolve(group, key);
        self.record(key, &t, v);
        match self.soft_take(group, key, &t, t.parameter_for_value(v)) {
            Some(p) => t.set(t.value_for_parameter(p)),
            None if self.soft_has(group, key) => {}
            None => t.set(v),
        }
    }

    pub(crate) fn set_parameter(&mut self, group: &str, key: &str, p: f64) {
        let t = self.resolve(group, key);
        self.record(key, &t, t.value_for_parameter(p));
        match self.soft_take(group, key, &t, p) {
            Some(p) => t.set(t.value_for_parameter(p)),
            None if self.soft_has(group, key) => {}
            None => t.set(t.value_for_parameter(p)),
        }
    }

    /// Notes a script write while tracing; `key` as the script named it.
    pub(crate) fn record(&mut self, key: &str, t: &Target, value: f64) {
        if let Some(trace) = &mut self.trace {
            let k = match t {
                Target::Control(h) => h.key().clone(),
                Target::Sub(h, _) => ConfigKey::new(h.key().group.clone(), key),
            };
            match trace.iter_mut().find(|(t, _)| *t == k) {
                // A press that writes 1 then 0 is still "1".
                Some((_, v)) if value != 0.0 => *v = value,
                Some(_) => {}
                None => trace.push((k, value)),
            }
        }
    }

    pub(crate) fn soft_has(&self, group: &str, key: &str) -> bool {
        self.soft
            .contains_key(&(group.to_string(), key.to_string()))
    }

    /// Soft takeover of a write of parameter `p`: the parameter to set, or
    /// `None` when it is dropped or the control has no soft takeover.
    pub(crate) fn soft_take(&mut self, group: &str, key: &str, t: &Target, p: f64) -> Option<f64> {
        let input = self.input;
        let publish = self.publish;
        let scale = pickup::scale(&crate::device_key(&self.name));
        let followed = input.is_some_and(|i| self.followed.contains(&i));
        let st = self.soft.get_mut(&(group.to_string(), key.to_string()))?;
        let out = st.take(t.parameter(), p, scale);
        if let (Some(i), Target::Control(h)) = (input, t) {
            st.source = Some(i);
            let k = h.key().clone();
            if publish && !followed {
                if let Some(old) = st.moved.as_ref().filter(|old| **old != k) {
                    pickup::forget(old, &self.name);
                }
                pickup::note(&k, p, &self.name);
            }
            st.moved = Some(k);
        }
        out
    }

    pub(crate) fn soft_takeover(&mut self, group: &str, key: &str, enable: bool) {
        let k = (group.to_string(), key.to_string());
        if !enable {
            if let Some(old) = self.soft.remove(&k).and_then(|st| st.moved) {
                pickup::forget(&old, &self.name);
            }
            return;
        }
        if self.soft.contains_key(&k) {
            return;
        }
        let t = self.resolve(group, key);
        if let Target::Control(h) = &t {
            if t.is_potmeter() && !pickup::always_jumps(h.key()) {
                self.soft.insert(k, Takeover::new());
            }
        }
    }

    pub(crate) fn soft_takeover_ignore_next(&mut self, group: &str, key: &str) {
        if let Some(st) = self.soft.get_mut(&(group.to_string(), key.to_string())) {
            st.ignore_next = true;
        }
    }

    pub(crate) fn soft_takeover_will_ignore(&self, group: &str, key: &str, p: f64) -> bool {
        match self.soft.get(&(group.to_string(), key.to_string())) {
            Some(st) => st.will_ignore(self.resolve(group, key).parameter(), p, Instant::now()),
            None => false,
        }
    }

    /// The mapping sent `msg`: a knob or fader it moves is driven from here
    /// too (motorised, or an LED ring), so the hardware is where the screen
    /// is and needs no marker.
    pub(crate) fn sent_back(&mut self, msg: &[u8]) {
        let [status, control, _] = *msg else { return };
        if status & 0xF0 != 0xB0 || self.followed.contains(&(status, control)) {
            return;
        }
        let moved: Vec<ConfigKey> = self
            .soft
            .values()
            .filter(|st| st.source == Some((status, control)))
            .filter_map(|st| st.moved.clone())
            .collect();
        if moved.is_empty() {
            return;
        }
        self.followed.insert((status, control));
        for k in moved {
            pickup::forget(&k, &self.name);
        }
    }

    /// A deck switch moved the hardware over to other controls: its markers
    /// move with it.
    pub(crate) fn layers_moved(&mut self) {
        if !self.publish {
            return;
        }
        let names: Vec<(String, String)> = self.soft.keys().cloned().collect();
        for (group, key) in names {
            let Target::Control(h) = self.resolve(&group, &key) else {
                continue;
            };
            let k = h.key().clone();
            let st = self.soft.get_mut(&(group, key)).unwrap();
            let (Some(source), Some(old), Some(p)) = (st.source, st.moved.clone(), st.hardware())
            else {
                continue;
            };
            if old == k || self.followed.contains(&source) {
                continue;
            }
            pickup::forget(&old, &self.name);
            pickup::note(&k, p, &self.name);
            st.moved = Some(k);
        }
    }

    pub(crate) fn watch(&mut self, group: &str, key: &str) -> u32 {
        let control = match self.resolve(group, key) {
            Target::Control(h) | Target::Sub(h, _) => h,
        };
        let version = control.version();
        self.watches.push(Some(Watch {
            group: group.to_string(),
            key: key.to_string(),
            control,
            version,
        }));
        self.watches.len() as u32
    }

    pub(crate) fn unwatch(&mut self, id: u32) {
        if let Some(w) = self.watches.get_mut(id.wrapping_sub(1) as usize) {
            *w = None;
        }
    }

    pub(crate) fn add_input_handlers(&mut self, status: u8, control: u8, delta: i32) {
        let n = self.input_handlers.entry((status, control)).or_insert(0);
        *n = n.saturating_add_signed(delta);
        if *n == 0 {
            self.input_handlers.remove(&(status, control));
        }
    }

    pub(crate) fn has_input_handlers(&self, status: u8, control: u8) -> bool {
        self.input_handlers.contains_key(&(status, control))
    }

    /// Queues the connections to a control to run on the next poll.
    pub(crate) fn trigger(&mut self, group: &str, key: &str) {
        let key = match self.resolve(group, key) {
            Target::Control(h) | Target::Sub(h, _) => h.key().clone(),
        };
        if !self.triggered.contains(&key) {
            self.triggered.push(key);
        }
    }

    /// Connections whose control changed or was triggered since the last
    /// call, with values.
    pub(crate) fn changed_watches(&mut self) -> Vec<(u32, f64)> {
        let triggered = std::mem::take(&mut self.triggered);
        // A side switched decks: every connection now follows the other
        // deck, and fires so the LEDs show it.
        let layers = self.route_state();
        let switched = layers != self.layers_seen;
        if switched {
            self.layers_seen = layers;
            let resolved: Vec<Option<ControlHandle>> = self
                .watches
                .iter()
                .map(|w| {
                    w.as_ref().map(|w| match self.resolve(&w.group, &w.key) {
                        Target::Control(h) | Target::Sub(h, _) => h,
                    })
                })
                .collect();
            for (w, c) in self.watches.iter_mut().zip(resolved) {
                if let (Some(w), Some(c)) = (w, c) {
                    w.control = c;
                }
            }
        }
        let mut out = Vec::new();
        for (i, w) in self.watches.iter_mut().enumerate() {
            if let Some(w) = w {
                let v = w.control.version();
                if switched || v != w.version || triggered.contains(w.control.key()) {
                    w.version = v;
                    out.push((i as u32 + 1, w.control.get()));
                }
            }
        }
        out
    }

    pub(crate) fn timer_start(&mut self, ms: f64, one_shot: bool) -> u32 {
        let id = self.next_timer;
        self.next_timer += 1;
        // Mixxx takes the interval as a 32-bit int and raises it to 20 ms;
        // Infinity converts to 0 there, so it gets 20 ms too.
        let ms = if ms.is_finite() {
            ms.clamp(20.0, i32::MAX as f64)
        } else {
            20.0
        };
        let interval = Duration::from_secs_f64(ms / 1000.0);
        self.timers.insert(
            id,
            Timer {
                due: Instant::now() + interval,
                interval,
                one_shot,
            },
        );
        id
    }

    pub(crate) fn timer_stop(&mut self, id: u32) {
        self.timers.remove(&id);
    }

    /// Timers that are due, rescheduling repeating ones.
    pub(crate) fn due_timers(&mut self, now: Instant) -> Vec<u32> {
        let due: Vec<u32> = self
            .timers
            .iter()
            .filter(|(_, t)| t.due <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in &due {
            let t = self.timers.get_mut(id).unwrap();
            if t.one_shot {
                self.timers.remove(id);
            } else {
                t.due += t.interval;
                if t.due < now {
                    t.due = now + t.interval;
                }
            }
        }
        due
    }

    /// When the controller next needs attention.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let timer = self.timers.values().map(|t| t.due).min();
        let scratch = self
            .scratch
            .iter()
            .any(|s| s.active)
            .then(|| self.scratch_tick + Duration::from_millis(1));
        match (timer, scratch) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn control(&self, deck: usize, key: &str) -> ControlHandle {
        match self.resolve(&deck_group(deck), key) {
            Target::Control(h) | Target::Sub(h, _) => h,
        }
    }

    fn deck_rate(&self, deck: usize) -> f64 {
        let dir = if self.control(deck, "rate_dir").get() < 0.0 {
            -1.0
        } else {
            1.0
        };
        let rate =
            1.0 + self.control(deck, "rate").get() * self.control(deck, "rateRange").get() * dir;
        if self.control(deck, "reverse").get_bool() {
            -rate
        } else {
            rate
        }
    }

    fn deck_ok(deck: f64) -> Option<usize> {
        (deck >= 1.0 && deck <= MAX_DECKS as f64).then_some(deck as usize)
    }

    pub(crate) fn scratch_enable(
        &mut self,
        deck: f64,
        intervals_per_rev: f64,
        rpm: f64,
        alpha: f64,
        beta: f64,
        ramp: bool,
    ) {
        let Some(d) = Self::deck_ok(deck) else { return };
        let per_second = rpm * intervals_per_rev / 60.0;
        if per_second == 0.0 {
            self.log("Invalid rpm or intervalsPerRev supplied to scratchEnable");
            return;
        }
        let enable = self.control(d, "scratch2_enable");
        let mut v0 = 0.0;
        if ramp {
            if enable.get_bool() {
                v0 = self.control(d, "scratch2").get();
            } else if self.control(d, "play").get_bool() {
                v0 = self.deck_rate(d);
            }
        }
        let s = &mut self.scratch[d];
        s.dx = 1.0 / per_second;
        s.accumulator = 0.0;
        s.ramp = false;
        s.brake = false;
        let (alpha, beta) = if alpha != 0.0 && beta != 0.0 {
            (alpha, beta)
        } else {
            (1.0 / 8.0, (1.0 / 8.0) / 32.0)
        };
        s.filter.init(SCRATCH_DT, v0, alpha, beta);
        s.quick.init(
            SCRATCH_DT,
            v0,
            (alpha * SCRATCH_QUICK).min(1.0),
            (beta * SCRATCH_QUICK * SCRATCH_QUICK).min(0.5),
        );
        s.trust = SCRATCH_TRUST * SCRATCH_QUICK_NOISE / per_second;
        s.held = true;
        self.start_scratch_timer(d);
        enable.set_bool(true);
    }

    fn start_scratch_timer(&mut self, d: usize) {
        if !self.scratch.iter().any(|s| s.active) {
            self.scratch_tick = Instant::now();
        }
        self.scratch[d].active = true;
    }

    pub(crate) fn scratch_tick(&mut self, deck: f64, interval: f64) {
        let Some(d) = Self::deck_ok(deck) else { return };
        let s = &mut self.scratch[d];
        s.last_movement = Some(Instant::now());
        s.accumulator += interval;
    }

    pub(crate) fn scratch_disable(&mut self, deck: f64, ramp: bool) {
        let Some(d) = Self::deck_ok(deck) else { return };
        let mut ramp_to = 0.0;
        if !ramp {
            self.control(d, "scratch2_enable").set_bool(false);
            self.scratch[d].held = false;
        } else if self.control(d, "play").get_bool() {
            ramp_to = self.deck_rate(d);
        }
        let s = &mut self.scratch[d];
        s.ramp_to = ramp_to;
        s.last_movement = Some(Instant::now());
        s.ramp = true;
    }

    pub(crate) fn is_scratching(&self, deck: f64) -> bool {
        Self::deck_ok(deck).is_some_and(|d| self.control(d, "scratch2_enable").get_bool())
    }

    /// `engine.brake`, `engine.spinback` and `engine.softStart`, which Mixxx
    /// runs through the same filter as scratching.
    pub(crate) fn ramp(&mut self, deck: f64, kind: &str, activate: bool, factor: f64, rate: f64) {
        let Some(d) = Self::deck_ok(deck) else { return };
        let mut factor = factor;
        if factor > 1.0 {
            factor = (factor - 1.0) / 10.0 + 1.0;
        }
        let (alpha, beta) = (1.0 / 512.0, (1.0 / 512.0) / 1024.0 * factor);
        let enable = self.control(d, "scratch2_enable");
        self.scratch[d].held = activate;
        if kind == "softStart" {
            enable.set_bool(activate);
            let mut v0 = 0.0;
            let ramp_to = self.deck_rate(d);
            let s = &mut self.scratch[d];
            s.soft_start = activate;
            s.active = false;
            if !activate {
                return;
            }
            if s.brake || s.spinback {
                s.brake = false;
                s.spinback = false;
                v0 = s.filter.v;
            }
            s.ramp_to = ramp_to;
            s.filter.init(SCRATCH_DT, v0, alpha, beta);
            s.trust = 0.0;
            s.ramp = true;
            self.control(d, "play").set_bool(true);
            self.control(d, "scratch2").set(v0);
            self.start_scratch_timer(d);
            return;
        }
        enable.set_bool(activate);
        if !activate {
            let s = &mut self.scratch[d];
            s.brake = false;
            s.spinback = false;
            s.active = false;
            return;
        }
        let rate = if kind == "spinback" { rate } else { rate.abs() };
        let v0 = if rate < -BRAKE_RAMP_TO {
            let s = &mut self.scratch[d];
            s.spinback = true;
            s.brake = false;
            s.ramp_to = -BRAKE_RAMP_TO;
            rate
        } else if rate > BRAKE_RAMP_TO {
            let deck_rate = self.deck_rate(d);
            let s = &mut self.scratch[d];
            if s.spinback || s.brake {
                return;
            }
            s.brake = true;
            s.spinback = false;
            s.ramp_to = BRAKE_RAMP_TO;
            if s.soft_start {
                s.soft_start = false;
                s.filter.v
            } else {
                deck_rate
            }
        } else {
            let s = &mut self.scratch[d];
            s.brake = false;
            s.spinback = false;
            s.active = false;
            self.control(d, "play").set_bool(false);
            return;
        };
        self.control(d, "scratch2").set(v0);
        let s = &mut self.scratch[d];
        s.filter.init(SCRATCH_DT, v0, alpha, beta);
        s.trust = 0.0;
        s.ramp = true;
        self.start_scratch_timer(d);
    }

    pub(crate) fn ramp_active(&self, deck: f64, kind: &str) -> bool {
        Self::deck_ok(deck).is_some_and(|d| {
            let s = &self.scratch[d];
            match kind {
                "brake" => s.brake,
                "spinback" => s.spinback,
                _ => s.soft_start,
            }
        })
    }

    /// Runs the 1 kHz scratch filters for the time that has passed.
    pub(crate) fn process_scratch(&mut self, now: Instant) {
        if !self.scratch.iter().any(|s| s.active) {
            self.scratch_tick = now;
            return;
        }
        let elapsed = now.saturating_duration_since(self.scratch_tick);
        let ticks = (elapsed.as_secs_f64() / SCRATCH_DT) as u32;
        if ticks == 0 {
            return;
        }
        self.scratch_tick += Duration::from_secs_f64(ticks as f64 * SCRATCH_DT);
        // After a stall, catch up a little but don't spin for long.
        for _ in 0..ticks.min(50) {
            for d in 1..=MAX_DECKS {
                if self.scratch[d].active {
                    self.scratch_step(d, now);
                }
            }
        }
    }

    fn scratch_step(&mut self, d: usize, now: Instant) {
        let playing = self.control(d, "play").get_bool();
        let loaded = self.control(d, "track_loaded").get_bool();
        let s = &mut self.scratch[d];
        let idle = s
            .last_movement
            .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_millis(1));
        // Ramping after the wheel was let go, or a soft start: steer towards
        // the target speed; otherwise follow the wheel.
        if s.soft_start || (s.ramp && idle) {
            s.filter.observation(s.ramp_to * SCRATCH_DT);
            // The quick twin waits where the smooth filter is, for the hand.
            s.quick.x = s.filter.x;
            s.quick.v = s.filter.v;
        } else {
            let dx = s.dx * s.accumulator;
            s.filter.observation(dx);
            if s.trust > 0.0 {
                s.quick.observation(dx);
                s.follow_quick();
            }
        }
        let rate = s.filter.v;
        s.accumulator = 0.0;
        let done = (s.ramp && (s.ramp_to - rate).abs() <= 0.00001)
            || (s.brake && rate < s.ramp_to)
            || ((s.spinback || s.soft_start) && rate > s.ramp_to)
            || ((s.brake || s.soft_start) && !playing)
            || !loaded;
        let stop = done && (s.brake || s.spinback);
        if done {
            s.ramp = false;
            s.active = false;
            s.dx = 0.0;
            s.brake = false;
            s.spinback = false;
            s.soft_start = false;
        }
        let scratch2 = self.control(d, "scratch2");
        scratch2.set(rate);
        if stop {
            scratch2.set(0.0);
            self.control(d, "play").set_bool(false);
        }
        if done {
            self.control(d, "scratch2_enable").set_bool(false);
            self.scratch[d].held = false;
        }
    }

    /// Lets go of every deck this controller is scratching, ramping, braking
    /// or holding, as if the hand came off the wheel at once. Called when the
    /// controller goes away (unplugged, the computer slept, Refresh): the
    /// touch-release it would have sent never comes, and a deck left with
    /// `scratch2_enable` on stops playing and only follows `scratch2`.
    pub(crate) fn release_decks(&mut self) {
        for d in 1..=MAX_DECKS {
            // Held by a button: the button's release never comes either.
            // Which controller pressed it isn't known, so a deck playing
            // backwards loses its reverse whichever controller goes away.
            for key in HELD_KEYS {
                if let Some(h) = self.registry.get(&format!("[Channel{d}]"), key) {
                    if h.get_bool() {
                        h.set(0.0);
                    }
                }
            }
            let s = &mut self.scratch[d];
            let busy = s.held || s.active || s.ramp || s.brake || s.spinback || s.soft_start;
            if !busy {
                continue;
            }
            *s = Scratch::default();
            let enable = self.control(d, "scratch2_enable");
            if enable.get_bool() {
                enable.set_bool(false);
                self.control(d, "scratch2").set(0.0);
            }
        }
    }
}

impl Host {
    /// Makes `from` on MIDI channel `channel` act as `to` (see
    /// `note_routes`); `from == to` undoes it.
    pub(crate) fn route_note(&mut self, channel: u8, from: u8, to: u8) {
        let routes = self
            .note_routes
            .entry(channel & 0x0F)
            .or_insert_with(|| std::array::from_fn(|i| i as u8));
        let (from, to) = ((from & 0x7F) as usize, to & 0x7F);
        if routes[from] != to {
            routes[from] = to;
            self.routes_changed = true;
        }
    }

    /// The note an incoming note on or off acts as.
    pub(crate) fn routed_input(&self, status: u8, control: u8) -> u8 {
        if !matches!(status & 0xF0, 0x80 | 0x90) {
            return control;
        }
        match self.note_routes.get(&(status & 0x0F)) {
            Some(routes) => routes[(control & 0x7F) as usize],
            None => control,
        }
    }

    /// The messages waiting to be sent, with the lights of routed notes
    /// moved onto the pads routed to them.
    pub(crate) fn take_routed(&mut self) -> Vec<Vec<u8>> {
        let sent = std::mem::take(&mut self.sent);
        if self.note_routes.is_empty() {
            return sent;
        }
        let mut out = Vec::with_capacity(sent.len());
        for m in sent {
            let routes = match m[..] {
                [status, _, _] if status & 0xF0 == 0x90 => self.note_routes.get(&(status & 0x0F)),
                _ => None,
            };
            let Some(routes) = routes else {
                out.push(m);
                continue;
            };
            let (status, note, value) = (m[0], m[1] & 0x7F, m[2]);
            self.note_values.insert((status, note), value);
            for (from, to) in routes.iter().enumerate() {
                if *to == note {
                    out.push(vec![status, from as u8, value]);
                }
            }
        }
        if std::mem::take(&mut self.routes_changed) {
            // Every routed pad shows what its note last showed.
            let mut statuses: Vec<u8> = self.note_values.keys().map(|(s, _)| *s).collect();
            statuses.sort_unstable();
            statuses.dedup();
            for status in statuses {
                let Some(routes) = self.note_routes.get(&(status & 0x0F)) else {
                    continue;
                };
                for (from, to) in routes.iter().enumerate() {
                    if let Some(v) = self.note_values.get(&(status, *to)) {
                        out.push(vec![status, from as u8, *v]);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maschine_group_emitter_adds_sound_rgb_with_uniform_headroom() {
        use deckard_surface::{GroupView, MixView, SoundView};
        let sound = |rgb, meter| SoundView {
            rgb,
            mix: MixView {
                meter: (meter, meter),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut group = GroupView {
            sounds: vec![sound([255, 0, 0], 1.0), sound([0, 255, 0], 1.0)],
            ..Default::default()
        };
        assert_eq!(maschine_group_rgb(&group), [255, 255, 0]);
        group.sounds.push(sound([255, 0, 0], 1.0));
        assert_eq!(maschine_group_rgb(&group), [255, 128, 0]);
        group.sounds[1].mix.meter = (0.25, 0.25);
        assert_eq!(maschine_group_rgb(&group), [255, 32, 0]);
        group.muted = true;
        assert_eq!(maschine_group_rgb(&group), [0, 0, 0]);
    }

    fn host() -> Host {
        let reg = ControlRegistry::new();
        reg.create(
            "[Channel1]",
            "pregain",
            1.0,
            0.0,
            4.0,
            Behavior::CenteredOnDefault,
        );
        reg.create("[Channel1]", "rate", 0.0, -1.0, 1.0, Behavior::Linear);
        reg.create("[Channel1]", "rateRange", 0.08, 0.0, 1.0, Behavior::Linear);
        reg.toggle("[Channel1]", "play");
        reg.indicator("[Channel1]", "track_loaded", 1.0).set(1.0);
        reg.toggle("[Channel1]", "scratch2_enable");
        reg.toggle("[Channel1]", "reverse");
        reg.create(
            "[Channel1]",
            "scratch2",
            0.0,
            -100.0,
            100.0,
            Behavior::Linear,
        );
        Host::new(reg, "test".into())
    }

    #[test]
    fn shortcut_names_act_on_the_base_control() {
        let mut h = host();
        h.set_value("[Channel1]", "pregain", 3.0);
        h.set_value("[Channel1]", "pregain_set_default", 1.0);
        assert_eq!(h.resolve("[Channel1]", "pregain").get(), 1.0);
        h.set_value("[Channel1]", "rate_set_one", 1.0);
        assert_eq!(h.resolve("[Channel1]", "rate").get(), 1.0);
        h.set_value("[Channel1]", "rate_down", 1.0);
        assert!((h.resolve("[Channel1]", "rate").get() - 0.8).abs() < 1e-9);
        // Unknown names keep what scripts store in them.
        h.set_value("[EffectRack1_EffectUnit1]", "show_focus", 1.0);
        assert_eq!(
            h.resolve("[EffectRack1_EffectUnit1]", "show_focus").get(),
            1.0
        );
        // Relative controls add up.
        h.set_value("[Library]", "MoveVertical", 1.0);
        h.set_value("[Library]", "MoveVertical", 1.0);
        assert_eq!(h.resolve("[Library]", "MoveVertical").get(), 2.0);
    }

    #[test]
    fn soft_takeover_does_not_snap_back_after_an_outside_move() {
        let mut h = host();
        h.soft_takeover("[Channel1]", "rate", true);
        // The fader sits where the control is: taken.
        h.set_parameter("[Channel1]", "rate", 0.5);
        assert_eq!(h.control(1, "rate").get(), 0.0);
        // Something else moves the control within the grace period (a range
        // change, reset on load, sync, the mouse).
        h.control(1, "rate").set(-0.5);
        // The hardware's next message is stale: it must not snap it back.
        h.set_parameter("[Channel1]", "rate", 0.52);
        assert_eq!(h.control(1, "rate").get(), -0.5);
        // Reaching the new position picks it up again.
        h.set_parameter("[Channel1]", "rate", 0.24);
        assert!((h.control(1, "rate").get() + 0.52).abs() < 1e-9);
        // And fast moves from there are followed straight away.
        h.set_parameter("[Channel1]", "rate", 0.9);
        assert!((h.control(1, "rate").get() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn a_synced_follower_still_follows_its_tempo_fader() {
        let mut h = host();
        h.soft_takeover("[Channel1]", "rate", true);
        h.set_parameter("[Channel1]", "rate", 0.5);
        // Sync rewrites the follower's rate every buffer, drifting a hair
        // from what the fader set.
        h.control(1, "rate").set(0.003);
        // A fast fader move straight after is still followed.
        h.set_parameter("[Channel1]", "rate", 0.7);
        assert!((h.control(1, "rate").get() - 0.4).abs() < 1e-9);
        h.control(1, "rate").set(0.401);
        h.set_parameter("[Channel1]", "rate", 0.9);
        assert!((h.control(1, "rate").get() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn soft_takeover_waits_for_the_knob_to_catch_up() {
        let mut h = host();
        h.soft_takeover("[Channel1]", "pregain", true);
        // The first value is always dropped.
        h.set_parameter("[Channel1]", "pregain", 0.1);
        assert_eq!(h.resolve("[Channel1]", "pregain").get(), 1.0);
        // Still well below the knob's position on screen: ignored.
        std::thread::sleep(Duration::from_millis(60));
        h.set_parameter("[Channel1]", "pregain", 0.2);
        assert_eq!(h.resolve("[Channel1]", "pregain").get(), 1.0);
        // Close to the current value: taken from here on.
        h.set_parameter("[Channel1]", "pregain", 0.49);
        assert!(h.resolve("[Channel1]", "pregain").get() < 1.0);
        h.set_parameter("[Channel1]", "pregain", 0.7);
        assert!(h.resolve("[Channel1]", "pregain").get() > 1.0);
    }

    #[test]
    fn a_controller_that_goes_away_lets_go_of_the_deck() {
        let mut h = host();
        h.control(1, "play").set_bool(true);
        h.scratch_enable(1.0, 720.0, 100.0 / 3.0, 1.0 / 8.0, 1.0 / 256.0, false);
        h.scratch_tick(1.0, 3.0);
        h.process_scratch(Instant::now() + Duration::from_millis(5));
        assert!(h.is_scratching(1.0));
        // Unplugged with the hand on the wheel: no touch-release comes.
        h.release_decks();
        assert!(!h.is_scratching(1.0));
        assert_eq!(h.resolve("[Channel1]", "scratch2").get(), 0.0);
        assert!(h.control(1, "play").get_bool());
        // Nothing left running for the timer.
        assert!(h.scratch.iter().all(|s| !s.active));
    }

    #[test]
    fn a_controller_that_goes_away_lets_go_of_reverse() {
        let mut h = host();
        h.set_value("[Channel1]", "reverse", 1.0);
        h.release_decks();
        assert_eq!(h.resolve("[Channel1]", "reverse").get(), 0.0);
    }

    #[test]
    fn releasing_leaves_decks_other_controllers_hold_alone() {
        let mut h = host();
        // Another controller (or the screen) is scratching deck 1.
        h.control(1, "scratch2_enable").set_bool(true);
        h.release_decks();
        assert!(h.is_scratching(1.0));
    }

    /// The latency probe: a hand on a DDJ-FLX4-like wheel (720 ticks a
    /// turn, a MIDI message every 2 ms), fed through the 1 kHz filter.
    /// Returns the speed heard each millisecond.
    fn probe(hand: impl Fn(f64) -> f64, ms: u64) -> (Vec<f64>, Vec<f64>) {
        let (ipr, rpm) = (720.0, 100.0 / 3.0);
        let per_second = ipr * rpm / 60.0;
        let mut h = host();
        h.scratch_enable(1.0, ipr, rpm, 1.0 / 8.0, 1.0 / 256.0, false);
        let start = Instant::now();
        let (mut at, mut sent) = (0.0f64, 0.0f64);
        let (mut heard, mut wanted) = (Vec::new(), Vec::new());
        for ms in 0..ms {
            let t = ms as f64 / 1000.0;
            at += hand(t) * per_second / 1000.0;
            if ms % 2 == 0 {
                let ticks = at.trunc() - sent;
                sent += ticks;
                if ticks != 0.0 {
                    h.scratch_tick(1.0, ticks);
                }
            }
            h.process_scratch(start + Duration::from_millis(ms + 1));
            heard.push(h.resolve("[Channel1]", "scratch2").get());
            wanted.push(hand(t));
        }
        (heard, wanted)
    }

    #[test]
    fn scratch_latency_probe() {
        // Grabbing the record and pushing it at normal speed.
        let (heard, _) = probe(|_| 1.0, 1500);
        let reach = |f: f64| heard.iter().position(|&v| v >= f).unwrap() + 1;
        let (half, most) = (reach(0.5), reach(0.9));
        // A steady hand: the wobble left in the speed.
        let tail = &heard[500..];
        let mean = tail.iter().sum::<f64>() / tail.len() as f64;
        let wobble =
            (tail.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        // Baby scratches: back and forth eight times a second, up to 3x speed.
        let (fast, hand) = probe(|t| 3.0 * (std::f64::consts::TAU * 8.0 * t).sin(), 1500);
        let peak = fast[300..].iter().cloned().fold(0.0, f64::max);
        let lag = (0..80)
            .min_by(|&a, &b| {
                let err = |l: usize| {
                    (300..1500)
                        .map(|i| (fast[i] - hand[i - l]).powi(2))
                        .sum::<f64>()
                };
                err(a).total_cmp(&err(b))
            })
            .unwrap();
        eprintln!(
            "scratch probe: 50% in {half} ms, 90% in {most} ms, wobble {:.2}%, \
             8 Hz scratch lag {lag} ms, peak {peak:.2} of 3",
            wobble * 100.0
        );
        // Mixxx's filter alone: 26 ms, 63 ms, 0.46%, 26 ms and 1.8.
        assert!(half <= 16, "{half} ms to half speed");
        assert!(most <= 40, "{most} ms to 90%");
        assert!(wobble < 0.01, "wobble {wobble}");
        assert!(lag <= 14, "8 Hz lag {lag} ms");
        assert!(peak > 2.5, "8 Hz peak {peak}");
    }

    #[test]
    fn scratch_ticks_become_speed() {
        let mut h = host();
        h.scratch_enable(1.0, 128.0, 33.0 + 1.0 / 3.0, 1.0 / 8.0, 1.0 / 256.0, false);
        assert!(h.is_scratching(1.0));
        // One revolution per 1.8 s is normal speed; feed that for 300 ms.
        let start = Instant::now();
        let per_ms = 128.0 * (100.0 / 3.0) / 60.0 / 1000.0;
        for ms in 1..=300u64 {
            h.scratch_tick(1.0, per_ms);
            h.process_scratch(start + Duration::from_millis(ms));
        }
        let speed = h.resolve("[Channel1]", "scratch2").get();
        assert!((speed - 1.0).abs() < 0.1, "{speed}");
        // Backwards.
        for ms in 301..=600u64 {
            h.scratch_tick(1.0, -per_ms);
            h.process_scratch(start + Duration::from_millis(ms));
        }
        let speed = h.resolve("[Channel1]", "scratch2").get();
        assert!((speed + 1.0).abs() < 0.1, "{speed}");
        h.scratch_disable(1.0, false);
        assert!(!h.is_scratching(1.0));
    }
}
