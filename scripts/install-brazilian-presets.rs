//! Run against the same DECKARD SDK dependencies as OpenDJ. No host audio or GUI.
use base64::Engine as _;
use opendj_engine::maschine::{
    instrument_defaults, level_from_db, sample_edit, Document, FileItem, NewModule, Note, Sample, SoundInfo, Target,
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
    let mut total_notes = 0;
    let mut bank_reports = Vec::new();
    for bank in banks {
        let previous_notes = total_notes;
        let previous_created = created;
        let previous_preserved = preserved;
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
            total_notes += notes.len();
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
        bank_reports.push(json!({"name":bank["name"],"tempo":bank["target_bpm"],"groups":1,"notes":total_notes-previous_notes,"presets":4,"created":created-previous_created,"existing_preserved":preserved-previous_preserved,"warnings":if loops {vec!["Source phrase beat counts include duration-based hypotheses; private WSOLA derivatives fit the displayed playback BPM"]} else {Vec::<&str>::new()}}));
    }
    Ok(
        json!({"format":1,"imported_projects":banks.len(),"notes":total_notes,"projects":bank_reports,"errors":[],"banks":banks.len(),"presets":banks.len()*4,"created":created,"existing_preserved":preserved,"sources":verified,"output":output}),
    )
}
fn publish_new(path: &Path, bytes: &[u8]) -> Result<bool, Box<dyn std::error::Error>> {
    if path.exists() {
        return Ok(false);
    }
    let parent = path.parent().ok_or("output needs a parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".content-{}-{}.pending",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| -> Result<bool, Box<dyn std::error::Error>> {
        file.write_all(bytes)?;
        file.sync_all()?;
        match std::fs::hard_link(&temporary, path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    })();
    std::fs::remove_file(temporary)?;
    result
}

fn safe_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_control() || r#"/\:*?"<>|"#.contains(c) {
                '-'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim_matches(['.', ' ']);
    if clean.is_empty() {
        "Imported content".into()
    } else {
        clean.chars().take(150).collect()
    }
}

fn content_sample(
    path: &Path,
    output: &Path,
) -> Result<(PathBuf, Sample), Box<dyn std::error::Error>> {
    match Sample::load(path) {
        Ok(audio) => Ok((path.to_path_buf(), audio)),
        Err(error) if error.starts_with("unsupported sample format") => {
            let folder = output.join("Converted audio");
            std::fs::create_dir_all(&folder)?;
            let temporary = folder.join(format!(
                ".decode-{}-{}.wav",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            ));
            let status = std::process::Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-n", "-i"])
                .arg(path)
                .args(["-map", "0:a:0", "-c:a", "pcm_s24le"])
                .arg(&temporary)
                .output()?;
            if !status.status.success() {
                let _ = std::fs::remove_file(&temporary);
                return Err(format!(
                    "source audio decode failed: {}",
                    String::from_utf8_lossy(&status.stderr).trim()
                )
                .into());
            }
            let bytes = std::fs::read(&temporary)?;
            let hash: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let destination = folder.join(format!("{hash}.wav"));
            let result = publish_new(&destination, &bytes);
            std::fs::remove_file(&temporary)?;
            result?;
            if std::fs::read(&destination)? != bytes {
                return Err("converted audio hash collision or modified file".into());
            }
            let audio = Sample::load(&destination)?;
            Ok((destination, audio))
        }
        Err(error) => Err(error.into()),
    }
}

fn content_project(
    project: &Value,
    parent: &Path,
    output: &Path,
) -> Result<Value, Box<dyn std::error::Error>> {
    let name = project["name"].as_str().ok_or("project name missing")?;
    let ppq = project["ppq"]
        .as_i64()
        .filter(|v| *v > 0 && *v <= 1_000_000)
        .ok_or("invalid source PPQ")?;
    let tempo = project["tempo"]
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 10.0 && *v <= 1000.0)
        .ok_or("invalid source tempo")?;
    let sounds = project["sounds"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 128)
        .ok_or("content needs 1..128 sounds")?;
    let patterns = project["patterns"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or("content needs patterns")?;
    let velocity_max = project["velocity_max"].as_f64().unwrap_or(128.0);
    if !velocity_max.is_finite() || velocity_max <= 0.0 {
        return Err("invalid source velocity_max".into());
    }
    let groups = sounds.len().div_ceil(16);
    let mut doc = Document::new_project()?;
    for _ in 1..groups {
        doc.add_group()?;
    }
    doc.set_tempo(tempo)?;
    let mut warnings = Vec::new();
    if let Some(values) = project["warnings"].as_array() {
        warnings.extend(values.iter().map(|v| {
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())
        }));
    }
    if project["unresolved_channels"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        warnings.push("Unresolved sample/plugin channels remain opaque in source_content and the original project metadata".into());
    }
    if sounds.iter().any(|sound| sound["native_instrument"].is_null()) {
        warnings.push("Native conversion retains resolved sample notes. Source plugin, mixer, sampler-envelope and arrangement behavior require review".into());
    }
    let mut loaded = vec![false; sounds.len()];
    for (index, sound) in sounds.iter().enumerate() {
        let group = index / 16;
        let slot = index % 16;
        if let Some(class) = sound["native_instrument"].as_str() {
            if !matches!(class, "BassSynth" | "PolySynth") {
                return Err(format!("unsupported native melodic instrument {class}").into());
            }
            let root = sound["root_note"].as_i64().unwrap_or(60);
            if !(0..=127).contains(&root) {
                return Err("invalid native instrument root note".into());
            }
            let params = instrument_defaults(class).ok_or("native instrument defaults unavailable")?
                .into_iter().map(|(field, value)| (field.into(), value)).collect();
            doc.add_slot(group, slot, 0, &NewModule::Instrument { class: class.into(), params })?;
            doc.rename(Target::Sound(group, slot), sound["name"].as_str().unwrap_or(class))?;
            doc.set("Native instrument base note", Target::Sound(group, slot), "base_key", Parameter::Int(root))?;
            doc.set_mute(Target::Sound(group, slot), sound["enabled"].as_bool() == Some(false))?;
            loaded[index] = true;
            continue;
        }
        let Some(relative) = sound["path"].as_str().filter(|s| !s.is_empty()) else {
            warnings.push(format!(
                "Sound {index} has no resolved sample and remains silent"
            ));
            continue;
        };
        let supplied = Path::new(relative);
        let path = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            parent.join(supplied)
        }
        .canonicalize()?;
        let expected = sound["sha256"].as_str().ok_or("sample SHA-256 missing")?;
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
        if !digest.eq_ignore_ascii_case(expected) {
            return Err(format!("sample SHA-256 mismatch: {}", path.display()).into());
        }
        let (selected_path, audio) = content_sample(&path, output)?;
        if selected_path != path {
            warnings.push(format!(
                "Sound {index} decoded to a private 24-bit WAV; original retained at {}",
                path.display()
            ));
        }
        if audio.rate == 0
            || audio.frames.is_empty()
            || audio
                .frames
                .iter()
                .flat_map(|f| f.iter())
                .any(|v| !v.is_finite())
        {
            return Err("invalid source audio".into());
        }
        let root = sound["root_note"].as_i64().unwrap_or(60);
        if !(0..=127).contains(&root) {
            return Err("invalid sample root note".into());
        }
        let cents = sound["fine_tune"].as_f64().unwrap_or(0.0)
            + sound["channel_pitch_shift"].as_f64().unwrap_or(0.0);
        if !cents.is_finite() || cents.abs() > 9600.0 {
            return Err("invalid source sample tuning".into());
        }
        if sound["stretching_pitch_cents"]
            .as_f64()
            .is_some_and(|v| v != 0.0)
        {
            warnings.push(format!(
                "Sound {index} stretching pitch remains opaque in source_content"
            ));
        }
        doc.add_slot(group, slot, 0, &NewModule::Sampler)?;
        let last = i64::try_from(audio.frames.len().saturating_sub(1))?;
        let first = sound
            .get("sample_start")
            .filter(|v| !v.is_null())
            .map(|v| v.as_i64().ok_or("sample_start must be integer frames"))
            .transpose()?
            .unwrap_or(0);
        let mut end = sound
            .get("sample_end")
            .filter(|v| !v.is_null())
            .map(|v| v.as_i64().ok_or("sample_end must be integer frames"))
            .transpose()?
            .unwrap_or(last);
        if sound["sample_end_inclusive"].as_bool() == Some(false) {
            end = end
                .checked_sub(1)
                .ok_or("exclusive sample_end is invalid")?;
        }
        if first < 0 || end < first || end > last {
            return Err(format!("Sound {index} sample trim lies outside decoded audio").into());
        }
        doc.set_zones(
            group,
            slot,
            &[ZoneSpec {
                from: ZoneFrom::Template,
                file: Some(selected_path),
                set: vec![
                    ("play_range/low", first as f64),
                    ("play_range/high", end as f64),
                    ("loop_range/low", first as f64),
                    ("loop_range/high", end as f64),
                    ("root_key", root as f64),
                    ("tune", cents / 100.0),
                ],
            }],
            "Import source sample",
        )?;
        doc.rename(
            Target::Sound(group, slot),
            sound["name"].as_str().unwrap_or("Source sample"),
        )?;
        // Keep chord voices available. Source envelopes and synths remain documented limitations.
        doc.set(
            "Import sample",
            Target::Sampler(group, slot),
            "envelope_type",
            Parameter::Int(0),
        )?;
        doc.set(
            "Import sample",
            Target::Sampler(group, slot),
            "polyphony",
            Parameter::Int(4),
        )?;
        doc.set_mute(
            Target::Sound(group, slot),
            sound["enabled"].as_bool() == Some(false),
        )?;
        loaded[index] = true;
    }
    let ticks = |value: i64| -> Result<i64, Box<dyn std::error::Error>> {
        let scaled = i128::from(value) * i128::from(PPQ);
        Ok(i64::try_from(
            (scaled + i128::from(ppq) / 2) / i128::from(ppq),
        )?)
    };
    let mut note_count = 0;
    let mut skipped = 0;
    let mut non_exact = 0;
    let mut zero_lengths = 0;
    for group in 0..groups {
        doc.rename(
            Target::Group(group),
            &format!("{name} · Part {}", group + 1),
        )?;
        // Fixed output headroom leaves the source sample dynamics unchanged.
        doc.set_level(Target::Group(group), level_from_db(-12.0))?;
    }
    for (p, pattern) in patterns.iter().enumerate() {
        let scene = if p == 0 { 0 } else { doc.add_scene()? };
        let pattern_name = pattern["name"].as_str().unwrap_or("Source pattern");
        doc.rename(Target::Scene(scene), pattern_name)?;
        let length = ticks(
            pattern["length"]
                .as_i64()
                .filter(|v| *v > 0)
                .ok_or("invalid pattern length")?,
        )?;
        let mut by_group = vec![Vec::new(); groups];
        for note in pattern["notes"].as_array().ok_or("pattern notes missing")? {
            let index = note["sound"]
                .as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .filter(|v| *v < sounds.len())
                .ok_or("note references an unknown Sound index")?;
            if !loaded[index] {
                skipped += 1;
                continue;
            }
            let key = note["key"].as_i64().ok_or("note key missing")?;
            if !(0..=127).contains(&key) {
                skipped += 1;
                warnings.push(format!(
                    "Note key {key} is outside MIDI range and remains in source_content"
                ));
                continue;
            }
            let position = note["position"]
                .as_i64()
                .filter(|v| *v >= 0)
                .ok_or("invalid note position")?;
            let duration = note["length"]
                .as_i64()
                .filter(|v| *v >= 0)
                .ok_or("invalid note length")?;
            if position as i128 * PPQ as i128 % ppq as i128 != 0
                || duration as i128 * PPQ as i128 % ppq as i128 != 0
            {
                non_exact += 1;
            }
            if note["slide"].as_bool() == Some(true)
                || note["fine_pitch"].as_i64().is_some_and(|v| v != 120)
                || note["pan"].as_i64().is_some_and(|v| v != 64)
            {
                warnings.push(
                    "Per-note slide, fine pitch or pan remains opaque in source_content".into(),
                );
            }
            let velocity = note["velocity"]
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0 && *v <= velocity_max)
                .ok_or("invalid note velocity")?;
            let note_length = ticks(duration)?;
            if note_length == 0 {
                zero_lengths += 1;
            }
            by_group[index / 16].push(Note {
                sound: index % 16,
                position: ticks(position)?,
                length: note_length.max(1),
                pitch: key,
                velocity: (velocity * 127.0 / velocity_max).round() as i64,
                probability: 100,
                expr: None,
            });
            note_count += 1;
        }
        for (group, notes) in by_group.into_iter().enumerate() {
            let index = doc.add_pattern_in_slot(group, 0, p, pattern_name)?;
            doc.set_pattern_length(group, index, length.max(1))?;
            doc.edit_notes("Import original source notes", group, index, &[], &notes)?;
            doc.assign_pattern(scene, group, Some(index))?;
        }
    }
    if note_count == 0 {
        return Err("no resolved source notes to import".into());
    }
    if non_exact > 0 {
        warnings.push(format!(
            "{non_exact} notes required nearest-tick PPQ conversion"
        ));
    }
    if zero_lengths > 0 {
        warnings.push(format!("{zero_lengths} zero-length source triggers use the native minimum one tick; original lengths remain in source_content"));
    }
    warnings.sort();
    warnings.dedup();
    let source_bytes = serde_json::to_vec(project)?;
    let id: String = Sha256::digest(&source_bytes)
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect();
    let kind = if sounds.iter().any(|sound| sound["native_instrument"].is_string()) { "Native" } else { "Imported" };
    let basename = safe_name(&format!("{kind} · {name} · {id}"));
    let mut created = 0;
    let mut preserved = 0;
    let bytes = doc.to_bytes()?;
    let converted = output
        .join("Converted projects")
        .join(format!("{basename}.mxprj"));
    if publish_new(&converted, &bytes)? {
        created += 1;
    } else {
        preserved += 1;
    }
    let probe = Document::from_bytes(bytes.clone(), None)?;
    assert_eq!(
        probe
            .project()
            .groups
            .iter()
            .flat_map(|g| g.patterns.iter())
            .map(|p| p.notes.len())
            .sum::<usize>(),
        note_count
    );
    for group in 0..groups {
        let group_file = doc.file_bytes(FileItem::Group(group), &SoundInfo::default())?;
        let mut isolated = Document::from_bytes(bytes.clone(), None)?;
        for other in (0..groups).rev() {
            if other != group {
                isolated.delete_group(other)?;
            }
        }
        let preset_name = format!("{basename} · Part {} · {tempo} BPM", group + 1);
        let mut tags: Vec<_> = project["tags"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        if !tags.contains(&kind) { tags.push(kind); }
        let body = json!({"format":1,"instrument":"groove","instrument_name":"Groove Box","name":preset_name,"tags":tags,"controls":{},"native_groove":{"group":base64::engine::general_purpose::STANDARD.encode(group_file),"project":base64::engine::general_purpose::STANDARD.encode(isolated.to_bytes()?),"origin":null,"scene":0},"source_content":project,"harmonic":project["harmonic"],"import_warnings":warnings,"converted_project":converted});
        if publish_new(
            &output.join(format!(
                "Imported-{id}-{:02}-{}.json",
                group + 1,
                safe_name(name)
            )),
            &serde_json::to_vec_pretty(&body)?,
        )? {
            created += 1;
        } else {
            preserved += 1;
        }
    }
    Ok(
        json!({"name":name,"tempo":tempo,"groups":groups,"notes":note_count,"skipped_notes":skipped,"presets":groups,"created":created,"existing_preserved":preserved,"converted_project":converted,"warnings":warnings}),
    )
}

fn install_content(recipe: &Path, output: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    let root: Value = serde_json::from_slice(&std::fs::read(recipe)?)?;
    if root["format"] != 1 {
        return Err("unsupported content recipe format".into());
    }
    if root["banks"].is_array() {
        let sample_root = if let Some(root) =
            std::env::var_os("OPENDJ_MASCHINE_SAMPLE_ROOT").filter(|v| !v.is_empty())
        {
            PathBuf::from(root)
        } else {
            let data = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
                Some(path) => PathBuf::from(path),
                None => PathBuf::from(
                    std::env::var_os("HOME")
                        .ok_or("HOME or XDG_DATA_HOME is required for the sample collection")?,
                )
                .join(".local/share"),
            };
            data.join("laptopan/opendj/sample-packs")
        };
        return install(recipe, &sample_root, output);
    }
    let projects = root["projects"]
        .as_array()
        .ok_or("content recipe needs projects")?;
    let parent = recipe
        .parent()
        .ok_or("recipe needs parent")?
        .canonicalize()?;
    let mut imported = Vec::new();
    let mut errors = Vec::new();
    for project in projects {
        match content_project(project, &parent, output) {
            Ok(value) => imported.push(value),
            Err(e) => errors.push(json!({"name":project["name"],"error":e.to_string()})),
        }
    }
    Ok(
        json!({"format":1,"imported_projects":imported.len(),"notes":imported.iter().filter_map(|p|p["notes"].as_u64()).sum::<u64>(),"presets":imported.iter().filter_map(|p|p["presets"].as_u64()).sum::<u64>(),"created":imported.iter().filter_map(|p|p["created"].as_u64()).sum::<u64>(),"existing_preserved":imported.iter().filter_map(|p|p["existing_preserved"].as_u64()).sum::<u64>(),"projects":imported,"errors":errors}),
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let report = if args.len() == 3 && args[0] == Path::new("--content") {
        install_content(&args[1], &args[2])?
    } else if args.len() == 3 {
        install(&args[0], &args[1], &args[2])?
    } else {
        return Err("usage: opendj-install-brazilian-presets RECIPE_JSON SAMPLE_COLLECTION PRESET_OUTPUT_DIR | --content CONTENT_RECIPE_JSON PRESET_OUTPUT_DIR".into());
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_melodic_recipes_keep_all_keys_notes_synths_and_metadata() {
        use opendj_engine::maschine::{Mode, OfflineEngine, SampleCache};
        let root = std::env::temp_dir().join(format!("opendj-native-harmonic-test-{}-{}",
            std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&root).unwrap();
        let recipe = root.join("harmonic-native-recipes.json");
        std::fs::write(&recipe, include_bytes!("../res/presets/harmonic-native-recipes.json")).unwrap();
        let output = root.join("presets");
        let report = install_content(&recipe, &output).unwrap();
        assert_eq!(report["imported_projects"], 24);
        assert_eq!(report["presets"], 24);
        assert_eq!(report["notes"], 4128);
        assert_eq!(report["created"], 48);
        assert_eq!(report["errors"], json!([]));
        let source: Value = serde_json::from_slice(include_bytes!("../res/presets/harmonic-native-recipes.json")).unwrap();
        for (index, project) in report["projects"].as_array().unwrap().iter().enumerate() {
            let path = project["converted_project"].as_str().unwrap();
            let doc = Document::from_bytes(std::fs::read(path).unwrap(), None).unwrap();
            let group = &doc.project().groups[0];
            assert_eq!(group.patterns.len(), 7);
            assert_eq!(group.sounds.len(), 16);
            assert_eq!(group.sounds[0].instrument, "BassSynth");
            assert_eq!(group.sounds[1].instrument, "PolySynth");
            assert_eq!(group.sounds[0].channel.base_key, 60 + index as i64 % 12);
            for (pattern, original) in group.patterns.iter().zip(source["projects"][index]["patterns"].as_array().unwrap()) {
                let mut native: Vec<_> = pattern.notes.iter().map(|n| (n.sound, n.position, n.length, n.pitch, n.velocity)).collect();
                let mut expected: Vec<_> = original["notes"].as_array().unwrap().iter().map(|n| (
                    n["sound"].as_u64().unwrap() as usize,
                    n["position"].as_i64().unwrap() * PPQ / 96,
                    n["length"].as_i64().unwrap() * PPQ / 96,
                    n["key"].as_i64().unwrap(),
                    n["velocity"].as_i64().unwrap(),
                )).collect();
                native.sort(); expected.sort();
                assert_eq!(native, expected);
            }
            if index == 0 || index == 12 {
                for scene in [0, 5, 6] {
                    let (mut engine, loaded) = OfflineEngine::new(doc.project(), None, 22050, Mode::Scene(scene), &mut SampleCache::default());
                    assert!(loaded.missing.is_empty());
                    let mut audio = vec![0.0f32; 22050];
                    engine.process(&mut audio);
                    assert!(audio.iter().all(|v| v.is_finite()));
                    assert!(audio.iter().any(|v| v.abs() > 0.0001), "silent native scene {scene}");
                }
            }
        }
        let preset = std::fs::read_dir(&output).unwrap().map(|f| f.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "json")).unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(preset).unwrap()).unwrap();
        assert_eq!(saved["harmonic"], saved["source_content"]["harmonic"]);
        assert!(saved["tags"].as_array().unwrap().contains(&json!("Original")));
        let again = install_content(&recipe, &output).unwrap();
        assert_eq!(again["created"], 0);
        assert_eq!(again["existing_preserved"], 48);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn content_keeps_source_notes_tuning_groups_and_existing_files() {
        let root = std::env::temp_dir().join(format!(
            "opendj-content-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let sample = root.join("source.wav");
        sample_edit::write_wav(
            &sample,
            &Sample {
                rate: 8000,
                frames: vec![[0.1; 2]; 800],
            },
        )
        .unwrap();
        let digest: String = Sha256::digest(std::fs::read(sample).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut sounds:Vec<_>=(0..17).map(|i|json!({"name":format!("Original Sound {i}"),"path":"source.wav","sha256":digest,"root_note":57,"fine_tune":12,"channel_pitch_shift":100,"sample_start":10,"sample_end":700,"sample_end_inclusive":false})).collect();
        sounds.push(json!({"name":"Opaque source plugin","plugin":"Original synth state"}));
        let recipe = root.join("recipe.json");
        std::fs::write(&recipe,serde_json::to_vec(&json!({"format":1,"projects":[{"name":"Original pattern","ppq":96,"tempo":134,"velocity_max":128,"sounds":sounds,"patterns":[{"name":"Original notes","length":384,"notes":[{"sound":0,"position":24,"length":48,"key":62,"velocity":128},{"sound":16,"position":96,"length":0,"key":65,"velocity":64},{"sound":17,"position":192,"length":48,"key":60,"velocity":100}]}]}]})).unwrap()).unwrap();
        let output = root.join("presets");
        let first = install_content(&recipe, &output).unwrap();
        assert_eq!(first["imported_projects"], 1);
        assert_eq!(first["presets"], 2);
        assert_eq!(first["notes"], 2);
        assert_eq!(first["created"], 3);
        let converted = first["projects"][0]["converted_project"].as_str().unwrap();
        let doc = Document::from_bytes(std::fs::read(converted).unwrap(), None).unwrap();
        assert_eq!(doc.project().groups.len(), 2);
        assert!(doc.project().groups.iter().all(|g| g.sounds.len() == 16));
        assert_eq!(doc.project().tempo, 134.0);
        let note = &doc.project().groups[1].patterns[0].notes[0];
        assert_eq!(
            (
                note.sound,
                note.position,
                note.length,
                note.pitch,
                note.velocity
            ),
            (0, PPQ, 1, 65, 64)
        );
        let zone = &doc.project().groups[1].sounds[0]
            .sampler
            .as_ref()
            .unwrap()
            .zones[0];
        assert_eq!(zone.root_key, 57);
        assert_eq!(zone.play_range, (10, 699));
        assert!((zone.tune - 1.12).abs() < 1e-6);
        assert!(doc.project().groups[1].sounds[1].sampler.is_none());
        let preset = std::fs::read_dir(&output)
            .unwrap()
            .map(|v| v.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "json"))
            .unwrap();
        let mut custom = std::fs::read(&preset).unwrap();
        custom.extend_from_slice(b"\n \n");
        std::fs::write(&preset, &custom).unwrap();
        let again = install_content(&recipe, &output).unwrap();
        assert_eq!(again["created"], 0);
        assert_eq!(again["existing_preserved"], 3);
        assert_eq!(std::fs::read(&preset).unwrap(), custom);
        std::fs::remove_dir_all(root).unwrap();
    }
}
