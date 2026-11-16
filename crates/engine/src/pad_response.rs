//! Maschine input velocity calibration shared by the controller and its UI.
use opendj_control::{Behavior, ControlRegistry};

pub const GROUP: &str = "[MaschinePads]";

pub fn register(registry: &ControlRegistry) {
    for (key, value, low, high) in [
        ("curve", 1.0, 0.0, 3.0),
        ("sensitivity", 1.5, 0.25, 4.0),
        ("fixed", 127.0, 1.0, 127.0),
        ("last_pad", 0.0, 0.0, 16.0),
        ("last_input", 0.0, 0.0, 127.0),
        ("last_output", 0.0, 0.0, 127.0),
        ("peak_input", 0.0, 0.0, 127.0),
    ] {
        registry.create(GROUP, key, value, low, high, Behavior::Free);
    }
}

/// Input is the bridge's MIDI velocity, with zero retaining note-off semantics.
pub fn velocity(input: u8, curve: u8, sensitivity: f64, fixed: u8) -> u8 {
    if input == 0 {
        return 0;
    }
    let sensitivity = if sensitivity.is_finite() {
        sensitivity.clamp(0.25, 4.0)
    } else {
        1.0
    };
    let value = (f64::from(input.min(127)) * sensitivity / 127.0).min(1.0);
    let value = match curve {
        1 => value.sqrt(),
        2 => value * value,
        3 => f64::from(fixed.clamp(1, 127)) / 127.0,
        _ => value,
    };
    (value * 127.0).round().clamp(1.0, 127.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_preserve_release_and_are_monotonic() {
        for curve in 0..=3 {
            assert_eq!(velocity(0, curve, 1.5, 100), 0);
            let values: Vec<_> = (1..=127).map(|v| velocity(v, curve, 1.5, 100)).collect();
            assert!(values.windows(2).all(|v| v[0] <= v[1]));
            assert!(values.iter().all(|v| (1..=127).contains(v)));
        }
        assert_eq!(velocity(85, 1, 1.5, 127), 127);
        assert!(velocity(40, 1, 1.0, 127) > velocity(40, 0, 1.0, 127));
        assert!(velocity(40, 2, 1.0, 127) < velocity(40, 0, 1.0, 127));
        assert_eq!(velocity(5, 3, 1.0, 100), 100);
    }
}
