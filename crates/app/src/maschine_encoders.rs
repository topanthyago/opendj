use crate::{settings, AppWindow, MaschineUi};
use opendj_control::{maschine_encoders::GROUP, ControlRegistry};
use serde_json::json;
use slint::ComponentHandle;
use std::sync::Arc;

pub fn install(app: &AppWindow, registry: Arc<ControlRegistry>) {
    let saved = settings::load()["maschine_encoders"].clone();
    for key in [
        "knob_counts",
        "wheel_counts",
        "knob_measured",
        "wheel_measured",
    ] {
        if let (Some(control), Some(value)) = (registry.get(GROUP, key), saved[key].as_f64()) {
            if value.is_finite() {
                control.set(value);
            }
        }
    }
    app.global::<MaschineUi>()
        .on_encoder_calibration(move |action| {
            let get = |key| registry.get(GROUP, key).map_or(0.0, |c| c.get());
            let set = |key, value| {
                if let Some(c) = registry.get(GROUP, key) {
                    c.set(value);
                }
            };
            match action.as_str() {
                "knob" | "wheel" => {
                    set("measured", 0.0);
                    set("measured_encoder", 0.0);
                    set("measure", if action == "wheel" { 2.0 } else { 1.0 });
                    return;
                }
                "save" => {
                    let value = get("measured").abs();
                    if !(16.0..=4096.0).contains(&value) {
                        return;
                    }
                    let wheel = get("measure") == 2.0;
                    if get("measure") == 0.0 {
                        return;
                    }
                    set(if wheel { "wheel_counts" } else { "knob_counts" }, value);
                    set(
                        if wheel {
                            "wheel_measured"
                        } else {
                            "knob_measured"
                        },
                        1.0,
                    );
                    set("measure", 0.0);
                }
                "cancel" => {
                    set("measure", 0.0);
                    return;
                }
                "reset" => {
                    set("knob_counts", 250.0);
                    set("wheel_counts", 128.0);
                    set("knob_measured", 0.0);
                    set("wheel_measured", 0.0);
                    set("measure", 0.0);
                }
                _ => return,
            }
            settings::set(
                "maschine_encoders",
                json!({
                    "knob_counts": get("knob_counts"), "wheel_counts": get("wheel_counts"),
                    "knob_measured": get("knob_measured"), "wheel_measured": get("wheel_measured"),
                }),
            );
        });
}
