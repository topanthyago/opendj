//! Run against the same DECKARD SDK dependencies as OpenDJ. No host audio or GUI.
use base64::Engine as _;
use opendj_engine::maschine::{
    level_from_db, sample_edit, Document, FileItem, NewModule, Note, Sample, SoundInfo, Target,
    Value as Parameter, ZoneFrom, ZoneSpec, PPQ,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

fn install(recipe: &Path, root: &Path, output: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    let library: Value = serde_json::from_slice(&std::fs::read(recipe)?)?;
    let banks = library["banks"].as_array().ok_or("missing banks")?;
    if library["format"] != 1 || banks.len() != 8 {
        return Err("expected library format 1 with eight banks".into());
    }
    let root = root.canonicalize()?;
    // Validate every source before producing any output. Keep a private analysis report.
    let mut verified = Vec::new();
    let mut created = 0;
    let mut preserved = 0;
    for bank in banks {
        let sounds = bank["sounds"].as_array().ok_or("missing sounds")?;
        if sounds.len() != 16 {
            return Err("each bank needs sixteen sounds".into());
        }
        let mut sources = Vec::new();
        for sound in sounds {
            let relative = Path::new(sound["path"].as_str().ok_or("missing path")?);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
            {
                return Err("sample path must stay inside collection".into());
            }
            let path = root.join(relative).canonicalize()?;
            if !path.starts_with(&root) {
                return Err("sample outside collection".into());
            }
            let mut hash = Sha256::new();
            let mut file = std::fs::File::open(&path)?;
            let mut block = [0u8; 65536];
            loop {
                let n = file.read(&mut block)?;
                if n == 0 {
                    break;
                }
                hash.update(&block[..n]);
            }
            let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
            if sound["sha256"].as_str() != Some(digest.as_str()) {
                return Err(format!("changed sample {}", path.display()).into());
            }
            let audio = Sample::load(&path)?;
            let mut peak = 0.0f32;
            let mut energy = 0.0f64;
            for frame in &audio.frames {
                for &v in frame {
                    if !v.is_finite() {
                        return Err("non-finite sample".into());
                    }
                    peak = peak.max(v.abs());
                    energy += f64::from(v).powi(2);
                }
            }
            if audio.frames.is_empty() || audio.rate == 0 || peak == 0.0 {
                return Err("empty or silent sample".into());
            }
            let duration = audio.frames.len() as f64 / audio.rate as f64;
            if (duration - sound["duration_s"].as_f64().ok_or("missing duration")?).abs() > 0.002 {
                return Err("sample duration changed".into());
            }
            let (selected, selected_audio, derived_hash) = if sound["kind"] == "loop" {
                let beats = sound["beats"].as_f64().ok_or("missing beats")?;
                let bpm = sound["target_bpm"].as_f64().ok_or("missing target BPM")?;
                if !beats.is_finite() || !bpm.is_finite() || beats <= 0.0 || bpm <= 0.0 {
                    return Err("invalid loop timing".into());
                }
                let target = (beats * 60.0 / bpm * audio.rate as f64).round() as usize;
                let mut fitted = sample_edit::stretch(
                    &audio,
                    (0, audio.frames.len()),
                    target as f64 / audio.frames.len() as f64,
                    0.0,
                );
                fitted.frames.resize(target, [0.0; 2]);
                let folder = root.join(".opendj-derived/brazilian-v1");
                std::fs::create_dir_all(&folder)?;
                let temporary = folder.join(format!(
                    ".{}-{}-{}.wav",
                    std::process::id(),
                    digest,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos()
                ));
                sample_edit::write_wav(&temporary, &fitted)?;
                let bytes = std::fs::read(&temporary)?;
                let hash: String = Sha256::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                let destination = folder.join(format!("{hash}.wav"));
                let link = std::fs::hard_link(&temporary, &destination);
                std::fs::remove_file(&temporary)?;
                if let Err(e) = link {
                    if e.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(e.into());
                    }
                }
                if std::fs::read(&destination)? != bytes {
                    return Err("derived sample hash collision or corruption".into());
                }
                let reopened = Sample::load(&destination)?;
                assert_eq!(reopened.frames.len(), target);
                (destination, reopened, Some(hash))
            } else {
                (path, audio, None)
            };
            let selected_peak = selected_audio
                .frames
                .iter()
                .flat_map(|f| f.iter())
                .fold(0.0f32, |p, v| p.max(v.abs()));
            verified.push(json!({"bank": bank["bank"], "name": sound["name"], "kind": sound["kind"], "duration_s": duration, "peak":peak, "rms":(energy/(duration*selected_audio.rate as f64*2.0)).sqrt(), "sha256":digest,"selected_path":selected,"derived_sha256":derived_hash,"selected_duration_s":selected_audio.frames.len() as f64/selected_audio.rate as f64,"target_bpm":bank["target_bpm"]}));
            sources.push((selected, selected_audio.frames.len(), selected_peak));
        }
        // The complete source document has the usual eight groups and sixteen Sounds.
        let mut doc = Document::new_project()?;
        for _ in 1..8 {
            doc.add_group()?;
        }
        doc.rename(
            Target::Group(0),
            bank["name"].as_str().ok_or("missing bank name")?,
        )?;
        for (slot, (sound, (path, frames, peak))) in sounds.iter().zip(&sources).enumerate() {
            doc.add_slot(0, slot, 0, &NewModule::Sampler)?;
            let high = frames.saturating_sub(1) as f64;
            doc.set_zones(
                0,
                slot,
                &[ZoneSpec {
                    from: ZoneFrom::Template,
                    file: Some(path.clone()),
                    set: vec![
                        ("play_range/low", 0.0),
                        ("play_range/high", high),
                        ("loop_range/low", 0.0),
                        ("loop_range/high", high),
                    ],
                }],
                "Brazilian source",
            )?;
            doc.rename(
                Target::Sound(0, slot),
                sound["name"].as_str().ok_or("missing name")?,
            )?;
            doc.set(
                "Retrigger",
                Target::Sampler(0, slot),
                "envelope_type",
                Parameter::Int(0),
            )?;
            doc.set(
                "Retrigger",
                Target::Sampler(0, slot),
                "polyphony",
                Parameter::Int(1),
            )?;
            // Leave at least 9 dB of per-Sound headroom, never boost quiet sources.
            let gain = (0.35 / f64::from(*peak)).min(1.0);
            doc.set_level(Target::Sound(0, slot), level_from_db(20.0 * gain.log10()))?;
        }
        let loops = sounds[0]["kind"] == "loop";
        if !loops {
            doc.set_level(Target::Group(0), level_from_db(-9.0))?;
        }
        let count = if loops { 16 } else { 4 };
        for pattern in 0..count {
            let sound = &sounds[pattern];
            let title = if loops {
                sound["name"].as_str().ok_or("name")?.to_string()
            } else {
                format!("Original demo {}", pattern + 1)
            };
            let p = doc.add_pattern_in_slot(0, 0, pattern, &title)?;
            let beats = if loops {
                sound["beats"].as_i64().ok_or("missing loop beats")?
            } else {
                8
            };
            doc.set_pattern_length(0, p, PPQ * beats)?;
            let mut notes = Vec::new();
            if loops {
                notes.push(Note {
                    sound: pattern,
                    position: 0,
                    length: PPQ * beats,
                    pitch: 60,
                    velocity: 110,
                    probability: 100,
                    expr: None,
                });
            } else {
                // Sparse two-bar demonstrations. Each variant uses different source slots.
                for (row, steps) in [
                    (0, vec![0, 6, 16, 23]),
                    (2, vec![4, 12, 20, 28]),
                    (4, vec![2, 10, 18, 26]),
                    (8, vec![3, 11, 19, 27]),
                    (14, vec![1, 7, 13, 17, 24, 30]),
                    (12, vec![0]),
                ] {
                    for step in steps {
                        notes.push(Note {
                            sound: row + (pattern % 2),
                            position: (step + pattern / 2) as i64 * PPQ / 4,
                            length: PPQ / 4,
                            pitch: 60,
                            velocity: if step % 4 == 0 { 108 } else { 90 },
                            probability: 100,
                            expr: None,
                        });
                    }
                }
            }
            doc.edit_notes("Brazilian source pattern", 0, p, &[], &notes)?;
            let scene = if pattern == 0 { 0 } else { doc.add_scene()? };
            doc.assign_pattern(scene, 0, Some(p))?;
            doc.rename(Target::Scene(scene), &title)?;
        }
        assert_eq!(doc.project().groups.len(), 8);
        assert!(doc.project().groups.iter().all(|g| g.sounds.len() == 16));
        let group = doc.file_bytes(FileItem::Group(0), &SoundInfo::default())?;
        // Match OpenDJ's Native preset exporter: strip unrelated groups before saving.
        for other in (1..8).rev() {
            doc.delete_group(other)?;
        }
        let project = doc.to_bytes()?;
        let mut probe = Document::new_project()?;
        probe.load_group_file_with(0, &group, false, false)?;
        assert!(probe.project().groups[0].sounds.iter().all(|s| s
            .sampler
            .as_ref()
            .is_some_and(|sampler| sampler.params.get("polyphony") == Some(&1.0)
                && sampler.envelope_type == 0)));
        let roundtrip = Document::from_bytes(project.clone(), None)?;
        assert_eq!(roundtrip.project().groups[0].patterns.len(), count);
        if loops {
            for (i, pattern) in roundtrip.project().groups[0].patterns.iter().enumerate() {
                assert_eq!(pattern.notes.len(), 1);
                assert_eq!(pattern.notes[0].position, 0);
                assert_eq!(pattern.notes[0].sound, i);
            }
        }
        std::fs::create_dir_all(output)?;
        for variant in 0..4 {
            let focus = if loops { variant * 4 } else { variant };
            let tempo = if loops {
                format!(" · {} BPM fit", bank["target_bpm"])
            } else {
                String::new()
            };
            let name = format!(
                "BR {} · {} · {:02}{}",
                bank["bank"].as_str().ok_or("bank")?,
                bank["name"].as_str().ok_or("name")?,
                variant + 1,
                tempo
            );
            let body = json!({"format":1,"instrument":"groove","instrument_name":"Groove Box","name":name,"tags":["Drums","Brazilian",if loops {"Source loops"} else {"One shots"}],"controls":{},"native_groove":{"group":base64::engine::general_purpose::STANDARD.encode(&group),"project":base64::engine::general_purpose::STANDARD.encode(&project),"origin":null,"scene":focus},"source_library":{"recipe_format":1,"bank":bank,"selected_pattern":focus,"note":"Loops use private pitch-preserving WSOLA derivatives fitted to the bank target BPM. Source beat counts remain inferred where recorded. Native presets preserve the mix tempo; set deck to target BPM."}});
            let path = output.join(format!("{name}.json"));
            if path.exists() {
                preserved += 1;
                continue;
            }
            let temporary = path.with_extension(format!("{}.pending", std::process::id()));
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)?;
                file.write_all(&serde_json::to_vec_pretty(&body)?)?;
                file.sync_all()?;
                match std::fs::hard_link(&temporary, &path) {
                    Ok(()) => created += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => preserved += 1,
                    Err(e) => return Err(e.into()),
                }
                Ok(())
            })();
            // Remove only the file this invocation created.
            if result.is_ok() {
                std::fs::remove_file(&temporary)?;
            }
            result?;
        }
    }
    Ok(
        json!({"banks":banks.len(),"presets":banks.len()*4,"created":created,"existing_preserved":preserved,"sources":verified,"output":output}),
    )
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if args.len() != 3 {
        return Err(
            "usage: install-brazilian-presets RECIPE_JSON SAMPLE_COLLECTION PRESET_OUTPUT_DIR"
                .into(),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&install(&args[0], &args[1], &args[2])?)?
    );
    Ok(())
}
