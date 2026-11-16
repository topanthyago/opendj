//! Physical-angle response and optional one-revolution measurements for MK2 encoders.
use crate::{Behavior, ControlRegistry};

pub const GROUP: &str = "[MaschineEncoders]";
// Bridge HID descriptor/previous measurement: 1000 raw counts / divisor 4.
// Large encoder: 16 detents * 8 bridge increments. These are defaults, not a live measurement.
pub const KNOB_COUNTS: f64 = 250.0;
pub const WHEEL_COUNTS: f64 = 128.0;

pub fn register(registry: &ControlRegistry) {
    for (key, value, low, high) in [
        ("knob_counts", KNOB_COUNTS, 16.0, 4096.0),
        ("wheel_counts", WHEEL_COUNTS, 16.0, 4096.0),
        ("measure", 0.0, 0.0, 2.0),
        ("measured", 0.0, -65536.0, 65536.0),
        ("measured_encoder", 0.0, 0.0, 23.0),
        ("knob_measured", 0.0, 0.0, 1.0),
        ("wheel_measured", 0.0, 0.0, 1.0),
    ] {
        registry.create(GROUP, key, value, low, high, Behavior::Free);
    }
}

pub fn counts(registry: &ControlRegistry, wheel: bool) -> f64 {
    registry
        .get(GROUP, if wheel { "wheel_counts" } else { "knob_counts" })
        .map_or(if wheel { WHEEL_COUNTS } else { KNOB_COUNTS }, |c| c.get())
}

/// Signed remainder preserves reversals and does not depend on packet size or timing.
pub fn steps(remainder: &mut f64, delta: f64, threshold: f64) -> i64 {
    *remainder += delta;
    let n = ((*remainder / threshold).abs() + 1e-9).floor() * remainder.signum();
    *remainder -= n * threshold;
    n as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_angle_is_packet_invariant_and_symmetric() {
        for sign in [-1.0, 1.0] {
            for packets in [10, 100] {
                let mut remainder = 0.0;
                let count: i64 = (0..packets)
                    .map(|_| {
                        steps(
                            &mut remainder,
                            sign * KNOB_COUNTS / packets as f64,
                            KNOB_COUNTS / 8.0,
                        )
                    })
                    .sum();
                assert_eq!(count, sign as i64 * 8);
                assert!(remainder.abs() < 1e-8);
            }
        }
        let mut remainder = 0.0;
        assert_eq!(steps(&mut remainder, 15.0, 31.25), 0);
        assert_eq!(steps(&mut remainder, -15.0, 31.25), 0);
        assert_eq!(steps(&mut remainder, -31.25, 31.25), -1);
    }
}
