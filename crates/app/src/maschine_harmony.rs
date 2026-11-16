use std::sync::Arc;

use opendj_control::ControlRegistry;
use opendj_engine::{
    harmony,
    key::{Key, Mixing},
    maschine::Project,
    scale,
};
use slint::ComponentHandle;

use crate::{AppWindow, MaschineUi};

#[derive(Clone)]
pub struct Harmony {
    registry: Arc<ControlRegistry>,
}

impl Harmony {
    pub fn install(app: &AppWindow, registry: Arc<ControlRegistry>) -> Self {
        app.global::<MaschineUi>()
            .on_harmonic_note(|pitch, mask| mask == 0 || mask & (1 << pitch.rem_euclid(12)) != 0);
        app.global::<MaschineUi>()
            .on_harmonic_pitch(|pitch, mask, mapped| {
                let pitch = pitch.clamp(0, 127) as u8;
                i32::from(if mapped {
                    scale::nearest(pitch, mask as u16)
                } else {
                    pitch
                })
            });
        let weak = app.as_weak();
        let controls = registry.clone();
        app.global::<MaschineUi>().on_adopt_key(move |deck| {
            if !(1..=2).contains(&deck) {
                return;
            }
            let key = controls
                .get(&format!("[Channel{deck}]"), "key")
                .and_then(|c| Key::from_mixxx_number(c.get()));
            if let Some(key) = key {
                for (field, value) in [
                    ("harmony_root", f64::from(key.tonic)),
                    ("harmony_scale", f64::from(u8::from(key.minor))),
                    ("harmony_mode", f64::from(harmony::MANUAL)),
                ] {
                    if let Some(control) = controls.get("[Maschine3]", field) {
                        control.set(value);
                    }
                }
            } else if let Some(app) = weak.upgrade() {
                app.global::<MaschineUi>()
                    .set_status("Deck key unavailable. Choose the manual root and scale.".into());
            }
        });
        Self { registry }
    }

    fn value(&self, key: &str) -> f64 {
        self.registry
            .get("[Maschine3]", key)
            .map_or(0.0, |c| c.get())
    }

    pub fn mapping(&self) -> (u16, bool) {
        (
            self.value("harmony_mask") as u16,
            self.value("harmony_samples") > 0.0,
        )
    }

    pub fn update(&self, app: &AppWindow, project: Option<&Project>, group: usize, sound: usize) {
        let ui = app.global::<MaschineUi>();
        let mode = self.value("harmony_mode").round() as u8;
        let (mask, samples) = self.mapping();
        let key = Key::from_mixxx_number(self.value("harmony_key"));
        let source = self.value("harmony_source").round() as u8;
        let deck = self.value("harmony_deck").round() as usize;
        let label = if mode == harmony::CHROMATIC {
            "CHROMATIC".to_string()
        } else if mode == harmony::COMMON && mask != 0 {
            format!("COMMON · {} notes", mask.count_ones())
        } else {
            key.map_or("KEY UNAVAILABLE".to_string(), |k| {
                format!("{} · {}", k.name(), k.camelot())
            })
        };
        let source_text = match source {
            1 => "tags / imported metadata",
            2 => "OpenDJ key analysis",
            3 => "manual",
            4 => "shared scale notes of decks 1 and 2",
            _ => "choose a deck with a known key or set a manual key",
        };
        let reference = if deck > 0 {
            format!(
                "Deck {deck} · {source_text} · {}",
                if self.value("harmony_playing") > 0.0 {
                    "playing"
                } else {
                    "stopped"
                }
            )
        } else {
            source_text.to_string()
        };
        let instrument = project
            .and_then(|p| p.groups.get(group))
            .and_then(|g| g.sounds.get(sound))
            .map_or("", |s| s.instrument.as_str());
        let active = mask != 0 && harmony::maps_sound(instrument, samples);
        let detail = if mode == harmony::CHROMATIC {
            "Keyboard and Piano Roll use all 12 notes".into()
        } else if active && mode == harmony::COMMON {
            format!("{reference} · {}", harmony::note_names(mask))
        } else if active {
            let scale_type = if mode == harmony::MANUAL {
                self.value("harmony_scale") as usize
            } else {
                usize::from(key.is_some_and(|k| k.minor))
            };
            format!(
                "{reference} · {} · {}",
                scale::SCALES[scale_type.min(scale::SCALES.len() - 1)],
                harmony::note_names(mask)
            )
        } else if matches!(instrument, "Sampler" | "AudioModule") && !samples {
            format!("{reference} · enable MAP SAMPLES for a pitched sample")
        } else {
            format!("{reference} · selected Sound uses its original pitches")
        };
        ui.set_harmony_label(label.into());
        ui.set_harmony_detail(detail.into());
        ui.set_harmony_selected_active(active);
        let keys: [Option<Key>; 2] = std::array::from_fn(|i| {
            self.registry
                .get(&format!("[Channel{}]", i + 1), "key")
                .and_then(|c| Key::from_mixxx_number(c.get()))
        });
        let key_text = |key: Option<Key>| {
            key.map_or("?".to_string(), |k| {
                format!("{} · {}", k.name(), k.camelot())
            })
        };
        ui.set_harmony_key1(key_text(keys[0]).into());
        ui.set_harmony_key2(key_text(keys[1]).into());
        let (summary, notes) = match keys {
            [Some(a), Some(b)] => {
                let common = harmony::common_notes(a, b);
                let relation = if a == b {
                    "same key"
                } else if a.camelot_number() == b.camelot_number() {
                    "relative major / minor"
                } else if a.mixes(b, Mixing::Harmonic) {
                    "neighbouring wheel keys"
                } else {
                    "distant wheel keys"
                };
                let option = b
                    .shift_to_mix(a, 2, Mixing::Harmonic)
                    .filter(|&shift| shift != 0)
                    .map_or(String::new(), |shift| {
                        format!(
                            " · Deck 2 option {shift:+} st → {}",
                            b.transpose(shift).name()
                        )
                    });
                (
                    format!(
                        "{relation} · {} shared notes / 7{option}",
                        common.count_ones()
                    ),
                    harmony::note_names(common),
                )
            }
            _ => (
                "Load tracks with known keys on decks 1 and 2 to compare".into(),
                String::new(),
            ),
        };
        ui.set_harmony_transition(summary.into());
        ui.set_harmony_common_notes(notes.into());
    }
}

pub fn map_pitch(
    project: &Project,
    group: usize,
    sound: usize,
    pitch: i64,
    mapping: (u16, bool),
) -> i64 {
    let instrument = project
        .groups
        .get(group)
        .and_then(|g| g.sounds.get(sound))
        .map_or("", |s| s.instrument.as_str());
    harmony::note(pitch, instrument, mapping.1, mapping.0)
}
