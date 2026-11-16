//! Export the selected scene with the same renderer used by DECKARD.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use opendj_engine::maschine::{audio_export, MaschineHandle, Mode, OfflineEngine, SampleCache};

/// Runs on the editor command worker. The document and its original directory
/// are captured together so relative sample references keep their meaning.
pub fn export_scene(
    handle: &MaschineHandle,
    scene: usize,
    folder: PathBuf,
    tempo: f64,
) -> Result<(), String> {
    let (project, origin) = handle.snapshot_with_path()?;
    let mut project = (*project).clone();
    if tempo.is_finite() && tempo > 0.0 {
        project.tempo = tempo;
        project
            .scenes
            .get_mut(scene)
            .ok_or("No scene selected")?
            .tempo = Some(tempo);
    }
    let selected = project.scenes.get(scene).ok_or("No scene selected")?;
    let format = audio_export::Format::default();
    let mut cache = SampleCache::default();
    let (engine, report) = OfflineEngine::new(
        &project,
        origin.as_deref(),
        format.rate,
        Mode::Scene(scene),
        &mut cache,
    );
    drop(engine);
    let problems: Vec<_> = report
        .missing
        .iter()
        .chain(&report.failed)
        .chain(&report.plugin_errors)
        .chain(&report.no_sampler)
        .take(8)
        .cloned()
        .collect();
    if !problems.is_empty() {
        return Err(format!(
            "Audio export could not load {}",
            problems.join(", ")
        ));
    }
    let rendered = audio_export::render(
        &project,
        origin.as_deref(),
        Mode::Scene(scene),
        0.0,
        &format,
        &mut cache,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .ok_or("Audio export cancelled")?;
    if rendered.audio.iter().any(|sample| !sample.is_finite()) {
        return Err("Audio export produced invalid samples".into());
    }
    let bytes = audio_export::encode(
        &rendered.audio,
        rendered.rate,
        format.file_type,
        format.depth,
        None,
    );
    let name = audio_export::safe_name(&format!("{} - {}", project.name, selected.name));
    let destination = audio_export::unique_path(&folder, &name, "wav", &[]);
    crate::maschine_ui::write_new(&destination, &bytes)
}
