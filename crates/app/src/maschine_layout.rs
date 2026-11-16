//! Screen panels are user preferences. Percussion mappings remain in the project.
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use opendj_control::ControlRegistry;
use serde_json::json;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::{settings, AppWindow, MaschineUi};

const PANELS: [&str; 5] = [
    "Pads",
    "Steps / piano",
    "Parameters",
    "Browser",
    "Performance",
];
const EDIT: [bool; 5] = [true, true, true, false, true];

fn show(app: &AppWindow, panels: [bool; 5]) {
    let ui = app.global::<MaschineUi>();
    ui.set_show_pads(panels[0]);
    ui.set_show_editor(panels[1]);
    ui.set_show_parameters(panels[2]);
    ui.set_show_browser(panels[3]);
    ui.set_show_performance(panels[4]);
}

fn valid_roles(roles: &[i32]) -> bool {
    roles.iter().any(|&sound| sound >= 0)
        && roles.iter().enumerate().all(|(r, &sound)| {
            (-1..16).contains(&sound) && (sound < 0 || !roles[..r].contains(&sound))
        })
}

pub fn install(app: &AppWindow, registry: Arc<ControlRegistry>) {
    let saved = settings::load()["maschine_view"].clone();
    let panels = std::array::from_fn(|i| saved[PANELS[i]].as_bool().unwrap_or(EDIT[i]));
    show(app, panels);
    app.global::<MaschineUi>()
        .set_context(saved["context"].as_i64().unwrap_or(0).clamp(0, 2) as i32);
    let weak = app.as_weak();
    app.global::<MaschineUi>().on_view_change(move |panel| {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<MaschineUi>();
        let mut panels = [
            ui.get_show_pads(),
            ui.get_show_editor(),
            ui.get_show_parameters(),
            ui.get_show_browser(),
            ui.get_show_performance(),
        ];
        match panel.as_str() {
            "Perform" => panels = [false, false, false, false, true],
            "Edit" => panels = EDIT,
            "Compact" => panels = [false, false, false, false, false],
            name => {
                if let Some(i) = PANELS.iter().position(|&p| p == name) {
                    panels[i] = !panels[i];
                }
            }
        }
        show(&app, panels);
        settings::set(
            "maschine_view",
            json!({
                "Pads": panels[0], "Steps / piano": panels[1], "Parameters": panels[2],
                "Browser": panels[3], "Performance": panels[4], "context": ui.get_context(),
            }),
        );
    });

    let roles = Rc::new(VecModel::from(vec![-1i32; 8]));
    app.global::<MaschineUi>()
        .set_rhythm_roles(ModelRc::from(roles.clone()));
    let editing_group = Rc::new(Cell::new(-1));
    let weak = app.as_weak();
    let reg = registry.clone();
    let model = roles.clone();
    let selected = editing_group.clone();
    app.global::<MaschineUi>().on_rhythm_edit(move |group| {
        let Some(app) = weak.upgrade() else { return };
        if !(0..8).contains(&group) {
            return;
        }
        selected.set(group);
        app.global::<MaschineUi>().set_rhythm_edit_group(group);
        app.global::<MaschineUi>().set_rhythm_map_changed(false);
        let prefix = format!("group{}_rhythm_", group + 1);
        let legacy = reg
            .get("[Maschine3]", &format!("{prefix}profile"))
            .is_some_and(|c| c.get() == 1.0);
        let values: Vec<i32> = (0..8)
            .map(|r| {
                if legacy {
                    r
                } else {
                    reg.get("[Maschine3]", &format!("{prefix}role{}", r + 1))
                        .map_or(-1, |c| c.get() as i32)
                }
            })
            .collect();
        for (r, &value) in values.iter().enumerate() {
            model.set_row_data(r, value);
        }
        app.global::<MaschineUi>()
            .set_rhythm_map_valid(valid_roles(&values));
    });
    let weak = app.as_weak();
    let model = roles.clone();
    app.global::<MaschineUi>()
        .on_rhythm_role(move |role, sound| {
            let Some(app) = weak.upgrade() else { return };
            if !(0..8).contains(&role) || !(-1..16).contains(&sound) {
                return;
            }
            model.set_row_data(role as usize, sound);
            app.global::<MaschineUi>().set_rhythm_map_changed(true);
            app.global::<MaschineUi>()
                .set_rhythm_map_valid(valid_roles(&model.iter().collect::<Vec<_>>()));
        });
    app.global::<MaschineUi>().on_rhythm_apply(move |group| {
        let values = roles.iter().collect::<Vec<_>>();
        let current_group = registry
            .get("[Maschine3]", "group")
            .map_or(-1, |c| c.get() as i32);
        if group != editing_group.get() || current_group != group || !valid_roles(&values) {
            return;
        }
        let prefix = format!("group{}_rhythm_", group + 1);
        // Suspend the previous profile while publishing all role assignments.
        if let Some(c) = registry.get("[Maschine3]", &format!("{prefix}profile")) {
            c.set(0.0);
        }
        for (r, value) in values.into_iter().enumerate() {
            if let Some(c) = registry.get("[Maschine3]", &format!("{prefix}role{}", r + 1)) {
                c.set(f64::from(value));
            }
        }
        if let Some(c) = registry.get("[Maschine3]", &format!("{prefix}profile")) {
            c.set(2.0);
        }
    });
}
