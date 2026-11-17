//! The Groove Box's pad surface: what 16 pads on a controller do, and how
//! they light, the same for every pad controller.
//!
//! A controller mapping only passes presses in and colours out:
//!
//! - `[GrooveN],padK` (K 1-16, as the controller numbers its pads): set it
//!   to how hard the pad is pressed (0-1, 1 for a pad without pressure)
//!   and back to 0 on release.
//! - `[GrooveN],padK_color`: what pad K shows, one of the [`light`]
//!   colours; the mapping turns it into the device's own colour.
//!
//! The rest is here, so every controller behaves alike. `[GrooveN],pad_mode`
//! picks what the pads do:
//!
//! - **Steps** (0): the 16 steps of the selected row (`row_selected`, 1-16)
//!   of the groove on screen. A press adds or removes a hit; while stopped
//!   it also plays the sound once.
//! - **Play** (1): pads 1-16 play rows 1-16, now, as loud
//!   as the pad is hit. With `record` on while playing, the hit is also written
//!   into the nearest step of the groove playing.
//! - **Grooves** (2): pads 1-4 pick groove A-D (on the next bar, as the
//!   screen's buttons do), 5-8 the kit, 9-16 load the first eight grooves
//!   of the genre picked into the groove playing.
//!
//! `row_clear` empties the selected row of the groove on screen.
//! `note_repeat` held repeats Play pads on the audio clock. `repeat_rate`
//! selects quarter, eighth, sixteenth, thirty-second, then their triplets.

use opendj_control::{Behavior, ControlHandle, ControlRegistry};

use crate::groove::{groove_group, LIBRARY_SET_SIZE, ROWS, STEPS};

/// The rows the pads play in Play mode.
pub const PLAY_ROWS: usize = ROWS;
use crate::kits;

pub const PADS: usize = 16;
pub const REPEAT_BEATS: [f64; 8] = [
    1.0,
    0.5,
    0.25,
    0.125,
    2.0 / 3.0,
    1.0 / 3.0,
    1.0 / 6.0,
    1.0 / 12.0,
];

/// What `[GrooveN],pad_mode` holds.
pub mod mode {
    pub const STEPS: u8 = 0;
    pub const PLAY: u8 = 1;
    pub const GROOVES: u8 = 2;
}

/// The colours a pad shows, as `[GrooveN],padK_color` holds them.
pub mod light {
    pub const OFF: u8 = 0;
    /// Rows in their own colour: `ROW + row` (row 0-7).
    pub const ROW: u8 = 1;
    /// The step playing.
    pub const PLAYHEAD: u8 = 9;
    /// What is picked: the groove playing, the kit.
    pub const SELECTED: u8 = 10;
    /// Something a press would pick, or a beat mark in an empty row.
    pub const DIM: u8 = 11;
    /// The groove waiting for the bar.
    pub const QUEUED: u8 = 12;
    /// Rows dimmed: `ROW_DIM + row` (row 0-7).
    pub const ROW_DIM: u8 = 13;
    pub const MAX: u8 = 20;

    /// Row `row`'s own colour.
    pub fn row(row: usize) -> u8 {
        ROW + (row % 8) as u8
    }

    /// Row `row`'s colour, dimmed.
    pub fn row_dim(row: usize) -> u8 {
        ROW_DIM + (row % 8) as u8
    }
}

/// A press seen this buffer: the pad (0-15) and how hard (0-1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Press {
    pub pad: usize,
    pub velocity: f32,
}

pub struct PadControls {
    pub mode: ControlHandle,
    pub row: ControlHandle,
    pub record: ControlHandle,
    pub row_clear: ControlHandle,
    pub note_repeat: ControlHandle,
    pub repeat_rate: ControlHandle,
    pub pads: [ControlHandle; PADS],
    pub colors: [ControlHandle; PADS],
}

impl PadControls {
    pub fn new(reg: &ControlRegistry, deck: usize) -> Self {
        Self::new_scoped(reg, &groove_group(deck))
    }

    pub fn new_scoped(reg: &ControlRegistry, g: &str) -> Self {
        reg.indicator(g, "note_repeat_supported", 1.0).set(1.0);
        Self {
            mode: reg.create(g, "pad_mode", 0.0, 0.0, 2.0, Behavior::Free),
            row: reg.create(g, "row_selected", 1.0, 1.0, ROWS as f64, Behavior::Free),
            record: reg.toggle(g, "record"),
            row_clear: reg.push(g, "row_clear"),
            note_repeat: reg.toggle(g, "note_repeat"),
            repeat_rate: reg.create(
                g,
                "repeat_rate",
                2.0,
                0.0,
                (REPEAT_BEATS.len() - 1) as f64,
                Behavior::Free,
            ),
            pads: std::array::from_fn(|k| {
                reg.create(g, &format!("pad{}", k + 1), 0.0, 0.0, 1.0, Behavior::Free)
            }),
            colors: std::array::from_fn(|k| {
                reg.indicator(g, &format!("pad{}_color", k + 1), light::MAX as f64)
            }),
        }
    }
}

/// Reads the pads each buffer and keeps their colours.
pub struct PadSurface {
    pub c: PadControls,
    last: [f64; PADS],
    seen: [u32; PADS],
    clear_seen: u32,
}

impl PadSurface {
    pub fn new(reg: &ControlRegistry, deck: usize) -> Self {
        Self::new_scoped(reg, &groove_group(deck))
    }

    pub fn new_scoped(reg: &ControlRegistry, group: &str) -> Self {
        let c = PadControls::new_scoped(reg, group);
        let seen = std::array::from_fn(|k| c.pads[k].version());
        let clear_seen = c.row_clear.version();
        Self {
            c,
            last: [0.0; PADS],
            seen,
            clear_seen,
        }
    }

    pub fn mode(&self) -> u8 {
        (self.c.mode.get().round() as u8).min(mode::GROOVES)
    }

    /// The selected row, 0-15.
    pub fn row(&self) -> usize {
        (self.c.row.get().round() as usize).clamp(1, ROWS) - 1
    }

    /// Whether pad `k` is held down now.
    pub fn held(&self, k: usize) -> bool {
        self.last[k] > 0.0
    }

    pub fn velocity(&self, k: usize) -> f32 {
        self.last[k].clamp(0.0, 1.0) as f32
    }

    /// Release held pads when a page changes, preserving new input that has
    /// arrived since the preceding audio buffer.
    pub fn release_unchanged(&mut self) {
        for k in 0..PADS {
            if self.c.pads[k].version() == self.seen[k] {
                self.c.pads[k].set(0.0);
                self.last[k] = 0.0;
                self.seen[k] = self.c.pads[k].version();
            }
        }
    }

    /// The presses since the last call. A press and release between two
    /// buffers still counts: the control changed but is back at 0.
    pub fn presses(&mut self, mut each: impl FnMut(Press)) {
        for k in 0..PADS {
            let v = self.c.pads[k].get();
            let version = self.c.pads[k].version();
            let changed = version != self.seen[k];
            let was = self.last[k];
            self.seen[k] = version;
            self.last[k] = v;
            if was > 0.0 {
                continue;
            }
            if v > 0.0 {
                each(Press {
                    pad: k,
                    velocity: v as f32,
                });
            } else if changed {
                each(Press {
                    pad: k,
                    velocity: 1.0,
                });
            }
        }
    }

    /// Whether `row_clear` was pressed (and lets it go).
    pub fn take_clear(&mut self) -> bool {
        let on = self.c.row_clear.get_bool() || self.c.row_clear.version() != self.clear_seen;
        if on {
            self.c.row_clear.set(0.0);
        }
        self.clear_seen = self.c.row_clear.version();
        on
    }

    pub fn show(&self, colors: &[u8; PADS]) {
        for (c, &v) in self.c.colors.iter().zip(colors) {
            c.set(v as f64);
        }
    }
}

/// The row pad `k` plays in Play mode.
pub fn play_row(k: usize) -> usize {
    k % PLAY_ROWS
}

/// What the pads show, worked out from the Groove Box's state.
pub struct View<'a> {
    pub mode: u8,
    pub row: usize,
    /// The selected row's steps in the groove on screen.
    pub steps: u32,
    /// The step playing, when the groove on screen is the one playing.
    pub playhead: Option<usize>,
    pub held: &'a dyn Fn(usize) -> bool,
    pub muted: &'a dyn Fn(usize) -> bool,
    pub playing: usize,
    pub queued: Option<usize>,
    pub kit: usize,
    /// How many library grooves pads 9-16 load (the genre's first eight).
    pub grooves: usize,
}

pub fn colors(v: &View) -> [u8; PADS] {
    std::array::from_fn(|k| match v.mode {
        mode::PLAY => {
            let row = play_row(k);
            if (v.held)(k) {
                light::PLAYHEAD
            } else if (v.muted)(row) {
                light::DIM
            } else {
                light::row_dim(row)
            }
        }
        mode::GROOVES => {
            if k < LIBRARY_SET_SIZE {
                if v.queued == Some(k) {
                    light::QUEUED
                } else if k == v.playing {
                    light::SELECTED
                } else {
                    light::DIM
                }
            } else if k < 4 + kits::KITS.len().min(4) {
                if k - 4 == v.kit {
                    light::SELECTED
                } else {
                    light::DIM
                }
            } else if (8..8 + v.grooves.min(8)).contains(&k) {
                if (v.held)(k) {
                    light::SELECTED
                } else {
                    light::row_dim(k - 8)
                }
            } else {
                light::OFF
            }
        }
        _ => {
            if v.playhead == Some(k) {
                light::PLAYHEAD
            } else if v.steps & (1 << k) != 0 {
                light::row(v.row)
            } else if k % 4 == 0 {
                light::DIM
            } else {
                light::OFF
            }
        }
    })
}

/// The step a live hit at `beat` is written into: the nearest 16th.
pub fn nearest_step(beat: f64) -> usize {
    ((beat * 4.0).round() as i64).rem_euclid(STEPS as i64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_counts_once_and_a_quick_tap_still_counts() {
        let reg = ControlRegistry::default();
        let mut s = PadSurface::new(&reg, 0);
        let mut got = Vec::new();
        s.c.pads[2].set(0.6);
        s.presses(|p| got.push(p));
        // Still held: no new press.
        s.presses(|p| got.push(p));
        s.c.pads[2].set(0.0);
        s.presses(|p| got.push(p));
        // Pressed and let go between two buffers.
        s.c.pads[5].set(1.0);
        s.c.pads[5].set(0.0);
        s.presses(|p| got.push(p));
        assert_eq!(
            got,
            vec![
                Press {
                    pad: 2,
                    velocity: 0.6
                },
                Press {
                    pad: 5,
                    velocity: 1.0
                }
            ]
        );
    }

    fn view(mode: u8) -> [u8; PADS] {
        colors(&View {
            mode,
            row: 1,
            steps: 0b1000_0000_0000_0011,
            playhead: Some(4),
            held: &|k| k == 9,
            muted: &|r| r == 3,
            playing: 1,
            queued: Some(2),
            kit: 0,
            grooves: 6,
        })
    }

    #[test]
    fn steps_show_the_row_and_the_playhead() {
        let c = view(mode::STEPS);
        assert_eq!(c[0], light::ROW + 1);
        assert_eq!(c[1], light::ROW + 1);
        assert_eq!(c[15], light::ROW + 1);
        assert_eq!(c[4], light::PLAYHEAD);
        assert_eq!(c[8], light::DIM, "beat mark");
        assert_eq!(c[2], light::OFF);
    }

    #[test]
    fn play_shows_a_pad_per_row() {
        let c = view(mode::PLAY);
        assert_eq!(c[0], light::ROW_DIM);
        assert_eq!(c[8], light::ROW_DIM);
        assert_eq!(play_row(8), 8);
        assert_eq!(play_row(15), 15);
        assert_eq!(c[4], light::ROW_DIM + 4);
        assert_eq!(c[9], light::PLAYHEAD, "held");
        assert_eq!(c[3], light::DIM, "muted row");
    }

    #[test]
    fn grooves_show_playing_queued_and_kit() {
        let c = view(mode::GROOVES);
        assert_eq!(c[1], light::SELECTED);
        assert_eq!(c[2], light::QUEUED);
        assert_eq!(c[0], light::DIM);
        assert_eq!(c[4], light::SELECTED, "kit 1");
        assert_eq!(c[5], light::DIM);
        assert_eq!(c[9], light::SELECTED, "groove held");
        assert_eq!(c[14], light::OFF, "no seventh groove");
    }

    #[test]
    fn colors_repeat_for_the_second_eight_rows() {
        assert_eq!(light::row(7), light::ROW + 7);
        assert_eq!(light::row_dim(7), light::ROW_DIM + 7);
        assert_eq!(light::row(8), light::ROW);
        assert_eq!(light::row_dim(15), light::ROW_DIM + 7);
    }

    #[test]
    fn hits_go_to_the_nearest_step() {
        assert_eq!(nearest_step(0.0), 0);
        assert_eq!(nearest_step(0.2), 1);
        assert_eq!(nearest_step(3.9), 0, "wraps to the bar's first step");
    }
}
