//! Explicit Groove-role assignments. The material binding prevents a profile
//! from following a different kit, group or set of captured slices by name.
use deckard_model::Group;
use sha2::{Digest, Sha256};

pub const ROWS: usize = 8;
pub const NONE: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub kind: u8,
    pub roles: [usize; ROWS],
    pub material: String,
}

pub fn valid(kind: u8, roles: [usize; ROWS]) -> bool {
    if !(1..=2).contains(&kind) || !roles.iter().any(|&s| s < NONE) {
        return false;
    }
    let mut used = 0u32;
    for sound in roles {
        if sound > NONE {
            return false;
        }
        if sound < NONE {
            let bit = 1 << sound;
            if used & bit != 0 {
                return false;
            }
            used |= bit;
        }
    }
    kind != 1 || roles == [0, 1, 2, 3, 4, 5, 6, 7]
}

pub fn pack(kind: u8, roles: [usize; ROWS]) -> u64 {
    if !valid(kind, roles) {
        return 0;
    }
    let mut bits = (kind as u64) << 40;
    for (row, sound) in roles.into_iter().enumerate() {
        bits |= (if sound == NONE { 0 } else { sound as u64 + 1 }) << (row * 5);
    }
    bits
}

pub fn unpack(bits: u64) -> Option<(u8, [usize; ROWS])> {
    let kind = (bits >> 40) as u8;
    let roles = std::array::from_fn(|r| {
        let value = ((bits >> (r * 5)) & 31) as usize;
        if value == 0 {
            NONE
        } else {
            value - 1
        }
    });
    valid(kind, roles).then_some((kind, roles))
}

/// Stable across rename, note edits and mixer controls; changes when source
/// instruments, sample references or slice bounds change. File size and mtime
/// also catch ordinary replacement of an absolute file at the same path.
pub fn material(group: &Group) -> String {
    let mut hash = Sha256::new();
    fn bytes(hash: &mut Sha256, value: &[u8]) {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    fn sample(hash: &mut Sha256, value: &deckard_model::SampleRef) {
        bytes(hash, value.path.as_bytes());
        bytes(hash, value.library.as_bytes());
        bytes(hash, value.library_root.as_bytes());
        hash.update(value.kind.to_le_bytes());
        let path = std::path::Path::new(&value.path);
        if path.is_absolute() {
            if let Ok(meta) = std::fs::metadata(path) {
                hash.update(meta.len().to_le_bytes());
                if let Ok(time) = meta.modified().and_then(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map_err(std::io::Error::other)
                }) {
                    hash.update(time.as_nanos().to_le_bytes());
                }
            }
        }
    }
    hash.update((group.sounds.len() as u64).to_le_bytes());
    for sound in &group.sounds {
        if let Some(slot) = sound.slots.first() {
            bytes(&mut hash, slot.module.as_bytes());
            if let Some(plugin) = &slot.plugin {
                bytes(&mut hash, plugin.id.as_bytes());
                bytes(&mut hash, &plugin.chunk);
            } else if slot.module != "Sampler" {
                for (name, value) in &slot.params {
                    bytes(&mut hash, name.as_bytes());
                    hash.update(value.to_bits().to_le_bytes());
                }
            }
        } else {
            bytes(&mut hash, &[]);
        }
        if let Some(sampler) = &sound.sampler {
            hash.update([sampler.reverse as u8]);
            hash.update(sampler.envelope_type.to_le_bytes());
            hash.update((sampler.zones.len() as u64).to_le_bytes());
            for zone in &sampler.zones {
                sample(&mut hash, &zone.sample);
                for v in [
                    zone.play_range.0,
                    zone.play_range.1,
                    zone.key_range.0,
                    zone.key_range.1,
                    zone.loop_range.0,
                    zone.loop_range.1,
                    zone.root_key,
                ] {
                    hash.update(v.to_le_bytes());
                }
                hash.update([zone.loop_active as u8]);
            }
        } else {
            hash.update(0u64.to_le_bytes());
        }
        if let Some(audio) = &sound.audio {
            hash.update([audio.mode as u8]);
            hash.update((audio.takes.len() as u64).to_le_bytes());
            for take in &audio.takes {
                sample(&mut hash, &take.sample);
                hash.update(take.play_range.0.to_le_bytes());
                hash.update(take.play_range.1.to_le_bytes());
            }
        } else {
            hash.update(0u64.to_le_bytes());
        }
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn key(material: &str) -> u64 {
    u64::from_str_radix(material.get(..16).unwrap_or(""), 16).unwrap_or(0)
}

pub fn to_json(profile: &Profile) -> serde_json::Value {
    serde_json::json!({"kind":profile.kind,"roles":profile.roles.map(|s| if s == NONE { -1 } else { s as i64 }),"material":profile.material})
}

pub fn from_json(value: &serde_json::Value) -> Option<Profile> {
    let kind = u8::try_from(value.get("kind")?.as_u64()?).ok()?;
    let list = value.get("roles")?.as_array()?;
    if list.len() != ROWS {
        return None;
    }
    let mut roles = [NONE; ROWS];
    for (dst, value) in roles.iter_mut().zip(list) {
        let n = value.as_i64()?;
        *dst = if n == -1 {
            NONE
        } else {
            usize::try_from(n).ok()?
        };
    }
    let material = value.get("material")?.as_str()?.to_owned();
    (valid(kind, roles) && material.len() == 64 && material.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(Profile {
            kind,
            roles,
            material,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_rejects_duplicates_and_roundtrips_optional_roles() {
        let roles = [0, 2, NONE, 5, 6, NONE, NONE, 15];
        assert!(valid(2, roles));
        assert_eq!(unpack(pack(2, roles)), Some((2, roles)));
        assert!(!valid(2, [0, 0, NONE, NONE, NONE, NONE, NONE, NONE]));
        assert!(!valid(1, roles));
        let profile = Profile {
            kind: 2,
            roles,
            material: "ab".repeat(32),
        };
        assert_eq!(from_json(&to_json(&profile)), Some(profile));
    }

    #[test]
    fn identity_ignores_names_but_changes_for_slice_material() {
        let mut group = Group::default();
        group.sounds.push(deckard_model::Sound {
            name: "first".into(),
            sampler: Some(deckard_model::Sampler {
                zones: vec![deckard_model::Zone {
                    sample: deckard_model::SampleRef {
                        path: "sample.wav".into(),
                        ..Default::default()
                    },
                    play_range: (0, 100),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        let first = material(&group);
        group.name = "renamed group".into();
        group.sounds[0].name = "renamed sound".into();
        assert_eq!(material(&group), first);
        group.sounds[0].sampler.as_mut().unwrap().zones[0]
            .play_range
            .1 = 101;
        assert_ne!(material(&group), first);
    }
}
