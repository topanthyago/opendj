//! Content installation runs independently of the playing Maschine document.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use slint::ComponentHandle;

use crate::{prefs, presets, settings, AppWindow, MaschineUi};

pub fn install(app: &AppWindow) {
    let weak = app.as_weak();
    app.global::<MaschineUi>().on_import_content(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.global::<MaschineUi>().get_content_import_busy() {
            return;
        }
        let weak = app.as_weak();
        prefs::choose_file("Import content bank · choose native-recipes.json", move |path| {
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                let ui = app.global::<MaschineUi>();
                if ui.get_content_import_busy() {
                    return;
                }
                let Some(folder) = settings::config_dir() else {
                    ui.set_content_import_report("Could not locate the preset folder.".into());
                    return;
                };
                ui.set_content_import_busy(true);
                ui.set_content_import_report(format!(
                    "Importing {}…\nPreparing samples, projects and presets. You can keep playing while the import runs.",
                    path.display()
                ).into());
                let weak = app.as_weak();
                let worker = std::thread::Builder::new()
                    .name("content import".into())
                    .spawn(move || {
                        let report = import(&path, &folder.join("Instruments/Presets/Groove Box"))
                            .unwrap_or_else(|error| format!("Import failed\n{error}"));
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(app) = weak.upgrade() {
                                // A failed bank may still have installed some valid recipes.
                                presets::refresh_installed(&app);
                                let ui = app.global::<MaschineUi>();
                                ui.set_content_import_busy(false);
                                ui.set_content_import_report(report.into());
                            }
                        });
                    });
                if let Err(error) = worker {
                    ui.set_content_import_busy(false);
                    ui.set_content_import_report(format!("Could not start import\n{error}").into());
                }
            });
        });
    });
}

fn importer() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("OPENDJ_CONTENT_IMPORTER") {
        return Ok(PathBuf::from(path));
    }
    std::env::current_exe()
        .map(|path| path.with_file_name("opendj-install-brazilian-presets"))
        .map_err(|error| format!("Could not locate the content importer ({error})"))
}

fn import(path: &Path, folder: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err(format!("Content file is unavailable\n{}", path.display()));
    }
    let helper = importer()?;
    let output = Command::new(&helper)
        .arg("--content")
        .arg(path)
        .arg(folder)
        .output()
        .map_err(|error| format!("Could not run {} ({error})", helper.display()))?;
    if !output.status.success() {
        let message: String = String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(4000)
            .collect();
        return Err(format!("Importer exited with {}\n{message}", output.status));
    }
    let report: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Could not read the import report ({error})"))?;
    Ok(summary(&report))
}

fn summary(report: &Value) -> String {
    let count = |field: &str| report[field].as_u64().unwrap_or(0);
    let mut lines = vec![format!(
        "{} projects · {} presets · {} notes\n{} files created · {} existing files preserved",
        count("imported_projects"),
        count("presets"),
        count("notes"),
        count("created"),
        count("existing_preserved")
    )];
    if count("imported_projects") == 0 {
        lines
            .push("No projects were imported. Check the content bank and its source files.".into());
    } else {
        lines.push("Choose installed sounds in Instruments → Presets → Groove Box.".into());
    }
    for project in report["projects"].as_array().into_iter().flatten() {
        let name = project["name"].as_str().unwrap_or("Project");
        for warning in project["warnings"].as_array().into_iter().flatten() {
            if let Some(warning) = warning.as_str() {
                lines.push(format!("{name} — {warning}"));
            }
        }
    }
    for error in report["errors"].as_array().into_iter().flatten() {
        lines.push(format!(
            "{} — {}",
            error["name"].as_str().unwrap_or("Project"),
            error["error"].as_str().unwrap_or("Import failed")
        ));
    }
    lines.join("\n\n")
}
