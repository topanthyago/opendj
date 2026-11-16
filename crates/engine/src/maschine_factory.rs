//! Factory kits for a *new* Maschine document. Existing projects are never
//! passed here. The manifest is an explicit allowlist; no folders are scanned.
use deckard_doc::{Document, NewModule, Target, ZoneFrom, ZoneSpec};
use deckard_model::{Color, Note, PPQ};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

const GROUPS: usize = 8;
const SOUNDS: usize = 16;
const ROLES: [&str; 8] = [
    "kick", "snare", "hats", "clap", "toms", "percs", "graves", "fx",
];
// Groove rows Kick, Snare, Closed/Open Hat, Clap, Rim, Cowbell, Crash, Ride.
const RHYTHM_SOUND: [usize; 9] = [0, 2, 4, 5, 6, 10, 11, 14, 15];
const BPM: [u16; 8] = [130, 126, 130, 128, 130, 128, 130, 104];
// Original CC0 patterns from laptopan's brazilian-grooves/catalog.json and
// funk-expansion/catalog-extra.json. Names and tempo are source metadata, not
// claims of reference-audio analysis.
const PATTERNS: [[[&str; 9]; 4]; 8] = [
    // Funk carioca / Tamborzão — 130 BPM; catalog brasil-funk-expansion-01
    [
        [
            ".X..Xo.x...o....",
            "...........x....",
            "................",
            "................",
            "................",
            ".........x......",
            ".....o.xX....o.x",
            "................",
            "................",
        ],
        [
            ".xx.Xx.x...o....",
            "................",
            "................",
            "................",
            "................",
            ".o............X.",
            "x.......Xxxo....",
            "................",
            "................",
        ],
        [
            ".X..Xo.x...o....",
            "...........X....",
            "................",
            "................",
            "................",
            ".........x......",
            ".....o.xX....o.x",
            "................",
            "................",
        ],
        [
            ".xx.xo.x...x....",
            ".......o........",
            "................",
            "................",
            "................",
            ".o............X.",
            "x.......Xxxx....",
            "................",
            "................",
        ],
    ],
    // Funk carioca / Voltmix — 126 BPM; catalog brasil-funk-expansion-01
    [
        [
            "X.....x.X.....x.",
            "....x.......x...",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
        ],
        [
            "X..o..x.X..o..x.",
            "...oX.......x..o",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
        ],
        [
            "X.......x...o...",
            "....x.......x...",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
        ],
        [
            "X.....x.X...x.ox",
            "....x.....o.X.oo",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
        ],
    ],
    // Funk paulista / Mandelão — 130 BPM; catalog brasil-funk-expansion-01
    [
        [
            "x...X..Xx....o..",
            "..x..X..........",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
        ],
        [
            "x...x..oX...x...",
            "..x..o...o......",
            "................",
            "................",
            "................",
            "................",
            "..X...o..x......",
            "................",
            "................",
        ],
        [
            "o...X..Xx....o..",
            "..X..x..........",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
        ],
        [
            "x...x..ox...xx..",
            "..x..x...o......",
            "................",
            "................",
            "................",
            "................",
            "..X...o..x......",
            "................",
            "................",
        ],
    ],
    // Funk SC / Mega — 128 BPM; catalog brasil-performance-01
    [
        [
            "X...x...X...x...",
            "....x.......X...",
            "xo.oxo.oxo.oxo.o",
            "..x...x...x...x.",
            "....x.......x...",
            "...x..x....x....",
            "................",
            "................",
            "................",
        ],
        [
            "X...x...X...x...",
            "....o.......x...",
            "x...x...x...x...",
            "..x.......x.....",
            "............x...",
            "...x.......x....",
            "................",
            "................",
            "................",
        ],
        [
            "X...x...X..x..x.",
            "....x.......X...",
            "xo.oxoooxo.oxooo",
            "..x...x...x...x.",
            "....x.......x...",
            "...x..x..ox...x.",
            "................",
            "................",
            "................",
        ],
        [
            "X...x...X.......",
            "....x.......xox.",
            "xo.oxo.oxo.ox...",
            "..x...x...x.....",
            "....x...........",
            "...x..x....x...X",
            "................",
            "................",
            "................",
        ],
    ],
    // Funk MG / Montagem — 130 BPM; catalog brasil-performance-01
    [
        [
            "X.....x.....X...",
            "..x.......X....x",
            "..x...o...x...o.",
            "................",
            "......o.........",
            "...x.x.....x.x..",
            "................",
            "................",
            "................",
        ],
        [
            "X...........X...",
            "..x.......X.....",
            "..o.......o.....",
            "................",
            "................",
            "...x.......x....",
            "................",
            "................",
            "................",
        ],
        [
            "X.....x....oX...",
            "..x....o..X...ox",
            "..x...o...x...o.",
            "................",
            "......o.........",
            "...x.x.o..x..x..",
            "................",
            "................",
            "................",
        ],
        [
            "X.....x.....X...",
            "..x.......X.....",
            "..x...o...x.....",
            "................",
            "......o.........",
            "...x.x.....xx.oX",
            "................",
            "................",
            "................",
        ],
    ],
    // Funk GO / Eletro — 128 BPM; catalog brasil-performance-01
    [
        [
            "X...x...X...x..o",
            "....X.......x...",
            "x.o.x.o.x.o.x.ox",
            "..x...x...x...x.",
            "................",
            "...x......x....x",
            ".......x.......x",
            "................",
            "................",
        ],
        [
            "X...x...X...x...",
            "....X.......x...",
            "x...x...x...x...",
            "..x.......x.....",
            "................",
            "...x...........x",
            "................",
            "................",
            "................",
        ],
        [
            "X...x..xX.....x.",
            "....X......ox...",
            "x.o.x.o.x.o.x.ox",
            "..x...x...x...x.",
            "................",
            "...x.o....x..ox.",
            ".......x..o....x",
            "................",
            "................",
        ],
        [
            "X...x...X.......",
            "....X.......x.ox",
            "x.o.x.o.x.o.....",
            "..x...x...x.....",
            "................",
            "...x......x....X",
            ".......x........",
            "................",
            "................",
        ],
    ],
    // Rock Doido / Rock Doido — 130 BPM; catalog brasil-funk-expansion-01
    [
        [
            "X..x..x.X..x..x.",
            "................",
            "..x...o...x...o.",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
        ],
        [
            "X..x...xX.x...x.",
            "................",
            "..x...x...x..ox.",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
        ],
        [
            "X.....x.X.....x.",
            "................",
            "..x.......x...x.",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
        ],
        [
            "X..x..x.X.x.x.xo",
            "................",
            "..x...x...x.ooxx",
            "................",
            "................",
            "................",
            "................",
            "................",
            "................",
        ],
    ],
    // Samba / Roda — 104 BPM; catalog brasil-performance-01
    [
        [
            "o...X...o...X...",
            ".o.o.x.o.o.o.x.o",
            "xoxoxoxoxoxoxoxo",
            "......o.......o.",
            "................",
            "x..x..x.x.x..x..",
            "..x........x..x.",
            "................",
            "................",
        ],
        [
            "o...X...o...X...",
            ".....o.......o..",
            "x.o.x.o.x.o.x.o.",
            "................",
            "................",
            "x..x..x......x..",
            "..x.............",
            "................",
            "................",
        ],
        [
            "o...X...o...X...",
            ".o.o.x.o.oo..x.o",
            "xoxoxoxoxoxoxoxo",
            "......o.......o.",
            "................",
            "x..x.ox.x....xx.",
            "..x....o...x..x.",
            "................",
            "................",
        ],
        [
            "o...X...o...X...",
            ".o.o.x.o.o.o.ox.",
            "xoxoxoxoxoxox.o.",
            "......o.........",
            "................",
            "x..x..x.x...x.oX",
            "..x........x....",
            "................",
            "................",
        ],
    ],
];

struct Sound {
    path: PathBuf,
    name: String,
    role: usize,
    frames: usize,
    level: f64,
}
struct Kit {
    name: String,
    sounds: Vec<Sound>,
}

fn read_manifest(path: &Path) -> Result<Vec<Kit>, String> {
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    let json: Value = serde_json::from_reader(std::fs::File::open(path).map_err(fail)?)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if json.get("manifest_version").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported factory kit manifest".into());
    }
    let root = json
        .get("kits_root")
        .and_then(Value::as_str)
        .ok_or("missing kits_root")?;
    let root = Path::new(root);
    if !root.is_absolute() {
        return Err("kits_root must be absolute".into());
    }
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    let kits = json
        .get("kits")
        .and_then(Value::as_array)
        .ok_or("missing kits")?;
    if kits.len() != GROUPS {
        return Err("factory manifest must have eight groups".into());
    }
    let mut output = Vec::with_capacity(GROUPS);
    for (g, kit) in kits.iter().enumerate() {
        if kit.get("bank").and_then(Value::as_str)
            != Some(["A", "B", "C", "D", "E", "F", "G", "H"][g])
        {
            return Err(format!("wrong bank at group {}", g + 1));
        }
        let name = kit
            .get("style")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("missing kit style")?
            .to_owned();
        let slots = kit
            .get("slots")
            .and_then(Value::as_array)
            .ok_or("missing kit slots")?;
        if slots.len() != SOUNDS {
            return Err(format!("{name}: expected 16 sounds"));
        }
        let mut sounds: Vec<Option<Sound>> = (0..SOUNDS).map(|_| None).collect();
        for slot in slots {
            let i = slot
                .get("slot")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| (1..=SOUNDS).contains(n))
                .ok_or("invalid sound slot")?
                - 1;
            if sounds[i].is_some() {
                return Err(format!("{name}: duplicate slot {}", i + 1));
            }
            let role = slot
                .get("role")
                .and_then(Value::as_str)
                .ok_or("missing sound role")?;
            if role != ROLES[i / 2] {
                return Err(format!("{name}: unexpected role {role} in slot {}", i + 1));
            }
            let label = slot
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or("missing sound name")?
                .to_owned();
            let selected = Path::new(
                slot.get("selected_path")
                    .and_then(Value::as_str)
                    .ok_or("missing selected_path")?,
            );
            if !selected.is_absolute() {
                return Err("sample path must be absolute".into());
            }
            let selected = selected
                .canonicalize()
                .map_err(|e| format!("{}: {e}", selected.display()))?;
            if !selected.starts_with(&root) {
                return Err(format!(
                    "sample outside curated root: {}",
                    selected.display()
                ));
            }
            let expected = slot
                .get("sha256")
                .and_then(Value::as_str)
                .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or("invalid sample SHA-256")?;
            let mut file = std::fs::File::open(&selected)
                .map_err(|e| format!("{}: {e}", selected.display()))?;
            let mut hash = Sha256::new();
            let mut chunk = [0u8; 65_536];
            loop {
                let n = file
                    .read(&mut chunk)
                    .map_err(|e| format!("{}: {e}", selected.display()))?;
                if n == 0 {
                    break;
                }
                hash.update(&chunk[..n]);
            }
            let actual: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(format!("sample SHA-256 mismatch: {}", selected.display()));
            }
            let audio = deckard_engine::sample::Sample::load(&selected)
                .map_err(|e| format!("{}: {e}", selected.display()))?;
            if audio.rate == 0 || audio.frames.is_empty() {
                return Err(format!("empty sample: {}", selected.display()));
            }
            let peak = audio
                .frames
                .iter()
                .flat_map(|f| f.iter())
                .try_fold(0.0f32, |max, v| {
                    if v.is_finite() {
                        Some(max.max(v.abs()))
                    } else {
                        None
                    }
                })
                .ok_or("sample has non-finite audio")?;
            // No sample is boosted. Single-voice peak is limited to -3.1 dBFS;
            // simultaneous sounds still require master headroom.
            let gain = if peak > 0.7 { 0.7 / peak as f64 } else { 1.0 };
            let level = deckard_model::level_from_db(20.0 * gain.log10());
            sounds[i] = Some(Sound {
                path: selected,
                name: label,
                role: i / 2,
                frames: audio.frames.len(),
                level,
            });
        }
        output.push(Kit {
            name,
            sounds: sounds
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or("missing sound slot")?,
        });
    }
    Ok(output)
}

/// Installs curated samples into a fresh document, after the legacy Groove
/// import and before its first autosave. All files are checked before editing.
pub fn import(doc: &mut Document, manifest: &Path) -> Result<(), String> {
    let kits = read_manifest(manifest)?;
    if doc.project().groups.len() != GROUPS
        || doc
            .project()
            .groups
            .iter()
            .any(|g| g.sounds.len() != SOUNDS || g.patterns.is_empty())
    {
        return Err("factory import needs eight initialized groups of 16 sounds".into());
    }
    doc.transaction("Install factory kits", |doc| {
        if doc.project().scenes.is_empty() {
            return Err("factory import needs the initial scene".into());
        }
        // Scene 0 keeps Group A's selected legacy pattern. Other groups must
        // remain silent until the performer selects or layers them.
        for g in 1..GROUPS {
            doc.assign_pattern(0, g, None)?;
        }
        // Preserve every imported legacy note, including velocity and timing; only
        // its sound index changes to the corresponding role in the new group A.
        let legacy_names: Vec<String> = doc.project().groups[0]
            .sounds
            .iter()
            .take(8)
            .map(|s| s.name.to_lowercase())
            .collect();
        let mapping: Vec<usize> = legacy_names
            .iter()
            .enumerate()
            .map(|(row, name)| legacy_role(name, row))
            .collect();
        let legacy_patterns: Vec<Vec<Note>> = doc.project().groups[0]
            .patterns
            .iter()
            .map(|p| p.notes.clone())
            .collect();
        for (p, old) in legacy_patterns.iter().enumerate() {
            let mut mapped = old.clone();
            for note in &mut mapped {
                if let Some(&target) = mapping.get(note.sound) {
                    note.sound = target;
                }
            }
            if *old != mapped {
                doc.edit_notes("Remap legacy Groove", 0, p, old, &mapped)?;
            }
        }
        for (g, kit) in kits.iter().enumerate() {
            doc.rename(Target::Group(g), &kit.name)?;
            doc.set_color(&[Target::Group(g)], Color::Palette((g * 2) as u8))?;
            for (s, sound) in kit.sounds.iter().enumerate() {
                if doc.project().groups[g].sounds[s].sampler.is_none() {
                    doc.add_slot(g, s, 0, &NewModule::Sampler)?;
                }
                let last = sound.frames.saturating_sub(1) as f64;
                doc.set_zones(
                    g,
                    s,
                    &[ZoneSpec {
                        from: ZoneFrom::Template,
                        file: Some(sound.path.clone()),
                        set: vec![
                            ("play_range/low", 0.0),
                            ("play_range/high", last),
                            ("loop_range/low", 0.0),
                            ("loop_range/high", last),
                        ],
                    }],
                    "Install factory sample",
                )?;
                doc.rename(Target::Sound(g, s), &sound.name)?;
                doc.set_color(
                    &[Target::Sound(g, s)],
                    Color::Palette((sound.role * 2 + 1) as u8),
                )?;
                doc.set_level(Target::Sound(g, s), sound.level)?;
                doc.set_mute(Target::Sound(g, s), false)?;
                doc.set_pan(Target::Sound(g, s), 0.0)?;
            }
            let first_free = doc.project().groups[g]
                .patterns
                .iter()
                .map(|p| p.slot)
                .max()
                .unwrap_or(0)
                + 1;
            for variant in 0..4 {
                let label = format!(
                    "{} · {} · {} BPM",
                    ['A', 'B', 'C', 'D'][variant],
                    kit.name,
                    BPM[g]
                );
                let p = if g != 0 && variant == 0 {
                    if !doc.project().groups[g].patterns[0].notes.is_empty() {
                        return Err("factory pattern is not empty".into());
                    }
                    doc.rename(Target::Pattern(g, 0), &label)?;
                    0
                } else {
                    let slot = if g == 0 {
                        first_free + variant
                    } else {
                        first_free + variant - 1
                    };
                    doc.add_pattern_in_slot(g, 0, slot, &label)?
                };
                doc.set_pattern_length(g, p, PPQ * 4)?;
                let mut notes = Vec::new();
                for (row, rhythm) in PATTERNS[g][variant].iter().enumerate() {
                    for (step, byte) in rhythm.bytes().enumerate() {
                        let velocity = match byte {
                            b'X' => 127,
                            b'x' => 104,
                            b'o' => 80,
                            _ => continue,
                        };
                        notes.push(Note {
                            sound: RHYTHM_SOUND[row],
                            position: step as i64 * PPQ / 4,
                            length: PPQ / 4,
                            pitch: 60,
                            velocity,
                            probability: 100,
                            expr: None,
                        });
                    }
                }
                doc.edit_notes("Install factory rhythm", g, p, &[], &notes)?;
                let scene = doc.add_scene()?;
                doc.rename(Target::Scene(scene), &label)?;
                doc.set_color(&[Target::Scene(scene)], Color::Palette((g * 2) as u8))?;
                doc.assign_pattern(scene, g, Some(p))?;
            }
        }
        Ok(())
    })
}

fn legacy_role(name: &str, row: usize) -> usize {
    if name.contains("kick") || name.contains("bumbo") {
        0
    } else if name.contains("snare") || name.contains("caixa") {
        2
    } else if name.contains("open hat") || name.contains("open-hat") {
        5
    } else if name.contains("hat") || name.contains("chimbal") {
        4
    } else if name.contains("clap") || name.contains("palma") {
        6
    } else if name.contains("tom") {
        8
    } else if name.contains("rim") || name.contains("aro") {
        10
    } else if name.contains("cowbell") {
        11
    } else if name.contains("crash") || name.contains("cymbal") {
        14
    } else if name.contains("ride") {
        15
    } else {
        RHYTHM_SOUND[row.min(7)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    #[ignore = "requires the user's curated sample manifest"]
    fn imports_actual_curated_manifest() {
        let manifest = std::env::var_os("OPENDJ_MASCHINE_FACTORY_KITS")
            .map(PathBuf::from)
            .expect("set OPENDJ_MASCHINE_FACTORY_KITS");
        let mut doc = Document::new_project().unwrap();
        for g in 0..8 {
            if g > 0 {
                doc.add_group().unwrap();
            }
            doc.add_pattern(g, 0, "Initial").unwrap();
            doc.assign_pattern(0, g, Some(0)).unwrap();
        }
        import(&mut doc, &manifest).unwrap();
        assert_eq!(
            doc.project()
                .groups
                .iter()
                .flat_map(|g| &g.sounds)
                .filter(|s| s.sampler.as_ref().is_some_and(|x| !x.zones.is_empty()))
                .count(),
            128
        );
        assert_eq!(doc.project().scenes.len(), 33);
        assert_eq!(doc.project().scenes[0].patterns, vec![(0, 0)]);
        assert!(doc.project().scenes[1..]
            .iter()
            .all(|s| s.patterns.len() == 1));
    }

    #[test]
    fn imports_eight_complete_kits_and_preserves_legacy_patterns() {
        let root = std::env::temp_dir().join(format!(
            "opendj-factory-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let wav_path = root.join("fixture.wav");
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&40u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&44_100u32.to_le_bytes());
        wav.extend_from_slice(&88_200u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&i16::MAX.to_le_bytes());
        wav.extend_from_slice(&0i16.to_le_bytes());
        std::fs::write(&wav_path, &wav).unwrap();
        let digest: String = Sha256::digest(&wav)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let kits: Vec<_> = (0..8).map(|g| {
            let slots: Vec<_> = (0..16).map(|s| json!({"slot": s+1, "role": ROLES[s/2], "name": format!("sample {s}"), "selected_path": wav_path, "sha256": digest})).collect();
            json!({"bank": ((b'A'+g as u8) as char).to_string(), "style": format!("style {g}"), "slots": slots})
        }).collect();
        let manifest = root.join("manifest.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec(&json!({"manifest_version": 1, "kits_root": root, "kits": kits}))
                .unwrap(),
        )
        .unwrap();
        let mut doc = Document::new_project().unwrap();
        for group in 0..8 {
            if group > 0 {
                doc.add_group().unwrap();
            }
            doc.add_pattern(group, 0, "Legacy").unwrap();
        }
        doc.rename(Target::Sound(0, 1), "Snare").unwrap();
        for p in 1..4 {
            doc.add_pattern_in_slot(0, 0, p, "Legacy").unwrap();
        }
        for g in 0..8 {
            doc.assign_pattern(0, g, Some(if g == 0 { 3 } else { 0 }))
                .unwrap();
        }
        let note = Note {
            sound: 1,
            position: 0,
            length: PPQ / 4,
            pitch: 60,
            velocity: 105,
            probability: 100,
            expr: None,
        };
        doc.edit_notes("legacy", 0, 0, &[], &[note]).unwrap();
        import(&mut doc, &manifest).unwrap();
        assert_eq!(doc.project().groups.len(), 8);
        for (g, group) in doc.project().groups.iter().enumerate() {
            assert_eq!(group.sounds.len(), 16);
            assert_eq!(group.patterns.len(), if g == 0 { 8 } else { 4 });
            for sound in &group.sounds {
                assert!(sound.sampler.as_ref().is_some_and(|s| s
                    .zones
                    .first()
                    .is_some_and(|z| z.sample.path == wav_path.to_str().unwrap())));
            }
        }
        let preserved = &doc.project().groups[0].patterns[0].notes[0];
        assert_eq!(preserved.sound, 2);
        assert_eq!(preserved.velocity, note.velocity);
        assert_eq!(preserved.position, note.position);
        assert_eq!(doc.project().scenes.len(), 33);
        assert_eq!(doc.project().scenes[0].patterns, vec![(0, 3)]);
        for (g, group) in doc.project().groups.iter().enumerate() {
            for variant in 0..4 {
                let scene = &doc.project().scenes[1 + g * 4 + variant];
                let expected_pattern = if g == 0 { 4 + variant } else { variant };
                assert_eq!(scene.patterns, vec![(g, expected_pattern)]);
                assert_eq!(group.patterns[expected_pattern].slot, expected_pattern);
            }
        }
        // A modified file cannot be silently imported under its old identity.
        let before = doc.project().clone();
        std::fs::write(&wav_path, b"tampered").unwrap();
        assert!(read_manifest(&manifest)
            .err()
            .unwrap()
            .contains("SHA-256 mismatch"));
        assert!(import(&mut doc, &manifest).is_err());
        assert_eq!(doc.project(), &before);
        std::fs::remove_dir_all(root).unwrap();
    }
}
