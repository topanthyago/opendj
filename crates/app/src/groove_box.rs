//! The app's side of the Groove Box (`opendj_engine::groove`): the kits'
//! and the groove library's names for the screen, and the Groove Boxes kept
//! across restarts in `settings.json` under `groove_box`, one object per
//! deck of its controls' values.

use std::time::{Duration, Instant};

use opendj_control::{ControlHandle, ControlRegistry};
use opendj_engine::groove::{groove_group, GROOVES, ROWS};
use opendj_engine::NUM_DECKS;
use opendj_engine::{kits, patterns};
use serde_json::{json, Map, Value};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::{settings, AppWindow, GrooveGenre, GrooveKit, GrooveName, GrooveUi};

/// The layout of the saved Groove Boxes. Before 2 there were four kits
/// (now from index 2) and four rows, whose sounds were numbered in each
/// kit's own order; before 3, nine rows (8 crash, 9 ride), now eight.
pub(crate) const LAYOUT: f64 = 3.0;

/// Slint uses f32, which cannot preserve every bit of a packed u32 pattern.
pub(crate) fn mask_page(value: f64, shift: i32) -> f32 {
    ((value as u32 >> shift.clamp(0, 16)) & 0xffff) as f32
}

pub(crate) fn set_mask_step(control: &ControlHandle, step: i32, on: bool) {
    if !(0..32).contains(&step) {
        return;
    }
    let bit = 1u32 << step;
    let value = control.get() as u32;
    control.set(if on { value | bit } else { value & !bit } as f64);
}

/// The controls a Groove Box keeps, by name.
pub(crate) fn kept_keys() -> Vec<String> {
    let mut keys: Vec<String> = [
        "enabled",
        "kit",
        "groove",
        "genre",
        "pattern",
        "library_set",
        "chain",
        "pattern_chain",
        "pattern_chain_start",
        "pattern_chain_end",
        "bpm",
        "follow",
        "tempo_scale",
        "kick_key",
        "vary",
        "auto_fill",
        "auto_fill_style",
        "follow_breaks",
        "break_sensitivity",
        "swing",
        "repeat_rate",
        "duck_deck",
        "duck_amount",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    keys.extend(opendj_engine::groove_pages::state_keys());
    keys.extend(opendj_engine::groove_pages::VIEW_KEYS.map(String::from));
    for r in 1..=ROWS {
        for k in [
            "mute", "solo", "level", "pitch", "decay", "tone", "pan", "sound", "lock",
        ] {
            keys.push(format!("row{r}_{k}"));
        }
    }
    keys
}

/// Shows the kits' names and the groove library's.
pub fn install(app: &AppWindow) {
    let ui = app.global::<GrooveUi>();
    let kits: Vec<GrooveKit> = kits::infos()
        .iter()
        .map(|k| GrooveKit {
            name: k.name.into(),
            about: k.about.into(),
            sounds: ModelRc::new(VecModel::from(
                k.sounds().map(SharedString::from).collect::<Vec<_>>(),
            )),
        })
        .collect();
    ui.set_kits(ModelRc::new(VecModel::from(kits)));
    let genres: Vec<GrooveGenre> = patterns::library()
        .iter()
        .map(|g| GrooveGenre {
            name: g.name.as_str().into(),
            grooves: ModelRc::new(VecModel::from(
                g.grooves
                    .iter()
                    .map(|p| GrooveName {
                        name: p.name.as_str().into(),
                        about: p.about.as_str().into(),
                    })
                    .collect::<Vec<_>>(),
            )),
        })
        .collect();
    ui.set_genres(ModelRc::new(VecModel::from(genres)));
}

/// Brings a Groove Box saved by an older openDJ up to [`LAYOUT`]: the kit
/// it picked moves to its new place, and its rows' sounds go back to the
/// rows' own (rows 1-4 were already kick, snare, hat and open hat).
pub(crate) fn upgrade(values: &mut Map<String, Value>) {
    let layout = values.get("layout").and_then(Value::as_f64).unwrap_or(1.0);
    if layout < 2.0 {
        if let Some(kit) = values.get("kit").and_then(Value::as_f64) {
            values.insert("kit".into(), json!(kit + 2.0));
        }
        for r in 1..=4 {
            values.remove(&format!("row{r}_sound"));
        }
    }
    if layout < 3.0 {
        merge_ninth_row(values);
    }
    let has_patterns = values
        .keys()
        .any(|key| key.starts_with("pattern") && key.contains("_row"));
    let has_lengths = values
        .keys()
        .any(|key| key.starts_with("pattern") && key.ends_with("_length"));
    if has_patterns && !has_lengths {
        values.insert("step_page".into(), json!(0));
        for pattern in 0..GROOVES {
            values.insert(format!("pattern{}_length", pattern + 1), json!(16));
            values.insert(format!("pattern{}_meter_num", pattern + 1), json!(4));
            values.insert(format!("pattern{}_meter_unit", pattern + 1), json!(1));
            for chunk in 1..opendj_engine::groove_pages::CHUNKS {
                for row in 0..ROWS {
                    for kind in 0..4 {
                        values.insert(
                            opendj_engine::groove_pages::mask_key(pattern, chunk, row, kind),
                            json!(0),
                        );
                    }
                }
            }
        }
    }
}

/// Moves the old ninth row (ride) into row 8 (Cymbal) wherever row 8 (the
/// crash) has no hits, and has row 8 play the ride if it took any.
fn merge_ninth_row(values: &mut Map<String, Value>) {
    let num = |v: &Map<String, Value>, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let mut took_ride = false;
    for g in 1..=GROOVES {
        let (crash, ride) = (format!("pattern{g}_row8"), format!("pattern{g}_row9"));
        if num(values, &crash) == 0.0 && num(values, &ride) != 0.0 {
            took_ride = true;
            for part in ["", "_soft", "_accent"] {
                let v = num(values, &format!("{ride}{part}"));
                values.insert(format!("{crash}{part}"), json!(v));
            }
        }
        for part in ["", "_soft", "_accent", "_maybe"] {
            values.remove(&format!("{ride}{part}"));
        }
    }
    if took_ride && num(values, "row8_sound") <= 7.0 {
        values.insert("row8_sound".into(), json!(8.0));
    }
    for k in [
        "mute", "solo", "level", "pitch", "decay", "tone", "pan", "sound",
    ] {
        values.remove(&format!("row9_{k}"));
    }
}

/// Keeps the Groove Boxes: puts back what was saved, then saves what
/// changes (looked at every couple of seconds).
pub struct Keeper {
    scopes: Vec<String>,
    controls: Vec<Vec<(String, ControlHandle)>>,
    seen: Vec<Vec<f64>>,
    last_look: Instant,
}

impl Keeper {
    pub fn new(registry: &ControlRegistry) -> Self {
        let mut keys = kept_keys();
        keys.push("group_selected".into());
        for group in 1..=opendj_engine::groove_banks::GROUPS {
            for name in ["enabled", "mute", "solo", "level", "color"] {
                keys.push(format!("group{group}_{name}"));
            }
        }
        let mut scopes: Vec<String> = (0..NUM_DECKS).map(groove_group).collect();
        let mut controls: Vec<Vec<(String, ControlHandle)>> = (0..NUM_DECKS)
            .map(|d| {
                let g = groove_group(d);
                keys.iter()
                    .filter_map(|k| Some((k.clone(), registry.get(&g, k)?)))
                    // The input the deck plays instead (see
                    // `crate::instruments`), kept beside its Groove Box.
                    .chain(
                        ["live_input", "live_trim", "live_pan"]
                            .into_iter()
                            .filter_map(|k| {
                                registry
                                    .get(&format!("[Channel{}]", d + 1), k)
                                    .map(|c| (k.to_string(), c))
                            }),
                    )
                    .collect()
            })
            .collect();
        let mut group_keys = opendj_engine::groove_banks::kept_keys();
        group_keys.extend(opendj_engine::groove_pages::VIEW_KEYS.map(String::from));
        group_keys.push("initialized".into());
        for row in 1..=ROWS {
            group_keys.push(format!("row{row}_color"));
        }
        for deck in 0..NUM_DECKS {
            for group in 0..opendj_engine::groove_banks::GROUPS {
                let scope = opendj_engine::groove_banks::group_scope(deck, group);
                controls.push(
                    group_keys
                        .iter()
                        .filter_map(|key| {
                            registry
                                .get(&scope, key)
                                .map(|control| (key.clone(), control))
                        })
                        .collect(),
                );
                scopes.push(scope);
            }
        }
        let saved = settings::load()["groove_box"].clone();
        for (scope, deck) in scopes.iter().zip(&controls) {
            let Some(mut values) = saved[scope].as_object().cloned() else {
                continue;
            };
            upgrade(&mut values);
            for (key, c) in deck {
                if let Some(v) = values.get(key).and_then(Value::as_f64) {
                    c.set(v);
                }
            }
        }
        let seen = snapshot(&controls);
        Self {
            scopes,
            controls,
            seen,
            last_look: Instant::now(),
        }
    }

    /// Saves the Groove Boxes if anything changed since the last look.
    pub fn tick(&mut self) {
        if self.last_look.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_look = Instant::now();
        let now = snapshot(&self.controls);
        if now == self.seen {
            return;
        }
        self.seen = now;
        let mut all = Map::new();
        for (scope, deck) in self.scopes.iter().zip(&self.controls) {
            let mut values: Map<String, Value> = deck
                .iter()
                .map(|(k, c)| (k.clone(), json!(c.get())))
                .collect();
            values.insert("layout".into(), json!(LAYOUT));
            all.insert(scope.clone(), Value::Object(values));
        }
        settings::set("groove_box", Value::Object(all));
    }
}

fn snapshot(controls: &[Vec<(String, ControlHandle)>]) -> Vec<Vec<f64>> {
    controls
        .iter()
        .map(|deck| deck.iter().map(|(_, c)| c.get()).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skin_edits_preserve_every_bit_of_both_packed_pages() {
        let registry = ControlRegistry::new();
        let control = registry.create(
            "[Test]",
            "mask",
            0.0,
            0.0,
            u32::MAX as f64,
            opendj_control::Behavior::Free,
        );
        control.set(u32::MAX as f64);
        assert_eq!(mask_page(control.get(), 0), 65535.0);
        assert_eq!(mask_page(control.get(), 16), 65535.0);
        set_mask_step(&control, 31, false);
        set_mask_step(&control, 0, false);
        assert_eq!(control.get(), 0x7ffffffeu32 as f64);
        set_mask_step(&control, 31, true);
        set_mask_step(&control, 0, true);
        assert_eq!(control.get(), u32::MAX as f64);
        set_mask_step(&control, -1, false);
        set_mask_step(&control, 32, false);
        assert_eq!(control.get(), u32::MAX as f64);
    }

    #[test]
    fn every_kept_control_exists() {
        let reg = ControlRegistry::new();
        let (_engine, _input) = opendj_engine::engine::Engine::new(&reg, 48_000.0);
        for key in kept_keys() {
            assert!(reg.get("[Groove1]", &key).is_some(), "{key}");
        }
    }

    #[test]
    fn every_kept_control_exists_before_audio_starts() {
        let reg = ControlRegistry::new();
        opendj_engine::groove::create_restore_controls(&reg);
        for deck in 0..NUM_DECKS {
            for key in kept_keys() {
                assert!(reg.get(&groove_group(deck), &key).is_some(), "{key}");
            }
            for key in ["live_input", "live_trim", "live_pan"] {
                assert!(reg.get(&format!("[Channel{}]", deck + 1), key).is_some());
            }
        }
    }

    #[test]
    fn settings_and_presets_keep_break_preferences_but_not_actions() {
        let keys = kept_keys();
        for key in [
            "library_set",
            "auto_fill_style",
            "follow_breaks",
            "break_sensitivity",
        ] {
            assert!(keys.iter().any(|k| k == key), "{key}");
        }
        for key in ["load_set", "set_pending", "break_active"] {
            assert!(!keys.iter().any(|k| k == key), "{key}");
        }
        let mut values = json!({"layout": 3.0, "follow_breaks": 1.0, "break_sensitivity": 0.7});
        let before = values.clone();
        upgrade(values.as_object_mut().unwrap());
        assert_eq!(values, before);
    }

    #[test]
    fn the_old_ninth_row_moves_into_the_cymbal_track() {
        let mut old = json!({
            "layout": 2.0,
            "pattern1_row9": 4369.0, "pattern1_row9_accent": 1.0,
            "pattern2_row8": 1.0, "pattern2_row9": 2.0,
            "row9_level": 0.5,
        });
        let v = old.as_object_mut().unwrap();
        upgrade(v);
        assert_eq!(v["pattern1_row8"], json!(4369.0), "the ride moves over");
        assert_eq!(v["pattern1_row8_accent"], json!(1.0));
        assert_eq!(v["pattern2_row8"], json!(1.0), "a crash stays");
        assert_eq!(v["row8_sound"], json!(8.0), "and plays the ride");
        assert!(!v.contains_key("pattern1_row9"));
        assert!(!v.contains_key("pattern2_row9"));
        assert!(!v.contains_key("row9_level"));
    }

    #[test]
    fn an_old_groove_box_moves_to_the_new_kits() {
        let mut old = json!({"kit": 1.0, "row2_sound": 1.0, "row2_level": 0.8});
        let values = old.as_object_mut().unwrap();
        upgrade(values);
        assert_eq!(values["kit"], json!(3.0), "Punch is now the fourth kit");
        assert!(values.get("row2_sound").is_none());
        assert_eq!(values["row2_level"], json!(0.8));
        let mut new = json!({"kit": 1.0, "layout": LAYOUT});
        upgrade(new.as_object_mut().unwrap());
        assert_eq!(new["kit"], json!(1.0));
    }

    #[test]
    fn legacy_pattern_data_gets_one_bar_and_empty_extra_chunks() {
        let mut values = json!({"layout": 3.0, "kit": 8.0, "pattern1_row1": 4369.0});
        upgrade(values.as_object_mut().unwrap());
        assert_eq!(values["pattern1_length"], json!(16));
        assert_eq!(values["pattern1_meter_num"], json!(4));
        assert_eq!(values["pattern1_meter_unit"], json!(1));
        assert_eq!(values["pattern1_row1"], json!(4369.0));
        assert_eq!(values["pattern16_chunk4_row16_maybe"], json!(0));
    }

    #[test]
    fn extended_presets_and_kit_only_presets_keep_their_fields() {
        for mut values in [
            json!({"layout": 3.0, "pattern1_length": 64.0, "pattern1_chunk2_row1": 2147483648.0}),
            json!({"layout": 3.0, "kit": 12.0}),
        ] {
            let before = values.clone();
            upgrade(values.as_object_mut().unwrap());
            assert_eq!(values, before);
        }
    }
}
