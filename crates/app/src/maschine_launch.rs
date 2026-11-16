//! Launch controls edit the selected Sound's existing instrument in the document.
use opendj_engine::maschine::{Document, Project, Sound, Target, Value, PPQ};

pub fn mode(sound: Option<&Sound>) -> &'static str {
    let Some(sound) = sound.filter(|s| s.instrument == "Sampler") else {
        return "";
    };
    let Some(sampler) = &sound.sampler else {
        return "";
    };
    let polyphony = sampler.params.get("polyphony").copied().unwrap_or(4.0);
    match sampler.envelope_type {
        0 if polyphony == 1.0 => "retrigger",
        0 if polyphony >= 2.0 => "poly",
        2 if sampler.zones.iter().all(|z| z.loop_active) && !sampler.zones.is_empty() => "loop",
        2 if sampler.zones.iter().all(|z| !z.loop_active) => "gate",
        _ => "custom",
    }
}

pub fn apply(doc: &mut Document, group: usize, sound: usize, action: &str) -> Result<(), String> {
    let selected = doc
        .project()
        .groups
        .get(group)
        .and_then(|g| g.sounds.get(sound))
        .ok_or("No Sound selected")?;
    if selected.instrument == "AudioModule" {
        return audio(doc, group, sound, action);
    }
    let sampler = selected
        .sampler
        .as_ref()
        .filter(|_| selected.instrument == "Sampler")
        .ok_or("Launch modes require a Sampler Sound")?;
    if sampler.zones.is_empty() {
        return Err("Load a sample into the selected Sound".into());
    }
    let (envelope, polyphony, looping) = match action {
        "poly" => (0, 4, false), // Maschine's default eight-note polyphony.
        "retrigger" => (0, 1, false),
        "gate" => (2, 1, false),
        "loop" => (2, 1, true),
        _ => return Err("Unknown Sound launch mode".into()),
    };
    let ranges: Vec<_> = sampler
        .zones
        .iter()
        .map(|z| {
            if z.loop_range.1 > z.loop_range.0 {
                z.loop_range
            } else {
                z.play_range
            }
        })
        .collect();
    if looping && ranges.iter().any(|(low, high)| *low < 0 || high <= low) {
        return Err("Set a sample play range before enabling Loop held".into());
    }
    doc.transaction("Sound launch mode", |doc| {
        let target = Target::Sampler(group, sound);
        doc.set(
            "Sound launch mode",
            target,
            "envelope_type",
            Value::Int(envelope),
        )?;
        doc.set(
            "Sound launch mode",
            target,
            "polyphony",
            Value::Int(polyphony),
        )?;
        if envelope == 2 {
            // A held pad sustains at full level and release closes its gate.
            doc.set(
                "Sound launch mode",
                target,
                "volume_envelope/sustain",
                Value::Float(1.0),
            )?;
            doc.set(
                "Sound launch mode",
                target,
                "volume_envelope/release",
                Value::Float(0.0),
            )?;
        }
        for (zone, (low, high)) in ranges.iter().copied().enumerate() {
            let target = Target::Zone(group, sound, zone);
            doc.set(
                "Sound launch mode",
                target,
                "loop_active",
                Value::Int(i64::from(looping)),
            )?;
            if looping {
                doc.set(
                    "Sound launch mode",
                    target,
                    "loop_range/low",
                    Value::Int(low),
                )?;
                doc.set(
                    "Sound launch mode",
                    target,
                    "loop_range/high",
                    Value::Int(high),
                )?;
            }
        }
        Ok(())
    })
}

fn audio(doc: &mut Document, group: usize, sound: usize, action: &str) -> Result<(), String> {
    let audio = doc.project().groups[group].sounds[sound]
        .audio
        .as_ref()
        .ok_or("No Audio instrument in selected Sound")?;
    match action {
        "audio-loop" | "audio-gate" => doc.set(
            "Audio launch mode",
            Target::Module(group, sound, 0),
            "mode",
            Value::Int(i64::from(action == "audio-gate")),
        ),
        "stretch" | "formant" | "resample" => {
            let engine = match action {
                "resample" => 0,
                "stretch" => 1,
                _ => 2,
            };
            doc.set(
                "Audio tempo engine",
                Target::Module(group, sound, 0),
                "engine",
                Value::Int(engine),
            )
        }
        "fit-1" | "fit-2" | "fit-4" | "fit-8" => {
            let take = audio.focused;
            if take >= audio.takes.len() {
                return Err("Load an Audio take first".into());
            }
            let bars = action
                .trim_start_matches("fit-")
                .parse::<f64>()
                .map_err(|e| e.to_string())?;
            let beats = bars * doc.project().bar_length() as f64 / PPQ as f64;
            doc.set(
                "Audio source length",
                Target::Take(group, sound, take),
                "length",
                Value::Float(beats),
            )
        }
        _ => Err("Unknown Audio launch setting".into()),
    }
}

pub fn audio_bars(project: &Project, sound: Option<&Sound>) -> f64 {
    let beats = sound
        .and_then(|s| s.audio.as_ref())
        .and_then(|a| a.takes.get(a.focused))
        .map_or(0.0, |take| take.beats);
    beats * PPQ as f64 / project.bar_length().max(1) as f64
}

/// Run on the editor worker. The original WAV stays intact and undo restores it.
pub fn fit(
    handle: &opendj_engine::maschine::MaschineHandle,
    generation: u64,
    version: u64,
    group: usize,
    sound: usize,
    bars: u32,
    bpm: f64,
) -> Result<(), String> {
    if ![1, 2, 4, 8].contains(&bars) || !bpm.is_finite() || bpm <= 0.0 {
        return Err("Invalid fit length or tempo".into());
    }
    if handle.project_generation() != generation || handle.snapshot_version() != version {
        return Err("Project changed before preparing Fit; retry".into());
    }
    let project = handle
        .snapshot_cached()
        .ok_or("Maschine project unavailable")?;
    let expected = project
        .groups
        .get(group)
        .and_then(|g| g.sounds.get(sound))
        .filter(|s| s.instrument == "Sampler")
        .cloned()
        .ok_or("Fit requires a Sampler Sound")?;
    let sample = handle
        .sample(group, sound, 0)?
        .ok_or("Sample unavailable")?;
    if handle.project_generation() != generation || handle.snapshot_version() != version {
        return Err("Sample changed while preparing Fit; retry".into());
    }
    let beats = bars as f64 * project.bar_length() as f64 / PPQ as f64;
    let (stretched, edits) = stretch_fit(&sample, &expected, beats, bpm)?;
    let path = crate::maschine_sampling::store_sample(&stretched.frames, stretched.rate)?;
    let length = stretched.frames.len();
    handle
        .edit_deferred_for_project(generation, move |doc| {
            let current = doc
                .project()
                .groups
                .get(group)
                .and_then(|g| g.sounds.get(sound));
            if current != Some(&expected) {
                return Err("Sound changed while preparing Fit; retry".into());
            }
            doc.transaction("Fit sample to tempo", |doc| {
                // This preserves all zone identities, the focused zone and modulation references.
                doc.set_zone_sample(group, sound, 0, &path, length, "Fit sample to tempo")?;
                for (field, value) in edits {
                    doc.set(
                        "Fit sample to tempo",
                        Target::Zone(group, sound, 0),
                        &field,
                        Value::Int(value),
                    )?;
                }
                Ok(())
            })
        })?
        .recv()
        .map_err(|_| "Maschine editor stopped".to_string())??;
    Ok(())
}

fn stretch_fit(
    sample: &opendj_engine::maschine::Sample,
    expected: &Sound,
    beats: f64,
    bpm: f64,
) -> Result<(opendj_engine::maschine::Sample, Vec<(String, i64)>), String> {
    use opendj_engine::maschine::sample_edit;
    let sampler = expected
        .sampler
        .as_ref()
        .ok_or("No Sampler in selected Sound")?;
    let zone = sampler.zones.first().ok_or("Load a sample before Fit")?;
    // The SDK falls back to resampling below two WSOLA windows (80 ms).
    if sample.frames.len() < ((sample.rate as f64 * 0.04) as usize).max(64) * 2 {
        return Err("Pitch-preserving Fit requires at least 80 ms of source audio".into());
    }
    let play = frame_range(zone.play_range, sample.frames.len());
    let range = if zone.loop_active && zone.loop_range.1 > zone.loop_range.0 {
        let looping = frame_range(zone.loop_range, sample.frames.len());
        (looping.0.max(play.0), looping.1.min(play.1))
    } else {
        play
    };
    let frames = range.1.saturating_sub(range.0);
    if frames == 0 {
        return Err("The selected sample range is empty".into());
    }
    // Sampler tune is normalized to +/-36 st. Keep every pitch control unchanged.
    let semitones = sampler.get("tune") * 36.0
        + zone.tune
        + (expected.channel.base_key.clamp(0, 127) - zone.root_key) as f64;
    let target_frames = beats * 60.0 / bpm * sample.rate as f64 * 2f64.powf(semitones / 12.0);
    let ratio = target_frames / frames as f64;
    if !ratio.is_finite() || !(0.05..=20.0).contains(&ratio) {
        return Err("Requested Fit exceeds the stretch engine's 0.05x–20x range".into());
    }
    let stretched = sample_edit::stretch(sample, (0, sample.frames.len()), ratio, 0.0);
    let length = stretched.frames.len();
    let mut edits = Vec::new();
    for (name, original) in [
        ("play_range", zone.play_range),
        ("loop_range", zone.loop_range),
        ("selection_range", zone.selection_range),
    ] {
        if name != "play_range" && original == (0, 0) {
            continue;
        }
        let (low, high) = frame_range(original, sample.frames.len());
        let low = (low as f64 * ratio)
            .round()
            .min(length.saturating_sub(1) as f64) as i64;
        let high =
            ((high as f64 * ratio).round() as usize).clamp(low as usize + 1, length) as i64 - 1;
        edits.push((format!("{name}/low"), low));
        edits.push((format!("{name}/high"), high));
    }
    for (name, frames) in [
        ("attack", zone.fade_in),
        ("decay", zone.fade_out),
        ("x_fade", zone.x_fade),
    ] {
        edits.push((name.into(), (frames as f64 * ratio).round() as i64));
    }
    Ok((stretched, edits))
}

fn frame_range((low, high): (i64, i64), length: usize) -> (usize, usize) {
    if high <= low {
        return (0, length);
    }
    (
        (low.max(0) as usize).min(length),
        ((high.max(0) as usize).saturating_add(1)).min(length),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendj_engine::maschine::{NewModule, ZoneFrom, ZoneSpec};

    #[test]
    fn launch_modes_preserve_sample_pitch_and_other_sound_with_undo_and_save() {
        let mut doc = Document::new_project().unwrap();
        doc.add_slot(0, 0, 0, &NewModule::Sampler).unwrap();
        doc.set_zones(
            0,
            0,
            &[ZoneSpec {
                from: ZoneFrom::Template,
                file: Some("/virtual/original.wav".into()),
                set: vec![
                    ("play_range/low", 0.0),
                    ("play_range/high", 999.0),
                    ("tune", 3.0),
                ],
            }],
            "Sample",
        )
        .unwrap();
        doc.add_pattern(0, 0, "Preserved notes").unwrap();
        doc.add_note(
            0,
            0,
            opendj_engine::maschine::Note {
                sound: 0,
                position: 0,
                length: 120,
                pitch: 63,
                velocity: 91,
                probability: 73,
                expr: None,
            },
        )
        .unwrap();
        let patterns = doc.project().groups[0].patterns.clone();
        let original = doc.project().groups[0].sounds[0].clone();
        let untouched = doc.project().groups[0].sounds[1].clone();
        for action in ["poly", "retrigger", "gate", "loop"] {
            let revision = doc.revision();
            apply(&mut doc, 0, 0, action).unwrap();
            let selected = &doc.project().groups[0].sounds[0];
            assert_eq!(mode(Some(selected)), action);
            assert_eq!(
                selected.sampler.as_ref().unwrap().zones[0].sample,
                original.sampler.as_ref().unwrap().zones[0].sample
            );
            assert_eq!(selected.sampler.as_ref().unwrap().zones[0].tune, 3.0);
            assert_eq!(doc.project().groups[0].sounds[1], untouched);
            assert_eq!(doc.project().groups[0].patterns, patterns);
            assert_eq!(selected.mod_routes, original.mod_routes);
            assert_eq!(selected.modulators, original.modulators);
            let bytes = doc.to_bytes().unwrap();
            let reopened = Document::from_bytes(bytes, None).unwrap();
            assert_eq!(mode(Some(&reopened.project().groups[0].sounds[0])), action);
            if doc.revision() != revision {
                assert!(doc.undo());
                assert_eq!(doc.project().groups[0].sounds[0], original);
            }
        }
        assert!(apply(&mut doc, 0, 0, "invalid").is_err());
        assert_eq!(doc.project().groups[0].sounds[0], original);
    }
    #[test]
    fn fit_stretches_without_changing_pitch_and_accounts_for_pad_tuning() {
        use opendj_engine::maschine::Sample;
        let rate = 8000;
        let sample = Sample {
            rate,
            frames: (0..8000)
                .map(|i| {
                    let value =
                        (i as f32 * std::f32::consts::TAU * 200.0 / rate as f32).sin() * 0.4;
                    [value; 2]
                })
                .collect(),
        };
        let mut sound = Sound::default();
        sound.sampler = Some(Default::default());
        let sampler = sound.sampler.as_mut().unwrap();
        sampler.zones.push(Default::default());
        sampler.zones[0].root_key = sound.channel.base_key;
        sampler.zones[0].play_range = (0, 7999);
        let before = sound.clone();
        let (stretched, edits) = stretch_fit(&sample, &sound, 4.0, 120.0).unwrap();
        assert_eq!(stretched.frames.len(), 16000);
        assert!(edits.contains(&("play_range/high".into(), 15999)));
        let crossings = stretched.frames[2000..14000]
            .windows(2)
            .filter(|w| w[0][0] <= 0.0 && w[1][0] > 0.0)
            .count();
        let hz = crossings as f64 * rate as f64 / 12000.0;
        assert!((hz - 200.0).abs() < 3.0, "stretched pitch {hz} Hz");
        assert_eq!(sound, before);
        sound.sampler.as_mut().unwrap().zones[0].tune = 12.0;
        let (tuned, _) = stretch_fit(&sample, &sound, 4.0, 120.0).unwrap();
        assert_eq!(tuned.frames.len(), 32000);
        let short = Sample {
            rate,
            frames: vec![[0.1; 2]; 80],
        };
        assert!(stretch_fit(&short, &sound, 4.0, 120.0).is_err());
        assert!(stretch_fit(&sample, &sound, 1000.0, 120.0).is_err());
    }
}
