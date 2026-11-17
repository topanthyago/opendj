//! Eight independently sequenced groups on the original Groove engine.
//! The existing deck controls edit the selected group. Each group's own
//! controls remain addressable and continue playing when focus moves.
//! `pattern_target` and `pattern_select` assign one of sixteen slots and
//! enable the focused Group. `pattern_stop` disables that Group. The global
//! transport is independent. `erase_target` and `erase_pattern` also exist
//! in each Group scope, with `erase_serial` acknowledging completion.
//! `rowR_erase` and `groupK_erase` are continuous playhead erasure holds.

use std::sync::Arc;

use opendj_control::{Behavior, ControlHandle, ControlRegistry};

use crate::effects::BLOCK;
use crate::groove::{
    groove_group, Clock, GrooveBox, GrooveControls, GrooveHit, Source, GROOVES, ROWS,
};
use crate::kits::{self, Kit};

pub const GROUPS: usize = 8;
const UNDO_STATES: usize = 32;
const COLORS: [u32; GROUPS] = [
    0xff2d23, 0xff7819, 0xffdc23, 0x4bdc46, 0x1ed2dc, 0x3764ff, 0xaf3cff, 0xf541b4,
];

pub fn group_scope(deck: usize, group: usize) -> String {
    format!("[Groove{}Group{}]", deck + 1, group + 1)
}

pub fn register(registry: &ControlRegistry, deck: usize) {
    let scope = groove_group(deck);
    GrooveControls::new(registry, deck);
    crate::groove_pads::PadControls::new(registry, deck);
    registry
        .indicator(&scope, "pattern_api_version", 1.0)
        .set(1.0);
    registry
        .indicator(&scope, "pattern_slots", GROOVES as f64)
        .set(GROOVES as f64);
    registry.create(
        &scope,
        "group_selected",
        0.0,
        0.0,
        (GROUPS - 1) as f64,
        Behavior::Free,
    );
    registry.create(&scope, "group_enabled", 1.0, 0.0, 1.0, Behavior::Free);
    for index in 0..GROUPS {
        let group = group_scope(deck, index);
        GrooveControls::new_scoped(registry, &group);
        crate::groove_pads::PadControls::new_scoped(registry, &group);
        registry.toggle(&group, "initialized");
        registry.create(
            &scope,
            &format!("group{}_enabled", index + 1),
            if index == 0 { 1.0 } else { 0.0 },
            0.0,
            1.0,
            Behavior::Free,
        );
        registry.toggle(&scope, &format!("group{}_mute", index + 1));
        registry.toggle(&scope, &format!("group{}_solo", index + 1));
        registry.toggle(&scope, &format!("group{}_erase", index + 1));
        registry.create(
            &scope,
            &format!("group{}_level", index + 1),
            1.0,
            0.0,
            1.5,
            Behavior::Linear,
        );
        registry.create(
            &scope,
            &format!("group{}_color", index + 1),
            COLORS[index] as f64,
            0.0,
            0xffffff as f64,
            Behavior::Free,
        );
        for row in 0..ROWS {
            registry.create(
                &group,
                &format!("row{}_color", row + 1),
                COLORS[row % GROUPS] as f64,
                0.0,
                0xffffff as f64,
                Behavior::Free,
            );
        }
    }
}

/// Values saved with a group, independent of the editor's current focus.
pub fn kept_keys() -> Vec<String> {
    let mut keys: Vec<String> = [
        "kit",
        "groove",
        "genre",
        "pattern",
        "library_set",
        "chain",
        "pattern_chain",
        "pattern_chain_start",
        "pattern_chain_end",
        "swing",
        "kick_key",
        "vary",
        "auto_fill",
        "auto_fill_style",
        "follow_breaks",
        "break_sensitivity",
        "duck_deck",
        "duck_amount",
        "row_selected",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    for pattern in 1..=GROOVES {
        for row in 1..=ROWS {
            for suffix in ["", "_soft", "_accent", "_maybe"] {
                keys.push(format!("pattern{pattern}_row{row}{suffix}"));
            }
        }
    }
    for row in 1..=ROWS {
        for name in [
            "mute", "solo", "level", "pitch", "decay", "tone", "pan", "sound", "lock",
        ] {
            keys.push(format!("row{row}_{name}"));
        }
    }
    keys
}

fn action_keys() -> Vec<String> {
    let mut keys: Vec<String> = [
        "load",
        "load_set",
        "load_chain",
        "dice",
        "dice_row",
        "dice_undo",
        "row_clear",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    for row in 1..=ROWS {
        keys.push(format!("row{row}_preview"));
        keys.push(format!("pad{row}"));
    }
    keys
}

fn handles(registry: &ControlRegistry, scope: &str, names: &[String]) -> Vec<ControlHandle> {
    names
        .iter()
        .map(|name| {
            registry
                .get(scope, name)
                .expect("registered Groove control")
        })
        .collect()
}

fn take_pulse(control: &ControlHandle, seen: &mut u32) -> bool {
    let pressed = control.get_bool() || control.version() != *seen;
    control.set_bool(false);
    *seen = control.version();
    pressed
}

struct PatternErase {
    command: ControlHandle,
    seen: u32,
    target: ControlHandle,
    serial: ControlHandle,
    last_target: ControlHandle,
}

impl PatternErase {
    fn new(registry: &ControlRegistry, scope: &str) -> Self {
        let command = registry.push(scope, "erase_pattern");
        Self {
            seen: command.version(),
            command,
            target: registry.create(
                scope,
                "erase_target",
                -1.0,
                -1.0,
                (GROOVES - 1) as f64,
                Behavior::Free,
            ),
            serial: registry.indicator(scope, "erase_serial", u32::MAX as f64),
            last_target: registry.indicator(scope, "erase_last_target", (GROOVES - 1) as f64),
        }
    }

    fn take(&mut self, current: usize) -> Option<usize> {
        if !take_pulse(&self.command, &mut self.seen) {
            return None;
        }
        let target = if self.target.get() < 0.0 {
            current
        } else {
            self.target.get() as usize
        };
        self.target.set(-1.0);
        Some(target)
    }

    fn confirm(&self, target: usize) {
        self.last_target.set(target as f64);
        self.serial
            .set((self.serial.get() as u32).wrapping_add(1) as f64);
    }
}

struct Group {
    engine: GrooveBox,
    saved: Vec<ControlHandle>,
    actions: Vec<ControlHandle>,
    enabled: ControlHandle,
    mute: ControlHandle,
    solo: ControlHandle,
    erase: ControlHandle,
    pattern_erase: PatternErase,
    level: ControlHandle,
    color: ControlHandle,
    meter: ControlHandle,
    colors: [ControlHandle; ROWS],
    activity: [ControlHandle; 3],
    was_enabled: bool,
}

impl Group {
    fn erase_pattern(&self, pattern: usize) {
        let c = &self.engine.c;
        for row in 0..ROWS {
            for values in [&c.patterns, &c.soft, &c.accent, &c.maybe] {
                values[pattern][row].set(0.0);
            }
        }
        if c.groove.get() as usize == pattern {
            self.enabled.set_bool(false);
        }
        self.pattern_erase.confirm(pattern);
    }

    fn state(&self) -> impl Iterator<Item = &ControlHandle> {
        self.saved
            .iter()
            .chain([
                &self.enabled,
                &self.mute,
                &self.solo,
                &self.level,
                &self.color,
            ])
            .chain(&self.colors)
    }
}

pub struct GrooveBanks {
    pub c: GrooveControls,
    pub clock: Clock,
    pub key: Option<u8>,
    pub last_bpm: f64,
    pub(crate) hit_tx: Option<rtrb::Producer<GrooveHit>>,
    groups: Box<[Group]>,
    selected: ControlHandle,
    group_enabled: ControlHandle,
    last_enabled: u32,
    selected_index: usize,
    editor: Vec<ControlHandle>,
    editor_seen: Vec<u32>,
    editor_sound_indices: Vec<usize>,
    row_maps: Vec<[usize; ROWS]>,
    editor_actions: Vec<ControlHandle>,
    actions_seen: Vec<u32>,
    pulse_actions: Vec<bool>,
    erase_rows_seen: [u32; ROWS],
    editor_pad_mode: ControlHandle,
    editor_record: ControlHandle,
    editor_colors: [ControlHandle; ROWS],
    scratch: Vec<[f32; 2]>,
    rate: f64,
    running: bool,
    undo: ControlHandle,
    redo: ControlHandle,
    erase: PatternErase,
    pattern_target: ControlHandle,
    pattern_select: ControlHandle,
    pattern_select_seen: u32,
    pattern_stop: ControlHandle,
    pattern_stop_seen: u32,
    copy: ControlHandle,
    copy_kind: ControlHandle,
    copy_source: ControlHandle,
    copy_target: ControlHandle,
    copy_serial: ControlHandle,
    can_undo: ControlHandle,
    can_redo: ControlHandle,
    history: Vec<Box<[f64]>>,
    current: Box<[f64]>,
    cursor: usize,
    history_len: usize,
}

impl GrooveBanks {
    pub fn new(registry: &ControlRegistry, deck: usize, kits: Arc<[Kit]>, rate: f64) -> Self {
        register(registry, deck);
        let scope = groove_group(deck);
        let c = GrooveControls::new(registry, deck);
        crate::groove_pads::PadControls::new(registry, deck);
        let saved_names = kept_keys();
        let action_names = action_keys();
        let editor = handles(registry, &scope, &saved_names);
        let editor_actions = handles(registry, &scope, &action_names);
        let groups = (0..GROUPS)
            .map(|index| {
                let group = group_scope(deck, index);
                let initialized = registry
                    .get(&group, "initialized")
                    .expect("Groove group initialization");
                let fresh = !initialized.get_bool();
                let engine = GrooveBox::new_scoped(registry, deck, &group, kits.clone(), rate);
                if fresh {
                    if index == 0 {
                        for (target, source) in
                            handles(registry, &group, &saved_names).iter().zip(&editor)
                        {
                            target.set(source.get());
                        }
                    } else {
                        for pattern in 0..GROOVES {
                            for row in 0..ROWS {
                                engine.c.patterns[pattern][row].set(0.0);
                                engine.c.soft[pattern][row].set(0.0);
                                engine.c.accent[pattern][row].set(0.0);
                                engine.c.maybe[pattern][row].set(0.0);
                            }
                        }
                        if kits.len() > kits::KITS.len() + index {
                            engine.c.kit.set((kits::KITS.len() + index) as f64);
                        }
                    }
                    for (row, sound) in kits::default_row_map(engine.c.kit.get() as usize)
                        .into_iter()
                        .enumerate()
                    {
                        engine.c.sound[row].set(sound as f64);
                    }
                    if engine.c.kit.get() as usize >= kits::KITS.len() {
                        engine.c.auto_fill_style.set_bool(true);
                    }
                    initialized.set_bool(true);
                }
                let enabled = registry.create(
                    &scope,
                    &format!("group{}_enabled", index + 1),
                    if index == 0 { 1.0 } else { 0.0 },
                    0.0,
                    1.0,
                    Behavior::Free,
                );
                Group {
                    saved: handles(registry, &group, &saved_names),
                    actions: handles(registry, &group, &action_names),
                    colors: std::array::from_fn(|row| {
                        registry.create(
                            &group,
                            &format!("row{}_color", row + 1),
                            COLORS[row % GROUPS] as f64,
                            0.0,
                            0xffffff as f64,
                            Behavior::Free,
                        )
                    }),
                    activity: std::array::from_fn(|component| {
                        registry.indicator(
                            &scope,
                            &format!("group{}_activity_{}", index + 1, ["r", "g", "b"][component]),
                            255.0,
                        )
                    }),
                    mute: registry.toggle(&scope, &format!("group{}_mute", index + 1)),
                    solo: registry.toggle(&scope, &format!("group{}_solo", index + 1)),
                    erase: registry.toggle(&scope, &format!("group{}_erase", index + 1)),
                    pattern_erase: PatternErase::new(registry, &group),
                    level: registry.create(
                        &scope,
                        &format!("group{}_level", index + 1),
                        1.0,
                        0.0,
                        1.5,
                        Behavior::Linear,
                    ),
                    color: registry
                        .get(&scope, &format!("group{}_color", index + 1))
                        .expect("Groove group color"),
                    meter: registry.indicator(&scope, &format!("group{}_meter", index + 1), 1.0),
                    was_enabled: enabled.get_bool(),
                    enabled,
                    engine,
                }
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        for index in 0..GROUPS {
            registry.create(
                &scope,
                &format!("group{}_color", index + 1),
                COLORS[index] as f64,
                0.0,
                0xffffff as f64,
                Behavior::Free,
            );
        }
        let selected = registry.create(
            &scope,
            "group_selected",
            0.0,
            0.0,
            (GROUPS - 1) as f64,
            Behavior::Free,
        );
        let group_enabled = registry.create(&scope, "group_enabled", 1.0, 0.0, 1.0, Behavior::Free);
        let erase_rows_seen = std::array::from_fn(|row| c.erase_rows[row].version());
        let pattern_select = registry.push(&scope, "pattern_select");
        let pattern_stop = registry.push(&scope, "pattern_stop");
        let total = (editor.len() + 5 + ROWS) * GROUPS;
        let mut this = Self {
            c,
            clock: Clock::default(),
            key: None,
            last_bpm: crate::groove::DEFAULT_BPM,
            hit_tx: None,
            groups,
            last_enabled: group_enabled.version(),
            group_enabled,
            selected,
            selected_index: 0,
            editor_seen: editor.iter().map(ControlHandle::version).collect(),
            editor_sound_indices: saved_names
                .iter()
                .enumerate()
                .filter_map(|(index, name)| name.ends_with("_sound").then_some(index))
                .collect(),
            row_maps: (0..kits.len()).map(kits::default_row_map).collect(),
            actions_seen: editor_actions.iter().map(ControlHandle::version).collect(),
            pulse_actions: action_names
                .iter()
                .map(|name| name != "dice_row" && name != "dice" && !name.starts_with("pad"))
                .collect(),
            erase_rows_seen,
            editor,
            editor_actions,
            editor_pad_mode: registry.get(&scope, "pad_mode").expect("Groove pad mode"),
            editor_record: registry.get(&scope, "record").expect("Groove record mode"),
            editor_colors: std::array::from_fn(|row| {
                registry
                    .get(&scope, &format!("pad{}_color", row + 1))
                    .expect("Groove pad color")
            }),
            scratch: vec![[0.0; 2]; BLOCK],
            rate,
            running: false,
            undo: registry.push(&scope, "undo"),
            redo: registry.push(&scope, "redo"),
            erase: PatternErase::new(registry, &scope),
            pattern_target: registry.create(
                &scope,
                "pattern_target",
                0.0,
                0.0,
                (GROOVES - 1) as f64,
                Behavior::Free,
            ),
            pattern_select_seen: pattern_select.version(),
            pattern_select,
            pattern_stop_seen: pattern_stop.version(),
            pattern_stop,
            copy: registry.push(&scope, "copy"),
            copy_kind: registry.create(&scope, "copy_kind", 0.0, 0.0, 2.0, Behavior::Free),
            copy_source: registry.create(&scope, "copy_source", 0.0, 0.0, 15.0, Behavior::Free),
            copy_target: registry.create(&scope, "copy_target", 0.0, 0.0, 15.0, Behavior::Free),
            copy_serial: registry.indicator(&scope, "copy_serial", u32::MAX as f64),
            can_undo: registry.indicator(&scope, "can_undo_edit", 1.0),
            can_redo: registry.indicator(&scope, "can_redo_edit", 1.0),
            history: (0..UNDO_STATES)
                .map(|_| vec![0.0; total].into_boxed_slice())
                .collect(),
            current: vec![0.0; total].into_boxed_slice(),
            cursor: 0,
            history_len: 1,
        };
        this.show_selected(true);
        this.snapshot();
        this.history[0].copy_from_slice(&this.current);
        this
    }

    pub fn enabled(&self) -> bool {
        self.c.enabled.get_bool()
    }

    pub fn wants_breaks(&self) -> bool {
        self.groups
            .iter()
            .any(|group| group.enabled.get_bool() && group.engine.c.follow_breaks.get_bool())
    }

    pub fn listen_for_break(&mut self, rms: Option<(usize, f32)>, frames: usize, bpm: f64) {
        for group in &mut self.groups {
            group.engine.listen_for_break(rms, frames, bpm);
        }
    }

    fn receive_editor(&mut self) {
        let selected = self.selected.get() as usize;
        if selected != self.selected_index {
            let group = &self.groups[self.selected_index];
            for pad in &group.engine.pads.c.pads {
                pad.set(0.0);
            }
            for erase in &group.engine.c.erase_rows {
                erase.set_bool(false);
            }
            for (control, seen) in self.editor_actions.iter().zip(&mut self.actions_seen) {
                if control.version() == *seen {
                    control.set(0.0);
                    *seen = control.version();
                }
            }
            self.selected_index = selected.min(GROUPS - 1);
            self.show_selected(false);
        }
        let group = &self.groups[self.selected_index];
        let pattern_selected = self.editor[1].version() != self.editor_seen[1];
        let kit_only = self.editor[0].version() != self.editor_seen[0]
            && self
                .editor_sound_indices
                .iter()
                .all(|&index| self.editor[index].version() == self.editor_seen[index]);
        for ((source, seen), target) in self
            .editor
            .iter()
            .zip(&mut self.editor_seen)
            .zip(&group.saved)
        {
            if source.version() != *seen {
                target.set(source.get());
                *seen = source.version();
            }
        }
        if kit_only {
            if let Some(mapping) = self.row_maps.get(group.engine.c.kit.get() as usize) {
                for (row, sound) in mapping.iter().enumerate() {
                    group.engine.c.sound[row].set(*sound as f64);
                }
            }
        }
        if self.group_enabled.version() != self.last_enabled {
            group.enabled.set(self.group_enabled.get());
            self.last_enabled = self.group_enabled.version();
        }
        if pattern_selected {
            group.enabled.set_bool(true);
        }
        for ((source, seen), target) in self
            .c
            .erase_rows
            .iter()
            .zip(&mut self.erase_rows_seen)
            .zip(&group.engine.c.erase_rows)
        {
            if source.version() != *seen {
                // Continuous holds use their final state, including a complete
                // press/release that happened between audio buffers.
                target.set(source.get());
                *seen = source.version();
            }
        }
        for (((source, seen), target), pulse) in self
            .editor_actions
            .iter()
            .zip(&mut self.actions_seen)
            .zip(&group.actions)
            .zip(&self.pulse_actions)
        {
            if source.version() != *seen {
                // A complete press/release between buffers must remain an edge.
                if *pulse && source.get() == 0.0 && source.version().wrapping_sub(*seen) >= 2 {
                    target.set(1.0);
                } else {
                    if source.get() == 0.0 && source.version().wrapping_sub(*seen) >= 2 {
                        target.set(1.0);
                    }
                    target.set(source.get());
                }
                *seen = source.version();
            }
        }
    }

    fn show_selected(&mut self, force: bool) {
        let group = &self.groups[self.selected_index];
        for ((target, seen), source) in self
            .editor
            .iter()
            .zip(&mut self.editor_seen)
            .zip(&group.saved)
        {
            if force || target.version() == *seen {
                target.set(source.get());
                *seen = target.version();
            }
        }
        for ((target, seen), source) in self
            .c
            .erase_rows
            .iter()
            .zip(&mut self.erase_rows_seen)
            .zip(&group.engine.c.erase_rows)
        {
            if force || target.version() == *seen {
                target.set(source.get());
                *seen = target.version();
            }
        }
        for (target, source) in self.c.occupied.iter().zip(&group.engine.c.occupied) {
            target.set(source.get());
        }
        if force || self.group_enabled.version() == self.last_enabled {
            self.group_enabled.set(group.enabled.get());
            self.last_enabled = self.group_enabled.version();
        }
        for ((target, seen), source) in self
            .editor_actions
            .iter()
            .zip(&mut self.actions_seen)
            .zip(&group.actions)
        {
            if target.version() == *seen {
                target.set(source.get());
                *seen = target.version();
            }
        }
        for row in 0..ROWS {
            self.c.meter[row].set(group.engine.c.meter[row].get());
            self.editor_colors[row].set(group.engine.pads.c.colors[row].get());
        }
        for (target, source) in [
            (&self.c.position, &group.engine.c.position),
            (&self.c.beat, &group.engine.c.beat),
            (&self.c.groove_playing, &group.engine.c.groove_playing),
            (&self.c.groove_queued, &group.engine.c.groove_queued),
            (&self.c.fill_playing, &group.engine.c.fill_playing),
            (&self.c.break_active, &group.engine.c.break_active),
            (&self.c.can_undo, &group.engine.c.can_undo),
            (&self.c.dice_pending, &group.engine.c.dice_pending),
            (&self.c.set_pending, &group.engine.c.set_pending),
            (&self.c.kick_note, &group.engine.c.kick_note),
        ] {
            target.set(source.get());
        }
    }

    fn snapshot(&mut self) {
        for (target, source) in self
            .current
            .iter_mut()
            .zip(self.groups.iter().flat_map(Group::state))
        {
            *target = source.get();
        }
    }

    fn remember(&mut self) {
        self.snapshot();
        if self.current.as_ref() == self.history[self.cursor].as_ref() {
            return;
        }
        if self.cursor + 1 == UNDO_STATES {
            self.history.rotate_left(1);
        } else {
            self.cursor += 1;
        }
        self.history[self.cursor].copy_from_slice(&self.current);
        self.history_len = self.cursor + 1;
        self.can_undo.set_bool(self.cursor > 0);
        self.can_redo.set_bool(false);
    }

    fn restore(&mut self) {
        for (source, target) in self.history[self.cursor]
            .iter()
            .zip(self.groups.iter().flat_map(Group::state))
        {
            target.set(*source);
        }
        self.show_selected(true);
        self.can_undo.set_bool(self.cursor > 0);
        self.can_redo.set_bool(self.cursor + 1 < self.history_len);
    }

    fn edits(&mut self) {
        if take_pulse(&self.pattern_select, &mut self.pattern_select_seen) {
            let group = &self.groups[self.selected_index];
            group.engine.c.pattern_chain.set_bool(false);
            group.engine.c.groove.set(self.pattern_target.get());
            group.enabled.set_bool(true);
        }
        if take_pulse(&self.pattern_stop, &mut self.pattern_stop_seen) {
            self.groups[self.selected_index].enabled.set_bool(false);
        }
        if self.undo.get_bool() {
            self.undo.set_bool(false);
            if self.cursor > 0 {
                self.cursor -= 1;
                self.restore();
            }
        }
        if self.redo.get_bool() {
            self.redo.set_bool(false);
            if self.cursor + 1 < self.history_len {
                self.cursor += 1;
                self.restore();
            }
        }
        let selected = &self.groups[self.selected_index];
        if let Some(pattern) = self.erase.take(selected.engine.c.groove.get() as usize) {
            selected.erase_pattern(pattern);
            self.erase.confirm(pattern);
        }
        for group in &mut self.groups {
            if let Some(pattern) = group
                .pattern_erase
                .take(group.engine.c.groove.get() as usize)
            {
                group.erase_pattern(pattern);
            }
        }
        if self.copy.get_bool() {
            self.copy.set_bool(false);
            let source = self.copy_source.get() as usize;
            let target = self.copy_target.get() as usize;
            let group = &self.groups[self.selected_index].engine.c;
            match self.copy_kind.get() as usize {
                0 if source < ROWS && target < ROWS => {
                    for pattern in 0..GROOVES {
                        for values in [&group.patterns, &group.soft, &group.accent, &group.maybe] {
                            values[pattern][target].set(values[pattern][source].get());
                        }
                    }
                    for values in [
                        &group.mute,
                        &group.solo,
                        &group.level,
                        &group.pitch,
                        &group.decay,
                        &group.tone,
                        &group.pan,
                        &group.sound,
                        &group.lock,
                    ] {
                        values[target].set(values[source].get());
                    }
                }
                1 if source < GROOVES && target < GROOVES => {
                    for row in 0..ROWS {
                        for values in [&group.patterns, &group.soft, &group.accent, &group.maybe] {
                            values[target][row].set(values[source][row].get());
                        }
                    }
                }
                2 if source < GROUPS && target < GROUPS => {
                    for (from, to) in self.groups[source].state().zip(self.groups[target].state()) {
                        to.set(from.get());
                    }
                }
                _ => return,
            }
            self.copy_serial
                .set((self.copy_serial.get() as u32).wrapping_add(1) as f64);
        }
    }

    pub fn render(
        &mut self,
        play: bool,
        source: Source,
        out: &mut [[f32; 2]],
        mut kick: impl FnMut(usize),
    ) {
        self.receive_editor();
        self.edits();
        if play != self.running {
            if play {
                self.clock.start(source);
            } else {
                self.clock.stop();
            }
            self.running = play;
        }
        let span = self.clock.advance(out.len(), self.rate, source);
        let synchronized = Source {
            bpm: source.bpm,
            beat: play.then_some(span.beat),
        };
        let any_solo = self.groups.iter().any(|group| group.solo.get_bool());
        let selected = self.selected_index;
        let pad_mode = self.editor_pad_mode.get();
        let recording = self.editor_record.get_bool();
        out.fill([0.0; 2]);
        for (index, group) in self.groups.iter_mut().enumerate() {
            let enabled = group.enabled.get_bool();
            if group.was_enabled && !enabled {
                group.engine.stop_voices();
            }
            group.was_enabled = enabled;
            group.engine.key = self.key;
            group.engine.erase_all = group.erase.get_bool();
            group.engine.c.bpm.set(source.bpm);
            group.engine.c.fill.set(self.c.fill.get());
            group.engine.c.remix.set(self.c.remix.get());
            group.engine.pads.c.mode.set(if index == selected {
                pad_mode
            } else {
                crate::groove_pads::mode::PLAY as f64
            });
            group
                .engine
                .pads
                .c
                .record
                .set_bool(index == selected && recording);
            let audible = !group.mute.get_bool() && (!any_solo || group.solo.get_bool());
            group.engine.hit_tx = self.hit_tx.take();
            group.engine.render(
                play && enabled,
                synchronized,
                &mut self.scratch[..out.len()],
                |frame| {
                    if audible {
                        kick(frame);
                    }
                },
            );
            self.hit_tx = group.engine.hit_tx.take();
            let gain = if audible {
                group.level.get() as f32
            } else {
                0.0
            };
            let mut peak = 0.0f32;
            for (target, frame) in out.iter_mut().zip(&self.scratch) {
                target[0] += frame[0] * gain;
                target[1] += frame[1] * gain;
                peak = peak.max(frame[0].abs() * gain).max(frame[1].abs() * gain);
            }
            group.meter.set(peak.min(1.0) as f64);
            let mut rgb = [0.0f64; 3];
            for (row, color) in group.colors.iter().enumerate() {
                let color = color.get() as u32;
                let intensity = if audible {
                    group.engine.c.meter[row].get() * group.level.get()
                } else {
                    0.0
                };
                for component in 0..3 {
                    rgb[component] += ((color >> (16 - component * 8)) & 255) as f64 * intensity;
                }
            }
            let scale = rgb.iter().copied().fold(255.0f64, f64::max) / 255.0;
            for (target, value) in group.activity.iter().zip(rgb) {
                target.set(value / scale);
            }
        }
        self.last_bpm = source.bpm;
        // Keep all changes made by one continuous ERASE hold in a single
        // preallocated history entry, committed after the final release.
        if !self.groups.iter().any(|group| {
            group.erase.get_bool()
                || group
                    .engine
                    .c
                    .erase_rows
                    .iter()
                    .any(ControlHandle::get_bool)
        }) {
            self.remember();
        }
        self.show_selected(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits::KitSound;
    use crate::sampler::Sample;

    fn fixture() -> (Arc<ControlRegistry>, GrooveBanks) {
        let registry = ControlRegistry::new();
        let kits: Vec<Kit> = (0..2)
            .map(|kit| Kit {
                name: "Test kit",
                sounds: (0..kits::MAX_SOUNDS)
                    .map(|row| KitSound {
                        name: "Test sound",
                        pitch: 0.0,
                        sample: Arc::new(Sample::from_audio(
                            &crate::track::Audio::from_frames(
                                48_000,
                                vec![[0.1 + row as f32 / 100.0 + kit as f32 / 100.0; 2]; 4096],
                            ),
                            48_000.0,
                        )),
                    })
                    .collect(),
            })
            .collect();
        let bank = GrooveBanks::new(&registry, 0, kits.into(), 48_000.0);
        (registry, bank)
    }

    fn c(registry: &ControlRegistry, name: &str) -> ControlHandle {
        registry.get("[Groove1]", name).expect("test control")
    }

    fn render(bank: &mut GrooveBanks) -> Vec<[f32; 2]> {
        let mut out = vec![[0.0; 2]; BLOCK];
        bank.render(
            false,
            Source {
                bpm: 120.0,
                beat: None,
            },
            &mut out,
            |_| {},
        );
        out
    }

    fn advance(bank: &mut GrooveBanks, frames: usize) {
        let mut out = vec![[0.0; 2]; BLOCK];
        let mut remaining = frames;
        while remaining > 0 {
            let count = remaining.min(BLOCK);
            bank.render(
                true,
                Source {
                    bpm: 120.0,
                    beat: None,
                },
                &mut out[..count],
                |_| {},
            );
            remaining -= count;
        }
    }

    #[test]
    fn selecting_a_loaded_pattern_enables_its_group_and_preserves_stopped_transport() {
        let (registry, mut bank) = fixture();
        c(&registry, "group_enabled").set_bool(false);
        c(&registry, "pattern16_row16").set(0x8000 as f64);
        render(&mut bank);
        c(&registry, "pattern_target").set(15.0);
        c(&registry, "pattern_select").set_bool(true);
        c(&registry, "pattern_select").set_bool(false);
        render(&mut bank);
        assert_eq!(c(&registry, "groove").get(), 15.0);
        assert!(c(&registry, "group_enabled").get_bool());
        assert!(!bank.running);
        assert!(!bank.groups[0].engine.clock.running);
        assert_eq!(c(&registry, "pattern16_occupied").get(), 1.0);
        assert_eq!(c(&registry, "pattern_slots").get(), 16.0);
        assert_eq!(c(&registry, "pattern_api_version").get(), 1.0);
        assert!(kept_keys()
            .iter()
            .any(|name| name == "pattern16_row16_maybe"));
    }

    #[test]
    fn legacy_editor_selection_enables_a_disabled_group() {
        let (registry, mut bank) = fixture();
        c(&registry, "group_enabled").set_bool(false);
        render(&mut bank);
        c(&registry, "groove").set(1.0);
        render(&mut bank);
        assert!(c(&registry, "group_enabled").get_bool());
        assert_eq!(bank.groups[0].engine.c.groove.get(), 1.0);
    }

    #[test]
    fn native_pattern_selection_keeps_the_existing_bar_quantization() {
        let (registry, mut bank) = fixture();
        c(&registry, "pattern16_row1").set(0x5555 as f64);
        advance(&mut bank, BLOCK);
        c(&registry, "pattern_target").set(15.0);
        c(&registry, "pattern_select").set_bool(true);
        advance(&mut bank, BLOCK);
        assert_eq!(c(&registry, "groove").get(), 15.0);
        assert_eq!(c(&registry, "groove_playing").get(), 0.0);
        assert_eq!(c(&registry, "groove_queued").get(), 15.0);
        advance(&mut bank, 96_000 - 2 * BLOCK);
        assert_eq!(c(&registry, "groove_playing").get(), 0.0);
        advance(&mut bank, 1);
        assert_eq!(c(&registry, "groove_playing").get(), 15.0);
        assert!(bank.running);
    }

    #[test]
    fn enabling_a_group_in_a_running_transport_waits_for_the_downbeat() {
        let (registry, mut bank) = fixture();
        c(&registry, "group_enabled").set_bool(false);
        c(&registry, "pattern16_row1").set(0xffff as f64);
        advance(&mut bank, BLOCK);
        c(&registry, "pattern_target").set(15.0);
        c(&registry, "pattern_select").set_bool(true);
        advance(&mut bank, BLOCK);
        assert!(c(&registry, "group_enabled").get_bool());
        assert!(bank.running);
        assert_eq!(c(&registry, "position").get(), -1.0);
        advance(&mut bank, 96_000 - 2 * BLOCK);
        assert_eq!(c(&registry, "position").get(), -1.0);
        advance(&mut bank, 512);
        assert_eq!(c(&registry, "position").get(), 0.0);
    }

    #[test]
    fn targeted_delete_preserves_the_playing_pattern_kit_mix_and_other_groups() {
        let (registry, mut bank) = fixture();
        c(&registry, "pattern1_row1").set(0x1111 as f64);
        c(&registry, "pattern16_row16").set(0xffff as f64);
        c(&registry, "kit").set(1.0);
        c(&registry, "row16_level").set(0.75);
        bank.groups[1].engine.c.patterns[15][15].set(0x4444 as f64);
        advance(&mut bank, BLOCK);
        c(&registry, "erase_target").set(15.0);
        c(&registry, "erase_pattern").set_bool(true);
        c(&registry, "erase_pattern").set_bool(false);
        advance(&mut bank, BLOCK);
        assert_eq!(c(&registry, "pattern16_row16").get(), 0.0);
        assert_eq!(c(&registry, "pattern1_row1").get(), 0x1111 as f64);
        assert_eq!(c(&registry, "groove").get(), 0.0);
        assert_eq!(c(&registry, "groove_playing").get(), 0.0);
        assert!(c(&registry, "group_enabled").get_bool());
        assert_eq!(c(&registry, "kit").get(), 1.0);
        assert_eq!(c(&registry, "row16_level").get(), 0.75);
        assert_eq!(
            bank.groups[1].engine.c.patterns[15][15].get(),
            0x4444 as f64
        );
        assert_eq!(c(&registry, "erase_last_target").get(), 15.0);
        assert_eq!(c(&registry, "erase_target").get(), -1.0);
        assert_eq!(c(&registry, "erase_serial").get(), 1.0);
        advance(&mut bank, BLOCK);
        assert_eq!(c(&registry, "erase_serial").get(), 1.0);
    }

    #[test]
    fn deleting_the_assigned_pattern_stops_only_its_group_and_is_undoable() {
        let (registry, mut bank) = fixture();
        c(&registry, "pattern1_row1").set(0x1111 as f64);
        c(&registry, "group2_enabled").set_bool(true);
        bank.groups[1].engine.c.patterns[0][0].set(0x5555 as f64);
        advance(&mut bank, BLOCK);
        c(&registry, "erase_pattern").set_bool(true);
        advance(&mut bank, BLOCK);
        assert!(!c(&registry, "group1_enabled").get_bool());
        assert!(c(&registry, "group2_enabled").get_bool());
        assert!(bank.groups[1].engine.clock.running);
        assert!(bank.running);
        assert_eq!(bank.groups[1].engine.c.patterns[0][0].get(), 0x5555 as f64);
        c(&registry, "undo").set_bool(true);
        render(&mut bank);
        assert!(c(&registry, "group1_enabled").get_bool());
        assert_eq!(c(&registry, "pattern1_row1").get(), 0x1111 as f64);
        c(&registry, "pattern_stop").set_bool(true);
        c(&registry, "pattern_stop").set_bool(false);
        render(&mut bank);
        assert!(!c(&registry, "group1_enabled").get_bool());
        assert_eq!(c(&registry, "pattern1_row1").get(), 0x1111 as f64);
    }

    #[test]
    fn group_erase_removes_only_passing_events_without_changing_focus() {
        let (registry, mut bank) = fixture();
        c(&registry, "pattern1_row1").set(0xffff as f64);
        c(&registry, "group2_enabled").set_bool(true);
        for row in 0..ROWS {
            for values in [
                &bank.groups[1].engine.c.patterns,
                &bank.groups[1].engine.c.soft,
                &bank.groups[1].engine.c.accent,
                &bank.groups[1].engine.c.maybe,
            ] {
                values[0][row].set(0xffff as f64);
            }
        }
        advance(&mut bank, BLOCK);
        c(&registry, "group2_erase").set_bool(true);
        advance(&mut bank, BLOCK);
        assert_eq!(c(&registry, "group_selected").get(), 0.0);
        assert_eq!(c(&registry, "pattern1_row1").get() as u32, 0xffff);
        for row in 0..ROWS {
            for values in [
                &bank.groups[1].engine.c.patterns,
                &bank.groups[1].engine.c.soft,
                &bank.groups[1].engine.c.accent,
                &bank.groups[1].engine.c.maybe,
            ] {
                assert_eq!(values[0][row].get() as u32, 0xffff & !(1 << 1));
            }
        }
        c(&registry, "group2_erase").set_bool(false);
        advance(&mut bank, BLOCK);
        assert_ne!(
            bank.groups[1].engine.c.patterns[0][0].get() as u32 & (1 << 2),
            0
        );
    }

    #[test]
    fn a_held_erase_across_patterns_is_one_undo_with_exact_assignment_restore() {
        let (registry, mut bank) = fixture();
        for slot in [0, 1] {
            for values in [
                &bank.c.patterns,
                &bank.c.soft,
                &bank.c.accent,
                &bank.c.maybe,
            ] {
                values[slot][0].set(0xffff as f64);
            }
        }
        render(&mut bank);
        advance(&mut bank, BLOCK);
        let before = bank.cursor;
        c(&registry, "row1_erase").set_bool(true);
        advance(&mut bank, BLOCK * 2);
        c(&registry, "pattern_target").set(1.0);
        c(&registry, "pattern_select").set_bool(true);
        advance(&mut bank, 96_000);
        assert_eq!(bank.cursor, before, "the gesture is still open");
        assert_ne!(c(&registry, "pattern1_row1").get() as u32, 0xffff);
        assert_ne!(c(&registry, "pattern2_row1").get() as u32, 0xffff);
        c(&registry, "row1_erase").set_bool(false);
        advance(&mut bank, 512);
        assert_eq!(bank.cursor, before + 1);
        let erased = [bank.c.tracks(0), bank.c.tracks(1)];
        c(&registry, "undo").set_bool(true);
        render(&mut bank);
        assert_eq!(c(&registry, "groove").get(), 0.0);
        assert!(c(&registry, "group_enabled").get_bool());
        for slot in [0, 1] {
            for values in [
                &bank.c.patterns,
                &bank.c.soft,
                &bank.c.accent,
                &bank.c.maybe,
            ] {
                assert_eq!(values[slot][0].get() as u32, 0xffff);
            }
        }
        c(&registry, "redo").set_bool(true);
        render(&mut bank);
        assert_eq!(c(&registry, "groove").get(), 1.0);
        assert_eq!([bank.c.tracks(0), bank.c.tracks(1)], erased);
    }

    #[test]
    fn completed_erase_holds_never_stick_and_focus_change_releases_old_rows() {
        let (registry, mut bank) = fixture();
        c(&registry, "pattern1_row1").set(0xffff as f64);
        c(&registry, "row1_erase").set_bool(true);
        c(&registry, "row1_erase").set_bool(false);
        c(&registry, "group1_erase").set_bool(true);
        c(&registry, "group1_erase").set_bool(false);
        advance(&mut bank, BLOCK * 2);
        assert_eq!(c(&registry, "pattern1_row1").get() as u32, 0xffff);
        assert!(!bank.groups[0].engine.c.erase_rows[0].get_bool());
        c(&registry, "row1_erase").set_bool(true);
        advance(&mut bank, BLOCK);
        c(&registry, "group_selected").set(1.0);
        advance(&mut bank, BLOCK);
        assert!(!bank.groups[0].engine.c.erase_rows[0].get_bool());
        assert!(!bank.groups[1].engine.c.erase_rows[0].get_bool());
        assert!(!c(&registry, "row1_erase").get_bool());
    }

    #[test]
    fn scoped_delete_and_short_row_clear_keep_their_group_when_focus_changes() {
        let (registry, mut bank) = fixture();
        c(&registry, "pattern1_row16").set(0xffff as f64);
        c(&registry, "pattern16_row1").set(0x1111 as f64);
        bank.groups[1].engine.c.patterns[15][0].set(0x2222 as f64);
        render(&mut bank);
        let scope = group_scope(0, 0);
        let scoped = |name| registry.get(&scope, name).unwrap();
        scoped("erase_target").set(15.0);
        scoped("erase_pattern").set_bool(true);
        scoped("erase_pattern").set_bool(false);
        scoped("row_selected").set(16.0);
        scoped("row_clear").set_bool(true);
        scoped("row_clear").set_bool(false);
        c(&registry, "group_selected").set(1.0);
        render(&mut bank);
        assert_eq!(bank.groups[0].engine.c.patterns[0][15].get(), 0.0);
        assert_eq!(bank.groups[0].engine.c.patterns[15][0].get(), 0.0);
        assert_eq!(bank.groups[1].engine.c.patterns[15][0].get(), 0x2222 as f64);
        assert_eq!(scoped("erase_last_target").get(), 15.0);
        assert_eq!(scoped("erase_serial").get(), 1.0);
        assert_eq!(c(&registry, "group_selected").get(), 1.0);
        render(&mut bank);
        assert_eq!(scoped("erase_serial").get(), 1.0);
    }

    #[test]
    fn range_chain_cycles_one_bar_slots_and_manual_selection_exits_it() {
        let (registry, mut bank) = fixture();
        c(&registry, "groove").set(4.0);
        c(&registry, "pattern_chain_start").set(4.0);
        c(&registry, "pattern_chain_end").set(6.0);
        c(&registry, "pattern_chain").set_bool(true);
        render(&mut bank);
        for playing in [4.0, 5.0, 6.0, 4.0] {
            advance(&mut bank, 96_000);
            assert_eq!(c(&registry, "groove_playing").get(), playing);
            assert_eq!(c(&registry, "groove_queued").get(), playing);
            assert_eq!(c(&registry, "groove").get(), 4.0);
        }
        c(&registry, "pattern_target").set(15.0);
        c(&registry, "pattern_select").set_bool(true);
        advance(&mut bank, BLOCK);
        assert!(!c(&registry, "pattern_chain").get_bool());
        assert_eq!(c(&registry, "groove_playing").get(), 15.0);
        assert_eq!(c(&registry, "groove").get(), 15.0);
        assert!(kept_keys().iter().any(|name| name == "pattern_chain_end"));
    }

    #[test]
    fn explicit_slot_inside_a_range_takes_priority_at_the_next_bar() {
        let (registry, mut bank) = fixture();
        c(&registry, "groove").set(13.0);
        c(&registry, "pattern_chain_start").set(13.0);
        c(&registry, "pattern_chain_end").set(15.0);
        c(&registry, "pattern_chain").set_bool(true);
        advance(&mut bank, BLOCK);
        c(&registry, "groove").set(15.0);
        advance(&mut bank, BLOCK);
        assert_eq!(c(&registry, "groove_playing").get(), 13.0);
        assert_eq!(c(&registry, "groove_queued").get(), 15.0);
        advance(&mut bank, 96_000 - 2 * BLOCK + 1);
        assert_eq!(c(&registry, "groove_playing").get(), 15.0);
        advance(&mut bank, 96_000);
        assert_eq!(c(&registry, "groove_playing").get(), 13.0);
        assert_eq!(c(&registry, "groove_queued").get(), 13.0);
        assert_eq!(c(&registry, "groove").get(), 15.0);
        assert!(c(&registry, "pattern_chain").get_bool());
    }

    #[test]
    fn selecting_and_editing_in_one_buffer_targets_the_new_group() {
        let (registry, mut bank) = fixture();
        c(&registry, "pattern1_row1").set(5.0);
        render(&mut bank);
        c(&registry, "group_selected").set(1.0);
        c(&registry, "pattern1_row1").set(9.0);
        render(&mut bank);
        assert_eq!(bank.groups[0].engine.c.patterns[0][0].get(), 5.0);
        assert_eq!(bank.groups[1].engine.c.patterns[0][0].get(), 9.0);
        c(&registry, "group_selected").set(0.0);
        render(&mut bank);
        assert_eq!(c(&registry, "pattern1_row1").get(), 5.0);
    }

    #[test]
    fn pad_after_selection_in_one_buffer_plays_the_new_group() {
        let (registry, mut bank) = fixture();
        c(&registry, "pad_mode").set(1.0);
        c(&registry, "group_selected").set(1.0);
        c(&registry, "pad16").set(1.0);
        let out = render(&mut bank);
        assert!(out.iter().any(|frame| frame[0] > 0.0));
        assert_eq!(bank.groups[0].engine.c.meter[15].get(), 0.0);
        assert!(bank.groups[1].engine.c.meter[15].get() > 0.0);
    }

    #[test]
    fn complete_short_clear_is_consumed() {
        let (registry, mut bank) = fixture();
        c(&registry, "pattern1_row1").set(15.0);
        render(&mut bank);
        c(&registry, "row_clear").set(1.0);
        c(&registry, "row_clear").set(0.0);
        render(&mut bank);
        assert_eq!(c(&registry, "pattern1_row1").get(), 0.0);
    }

    #[test]
    fn group_mute_silences_manual_audio_and_duck_events() {
        let (registry, mut bank) = fixture();
        c(&registry, "pad_mode").set(1.0);
        c(&registry, "group1_mute").set_bool(true);
        c(&registry, "pad1").set(1.0);
        let mut out = vec![[0.0; 2]; BLOCK];
        let mut kicks = 0;
        bank.render(
            false,
            Source {
                bpm: 120.0,
                beat: None,
            },
            &mut out,
            |_| kicks += 1,
        );
        assert!(out.iter().all(|frame| *frame == [0.0; 2]));
        assert_eq!(kicks, 0);
    }

    #[test]
    fn changing_kit_keeps_notes_and_assigns_the_same_instrument_roles() {
        let (registry, mut bank) = fixture();
        c(&registry, "pattern1_row1").set(0x5151 as f64);
        c(&registry, "pattern2_row16").set(0x0101 as f64);
        render(&mut bank);
        let before = std::array::from_fn::<_, GROOVES, _>(|pattern| {
            std::array::from_fn::<_, ROWS, _>(|row| {
                bank.groups[0].engine.c.patterns[pattern][row].get()
            })
        });
        c(&registry, "kit").set(1.0);
        render(&mut bank);
        let after = std::array::from_fn::<_, GROOVES, _>(|pattern| {
            std::array::from_fn::<_, ROWS, _>(|row| {
                bank.groups[0].engine.c.patterns[pattern][row].get()
            })
        });
        assert_eq!(before, after);
        assert_eq!(
            std::array::from_fn::<_, ROWS, _>(
                |row| bank.groups[0].engine.c.sound[row].get() as usize
            ),
            kits::default_row_map(1)
        );
    }

    #[test]
    fn undo_after_kit_change_restores_custom_sound_assignments() {
        let (registry, mut bank) = fixture();
        c(&registry, "kit").set(1.0);
        c(&registry, "row1_sound").set(15.0);
        render(&mut bank);
        c(&registry, "kit").set(0.0);
        render(&mut bank);
        assert_eq!(c(&registry, "row1_sound").get(), 0.0);
        c(&registry, "undo").set_bool(true);
        render(&mut bank);
        assert_eq!(c(&registry, "kit").get(), 1.0);
        assert_eq!(c(&registry, "row1_sound").get(), 15.0);
    }

    #[test]
    fn copying_and_undo_restore_group_controls_and_sound_mapping() {
        let (registry, mut bank) = fixture();
        c(&registry, "row1_sound").set(15.0);
        c(&registry, "group1_mute").set_bool(true);
        render(&mut bank);
        c(&registry, "copy_kind").set(2.0);
        c(&registry, "copy_source").set(0.0);
        c(&registry, "copy_target").set(1.0);
        c(&registry, "copy").set_bool(true);
        render(&mut bank);
        assert_eq!(bank.groups[1].engine.c.sound[0].get(), 15.0);
        assert!(bank.groups[1].mute.get_bool());
        assert_eq!(c(&registry, "copy_serial").get(), 1.0);
        c(&registry, "undo").set_bool(true);
        render(&mut bank);
        assert_eq!(bank.groups[1].engine.c.sound[0].get(), 0.0);
        assert!(!bank.groups[1].mute.get_bool());
        c(&registry, "redo").set_bool(true);
        render(&mut bank);
        assert_eq!(bank.groups[1].engine.c.sound[0].get(), 15.0);
        assert!(bank.groups[1].mute.get_bool());
    }
}
