use std::sync::Arc;

use opendj_control::ControlRegistry;
use opendj_engine::pad_response::{self, GROUP};
use serde_json::json;
use slint::ComponentHandle;

use crate::{settings, AppWindow, MaschineUi};

pub fn install(app: &AppWindow, registry: Arc<ControlRegistry>) {
    let saved = settings::load()["maschine_pad_response"].clone();
    for (key, default) in [("curve", 1.0), ("sensitivity", 1.5), ("fixed", 127.0)] {
        if let Some(control) = registry.get(GROUP, key) {
            control.set(saved[key].as_f64().unwrap_or(default));
        }
    }
    app.global::<MaschineUi>()
        .on_pad_curve_value(|input, curve, gain, fixed| {
            f32::from(pad_response::velocity(
                input.clamp(0, 127) as u8,
                curve as u8,
                f64::from(gain),
                fixed.clamp(1, 127) as u8,
            )) / 127.0
        });
    app.global::<MaschineUi>()
        .on_pad_calibration(move |key, value| {
            let get = |key| registry.get(GROUP, key).map_or(0.0, |c| c.get());
            let set = |key, value| {
                if let Some(control) = registry.get(GROUP, key) {
                    control.set(value);
                }
            };
            match key.as_str() {
                "curve" => set("curve", f64::from(value).round()),
                "sensitivity" | "fixed" => set(key.as_str(), f64::from(value)),
                "match" if get("last_input") > 0.0 => set("sensitivity", 127.0 / get("last_input")),
                "reset" => {
                    set("curve", 1.0);
                    set("sensitivity", 1.5);
                    set("fixed", 127.0);
                }
                "clear" => {
                    for key in ["last_pad", "last_input", "last_output", "peak_input"] {
                        set(key, 0.0);
                    }
                    return;
                }
                _ => return,
            }
            settings::set(
                "maschine_pad_response",
                json!({
                    "curve": get("curve"), "sensitivity": get("sensitivity"), "fixed": get("fixed"),
                }),
            );
        });
}
