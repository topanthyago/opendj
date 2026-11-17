//! Pad response and physical encoder calibration for the original Groove controls.
use std::sync::Arc;

use opendj_control::ControlRegistry;
use opendj_engine::pad_response::{self, GROUP};
use serde_json::json;
use slint::ComponentHandle;

use crate::{settings, AppWindow, GrooveCalibrationUi};

fn install_pads(app: &AppWindow, registry: Arc<ControlRegistry>) {
    let saved = settings::load()["maschine_pad_response"].clone();
    for (key, default) in [("curve", 1.0), ("sensitivity", 1.5), ("fixed", 127.0)] {
        if let Some(control) = registry.get(GROUP, key) {
            control.set(
                saved[key]
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .unwrap_or(default),
            );
        }
    }
    app.global::<GrooveCalibrationUi>()
        .on_pad_curve_value(|input, curve, gain, fixed| {
            f32::from(pad_response::velocity(
                input.clamp(0, 127) as u8,
                curve as u8,
                f64::from(gain),
                fixed.clamp(1, 127) as u8,
            )) / 127.0
        });
    app.global::<GrooveCalibrationUi>()
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

fn install_encoders(app: &AppWindow, registry: Arc<ControlRegistry>) {
    let saved = settings::load()["maschine_encoders"].clone();
    for key in [
        "knob_counts",
        "wheel_counts",
        "knob_measured",
        "wheel_measured",
    ] {
        if let (Some(control), Some(value)) = (
            registry.get(opendj_control::maschine_encoders::GROUP, key),
            saved[key].as_f64(),
        ) {
            if value.is_finite() {
                control.set(value);
            }
        }
    }
    app.global::<GrooveCalibrationUi>()
        .on_encoder_calibration(move |action| {
            let get = |key| {
                registry
                    .get(opendj_control::maschine_encoders::GROUP, key)
                    .map_or(0.0, |c| c.get())
            };
            let set = |key, value| {
                if let Some(c) = registry.get(opendj_control::maschine_encoders::GROUP, key) {
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

/// Restore preferences and connect the calibration UI to the shared controls.
pub fn install(app: &AppWindow, registry: &Arc<ControlRegistry>) {
    install_pads(app, Arc::clone(registry));
    install_encoders(app, Arc::clone(registry));
}
