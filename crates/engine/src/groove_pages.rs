//! Length, metre and packed pages for native Groove patterns.
use opendj_control::{Behavior, ControlHandle, ControlRegistry};

use crate::groove::{GROOVES, ROWS};

pub const CHUNK_STEPS: usize = 32;
pub const CHUNKS: usize = 4;
pub const PAGE_STEPS: usize = 16;
pub const MAX_STEPS: usize = CHUNK_STEPS * CHUNKS;
pub const PAGES: usize = MAX_STEPS / PAGE_STEPS;
pub const SUFFIXES: [&str; 4] = ["", "_soft", "_accent", "_maybe"];
pub const DENOMINATORS: [usize; 4] = [2, 4, 8, 16];

pub struct Chunk {
    pub masks: [[[ControlHandle; ROWS]; GROOVES]; 4],
}

impl Chunk {
    fn new(registry: &ControlRegistry, scope: &str, chunk: usize) -> Self {
        Self {
            masks: std::array::from_fn(|kind| {
                std::array::from_fn(|pattern| {
                    std::array::from_fn(|row| {
                        registry.create(
                            scope,
                            &mask_key(pattern, chunk, row, kind),
                            0.0,
                            0.0,
                            u32::MAX as f64,
                            Behavior::Free,
                        )
                    })
                })
            }),
        }
    }
}

pub struct ExtendedPatterns {
    pub chunks: Box<[Chunk]>,
    pub length: [ControlHandle; GROOVES],
    pub numerator: [ControlHandle; GROOVES],
    pub unit: [ControlHandle; GROOVES],
    pub page: ControlHandle,
    pub follow: ControlHandle,
}

impl ExtendedPatterns {
    pub fn new(registry: &ControlRegistry, scope: &str) -> Self {
        registry
            .indicator(scope, "pattern_length_version", 1.0)
            .set(1.0);
        registry
            .indicator(scope, "pattern_max_steps", MAX_STEPS as f64)
            .set(MAX_STEPS as f64);
        Self {
            chunks: (1..CHUNKS)
                .map(|chunk| Chunk::new(registry, scope, chunk))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            length: std::array::from_fn(|pattern| {
                registry.create(
                    scope,
                    &format!("pattern{}_length", pattern + 1),
                    16.0,
                    1.0,
                    MAX_STEPS as f64,
                    Behavior::Free,
                )
            }),
            numerator: std::array::from_fn(|pattern| {
                registry.create(
                    scope,
                    &format!("pattern{}_meter_num", pattern + 1),
                    4.0,
                    1.0,
                    16.0,
                    Behavior::Free,
                )
            }),
            unit: std::array::from_fn(|pattern| {
                registry.create(
                    scope,
                    &format!("pattern{}_meter_unit", pattern + 1),
                    1.0,
                    0.0,
                    (DENOMINATORS.len() - 1) as f64,
                    Behavior::Free,
                )
            }),
            page: registry.create(
                scope,
                "step_page",
                0.0,
                0.0,
                (PAGES - 1) as f64,
                Behavior::Free,
            ),
            follow: registry.create(scope, "step_page_follow", 1.0, 0.0, 1.0, Behavior::Free),
        }
    }

    pub fn steps(&self, pattern: usize) -> usize {
        (self.length[pattern].get().round() as usize).clamp(1, MAX_STEPS)
    }

    pub fn bar_steps(&self, pattern: usize) -> usize {
        let numerator = (self.numerator[pattern].get().round() as usize).clamp(1, 16);
        let unit = (self.unit[pattern].get().round() as usize).min(DENOMINATORS.len() - 1);
        numerator * 16 / DENOMINATORS[unit]
    }

    pub fn reset_length(&self, pattern: usize) {
        self.length[pattern].set(16.0);
        self.numerator[pattern].set(4.0);
        self.unit[pattern].set(1.0);
    }
}

/// Chunk zero retains the pre-existing names and the low sixteen bits.
pub fn mask_key(pattern: usize, chunk: usize, row: usize, kind: usize) -> String {
    let prefix = if chunk == 0 {
        format!("pattern{}", pattern + 1)
    } else {
        format!("pattern{}_chunk{}", pattern + 1, chunk + 1)
    };
    format!("{prefix}_row{}{}", row + 1, SUFFIXES[kind])
}

pub fn state_keys() -> Vec<String> {
    let mut keys = Vec::new();
    for pattern in 0..GROOVES {
        for field in ["length", "meter_num", "meter_unit"] {
            keys.push(format!("pattern{}_{field}", pattern + 1));
        }
        for chunk in 0..CHUNKS {
            for row in 0..ROWS {
                for kind in 0..SUFFIXES.len() {
                    keys.push(mask_key(pattern, chunk, row, kind));
                }
            }
        }
    }
    keys
}

pub const VIEW_KEYS: [&str; 2] = ["step_page", "step_page_follow"];
