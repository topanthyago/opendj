//! Harmonic context for new melodic notes. Playing decks already publish
//! their sounding key, including pitch, speed and key lock, in `key`.
//! Existing sequences retain their original notes.

use crate::{key::Key, scale};

pub const CHROMATIC: u8 = 0;
pub const AUTO: u8 = 1;
pub const MANUAL: u8 = 2;
pub const COMMON: u8 = 3;

#[derive(Clone, Copy, Debug, Default)]
pub struct DeckKey {
    pub key: Option<Key>,
    pub playing: bool,
    pub detected: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub key: Option<Key>,
    pub mask: u16,
    pub deck: Option<usize>,
    /// 0 unavailable, 1 tags or imported metadata, 2 OpenDJ analysis,
    /// 3 manual, 4 shared notes of decks 1 and 2.
    pub source: u8,
    pub playing: bool,
    pub scale: usize,
}

/// A pinned deck can be cued while stopped. Automatic selection follows
/// the playing leader, then the first playing track, excluding this instrument.
pub fn resolve(
    mode: u8,
    chosen_deck: usize,
    root: u8,
    scale_type: usize,
    own_deck: usize,
    decks: &[DeckKey],
    leader: Option<usize>,
) -> Context {
    if mode == COMMON {
        return match (
            decks.first().and_then(|d| d.key),
            decks.get(1).and_then(|d| d.key),
        ) {
            (Some(a), Some(b)) => Context {
                mask: common_notes(a, b),
                source: 4,
                ..Context::default()
            },
            _ => Context::default(),
        };
    }
    if mode == MANUAL {
        let scale_type = scale_type.min(scale::SCALES.len() - 1);
        let key = Key {
            tonic: root % 12,
            minor: scale_type % 2 == 1,
        };
        return Context {
            key: Some(key),
            mask: scale::mask(key.tonic, scale_type),
            source: 3,
            scale: scale_type,
            ..Context::default()
        };
    }
    if mode != AUTO {
        return Context::default();
    }
    let playing = |i: usize| i != own_deck && decks.get(i).is_some_and(|d| d.playing);
    let deck = if chosen_deck > 0 {
        chosen_deck
            .checked_sub(1)
            .filter(|&i| i != own_deck && i < decks.len())
    } else {
        leader
            .filter(|&i| playing(i))
            .or_else(|| (0..decks.len()).find(|&i| playing(i)))
    };
    let Some(index) = deck else {
        return Context::default();
    };
    let reference = decks[index];
    let scale_type = reference.key.map_or(0, |k| usize::from(k.minor));
    Context {
        key: reference.key,
        mask: reference
            .key
            .map_or(0, |k| scale::mask(k.tonic, scale_type)),
        source: reference
            .key
            .map_or(0, |_| if reference.detected { 2 } else { 1 }),
        deck,
        playing: reference.playing,
        scale: scale_type,
    }
}

/// Drum synthesizers stay chromatic. Sample pitch mapping is an explicit
/// opt-in because a Sampler can hold either a drum hit or a pitched instrument.
pub fn maps_sound(instrument: &str, samples: bool) -> bool {
    matches!(instrument, "BassSynth" | "PolySynth")
        || (samples && matches!(instrument, "Sampler" | "AudioModule"))
}

pub fn note(pitch: i64, instrument: &str, samples: bool, mask: u16) -> i64 {
    let pitch = pitch.clamp(0, 127) as u8;
    i64::from(if maps_sound(instrument, samples) {
        scale::nearest(pitch, mask)
    } else {
        pitch
    })
}

pub type HeldNote = (usize, usize, i64);

/// Each physical pad remembers its actual group, Sound and mapped note.
/// Pads sharing a mapped pitch hold one voice until the last pad is released.
pub fn hold(held: &mut [Option<HeldNote>], pad: usize, note: HeldNote) -> bool {
    let first = !held.contains(&Some(note));
    held[pad] = Some(note);
    first
}

pub fn release(held: &mut [Option<HeldNote>], pad: usize) -> Option<HeldNote> {
    let note = held[pad].take()?;
    (!held.contains(&Some(note))).then_some(note)
}

/// Shared pitch classes for an overlap. The wheel relation is a musical
/// heuristic, while this mask states exactly which scale notes are shared.
pub fn common_notes(a: Key, b: Key) -> u16 {
    scale::mask(a.tonic, usize::from(a.minor)) & scale::mask(b.tonic, usize::from(b.minor))
}

pub fn note_names(mask: u16) -> String {
    const NAMES: [&str; 12] = [
        "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
    ];
    (0..12)
        .filter(|n| mask >> n & 1 == 1)
        .map(|n| NAMES[n])
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_twenty_four_keys_map_every_midi_note_into_their_scale() {
        for i in 0..24 {
            let key = Key::from_index(i).unwrap();
            let decks = [DeckKey {
                key: Some(key),
                playing: true,
                detected: true,
            }];
            let context = resolve(AUTO, 0, 0, 0, 2, &decks, Some(0));
            assert_eq!(context.key, Some(key));
            assert_eq!(context.source, 2);
            for pitch in 0..128 {
                let mapped = note(pitch, "BassSynth", false, context.mask);
                assert_ne!(context.mask & (1 << (mapped % 12)), 0);
                if context.mask & (1 << (pitch % 12)) != 0 {
                    assert_eq!(mapped, pitch);
                }
                assert_eq!(note(pitch, "Kick", true, context.mask), pitch);
                assert_eq!(note(pitch, "Sampler", false, context.mask), pitch);
                assert_eq!(note(pitch, "PolySynth", false, 0), pitch);
            }
        }
    }

    #[test]
    fn leader_changes_unknown_keys_and_stopped_pins_are_explicit() {
        let mut decks = [
            DeckKey {
                key: Key::parse("8B"),
                playing: true,
                detected: false,
            },
            DeckKey {
                key: Key::parse("2A"),
                playing: true,
                detected: true,
            },
            DeckKey {
                key: Key::parse("D"),
                playing: true,
                detected: true,
            },
        ];
        let first = resolve(AUTO, 0, 0, 0, 2, &decks, Some(0));
        let second = resolve(AUTO, 0, 0, 0, 2, &decks, Some(1));
        assert_eq!((first.deck, first.source), (Some(0), 1));
        assert_eq!((second.deck, second.source), (Some(1), 2));
        assert_ne!(first.mask, second.mask);
        decks[1].key = None;
        assert_eq!(resolve(AUTO, 0, 0, 0, 2, &decks, Some(1)).mask, 0);
        decks[0].playing = false;
        let pin = resolve(AUTO, 1, 0, 0, 2, &decks, None);
        assert_eq!(pin.key, decks[0].key);
        assert!(!pin.playing);
        decks[1].playing = false;
        assert_eq!(resolve(AUTO, 0, 0, 0, 2, &decks, None), Context::default());
        assert_eq!(
            resolve(CHROMATIC, 1, 0, 0, 2, &decks, None),
            Context::default()
        );
        let manual = resolve(MANUAL, 0, 9, 3, 2, &decks, None);
        assert_eq!(manual.mask, scale::mask(9, 3));
        assert_eq!(manual.source, 3);
    }

    #[test]
    fn transition_notes_and_sound_classes_are_exact() {
        let c = Key::parse("C").unwrap();
        let am = Key::parse("Am").unwrap();
        assert_eq!(common_notes(c, am).count_ones(), 7);
        assert_eq!(note_names(common_notes(c, am)), "C D E F G A B");
        assert_eq!(common_notes(c, Key::parse("F#").unwrap()).count_ones(), 2);
        for drum in ["Kick", "Snare", "Hihat", "Tom", "Perc", "Cymbal"] {
            assert!(!maps_sound(drum, true));
        }
        assert!(maps_sound("Sampler", true));
    }

    #[test]
    fn pads_release_the_original_note_after_key_sound_group_or_mode_changes() {
        let mut held = [None; 16];
        let c = scale::mask(0, 0);
        let db = scale::mask(1, 0);
        let original = (0, 2, note(61, "PolySynth", false, c));
        assert_eq!(original.2, 60);
        assert!(hold(&mut held, 1, original));
        assert!(!hold(&mut held, 0, (0, 2, note(60, "PolySynth", false, c))));
        let changed = (1, 4, note(61, "PolySynth", false, db));
        assert!(hold(&mut held, 2, changed));
        assert_eq!(release(&mut held, 1), None, "another pad holds C");
        assert_eq!(release(&mut held, 0), Some(original));
        assert_eq!(release(&mut held, 2), Some(changed));
        assert_eq!(release(&mut held, 2), None);
        assert!(held.iter().all(Option::is_none));
    }
}
