//! The Groove Box's library of named grooves, by genre.
//!
//! Grooves are text files in `data/grooves/`, built into the app (see
//! `basics.txt` for the format). Each groove has a step pattern for each of
//! the kits' nine sounds ([`crate::kits::ROLES`]), and every step one of
//! three levels: soft, normal or accented.
//!
//! The library is read once, the first time it is asked for (when the
//! Groove Boxes' controls are made, before any audio runs); after that it
//! is only read, so the audio thread can load a groove without allocating.

use std::sync::OnceLock;

use crate::kits::ROLES;

/// How loud a step plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Level {
    #[default]
    Normal,
    Soft,
    Accent,
}

impl Level {
    /// Step `step`'s level, from the masks of soft and accented steps.
    pub fn of(step: usize, soft: u32, accent: u32) -> Self {
        if accent & (1 << step) != 0 {
            Level::Accent
        } else if soft & (1 << step) != 0 {
            Level::Soft
        } else {
            Level::Normal
        }
    }

    /// The gain a hit at this level plays at.
    pub fn gain(self) -> f32 {
        match self {
            Level::Normal => 1.0,
            Level::Soft => 0.5,
            Level::Accent => 1.25,
        }
    }
}

/// One sound's steps in a groove: bit s of `hits` on = step s+1 plays,
/// softly when bit s of `soft` is on, accented when bit s of `accent` is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Steps {
    pub hits: u32,
    pub soft: u32,
    pub accent: u32,
    /// Steps that play only half the time.
    pub maybe: u32,
}

impl Steps {
    /// Reads 16 steps written as text: `.` rest, `o` soft, `x` hit, `X`
    /// accent.
    pub fn parse(text: &str) -> Steps {
        let mut s = Steps::default();
        for (i, c) in text.chars().take(16).enumerate() {
            let level = match c {
                'x' => Level::Normal,
                'o' => Level::Soft,
                'X' => Level::Accent,
                '?' => {
                    s.maybe |= 1 << i;
                    Level::Normal
                }
                _ => continue,
            };
            s.hits |= 1 << i;
            match level {
                Level::Soft => s.soft |= 1 << i,
                Level::Accent => s.accent |= 1 << i,
                Level::Normal => {}
            }
        }
        s
    }

    pub fn level(&self, step: usize) -> Level {
        Level::of(step, self.soft, self.accent)
    }
}

pub struct Groove {
    pub name: String,
    pub about: String,
    pub rows: [Steps; ROLES.len()],
}

pub struct Genre {
    pub name: String,
    pub grooves: Vec<Groove>,
}

/// The files the library is read from, in order: openDJ's own grooves,
/// mpump's (AGPL-3.0, see NOTICE), and the Brazilian performance library.
const FILES: [&str; 3] = [
    include_str!("../data/grooves/basics.txt"),
    include_str!("../data/grooves/mpump.txt"),
    include_str!("../data/grooves/brazilian.txt"),
];

const SET_CATALOG: &str = include_str!("../data/grooves/brazilian-catalog.json");

/// Settings that belong to a library set, prepared before audio starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SetDefaults {
    pub kit: usize,
    pub swing: f64,
    pub fill_style: f64,
    pub auto_fill: f64,
    pub levels: [f64; crate::groove::ROWS],
    pub sounds: [f64; crate::groove::ROWS],
    pub pitches: [f64; crate::groove::ROWS],
    pub decay: [f64; crate::groove::ROWS],
}

/// A catalog's metadata for a group of four. Unknown genres and incomplete
/// or invalid entries have no defaults; their patterns can still be loaded.
/// Called once when controls are created, then only read on the audio thread.
pub fn set_defaults(genre: usize, set: usize) -> Option<&'static SetDefaults> {
    static SETS: OnceLock<Vec<Vec<Option<SetDefaults>>>> = OnceLock::new();
    let sets = SETS.get_or_init(|| {
        let library = library();
        let mut out: Vec<Vec<Option<SetDefaults>>> = library
            .iter()
            .map(|g| vec![None; g.grooves.len() / crate::groove::GROOVES])
            .collect();
        let Ok(catalog) = serde_json::from_str::<serde_json::Value>(SET_CATALOG) else {
            return out;
        };
        let Some(families) = catalog.get("families").and_then(|v| v.as_array()) else {
            return out;
        };
        for family in families {
            let Some(genre) = family
                .get("genreName")
                .and_then(|v| v.as_str())
                .and_then(|name| library.iter().position(|g| g.name == name))
            else {
                continue;
            };
            let Some(sets) = family.get("sets").and_then(|v| v.as_array()) else {
                continue;
            };
            for set in sets {
                let Some(index) = set
                    .get("librarySetIndex")
                    .and_then(|v| v.as_u64())
                    .and_then(|n| usize::try_from(n).ok())
                else {
                    continue;
                };
                if let Some(slot) = out[genre].get_mut(index) {
                    *slot = parse_set_defaults(set);
                }
            }
        }
        out
    });
    sets.get(genre)?.get(set)?.as_ref()
}

fn parse_set_defaults(value: &serde_json::Value) -> Option<SetDefaults> {
    use crate::groove::ROWS;
    let number = |key: &str, min: f64, max: f64| {
        value
            .get(key)?
            .as_f64()
            .filter(|v| v.is_finite() && *v >= min && *v <= max)
    };
    let rows = |key: &str, min: f64, max: f64| -> Option<[f64; ROWS]> {
        let values = value.get(key)?.as_array()?;
        if values.len() != ROWS {
            return None;
        }
        let mut result = [0.0; ROWS];
        for (out, v) in result.iter_mut().zip(values) {
            *out = v
                .as_f64()
                .filter(|n| n.is_finite() && *n >= min && *n <= max)?;
        }
        Some(result)
    };
    let kit = usize::try_from(value.get("kitIndex")?.as_u64()?).ok()?;
    let sounds = crate::kits::KITS.get(kit)?.sounds().count();
    let integer = |key: &str, max: u64| value.get(key)?.as_u64().filter(|&n| n <= max);
    let sound_rows = value.get("rowSounds")?.as_array()?;
    if sound_rows.len() != ROWS {
        return None;
    }
    let mut selected_sounds = [0.0; ROWS];
    for (out, sound) in selected_sounds.iter_mut().zip(sound_rows) {
        let index = usize::try_from(sound.as_u64()?).ok()?;
        if index >= sounds {
            return None;
        }
        *out = index as f64;
    }
    Some(SetDefaults {
        kit,
        swing: number("swing", 0.0, 0.75)?,
        fill_style: integer("fillStyle", 1)? as f64,
        auto_fill: integer("autoFillMode", 2)? as f64,
        levels: rows("rowLevels", 0.0, 1.5)?,
        sounds: selected_sounds,
        pitches: rows("rowPitches", -12.0, 12.0)?,
        decay: rows("rowDecay", 0.0, 1.0)?,
    })
}

/// Every genre, Basics first.
pub fn library() -> &'static [Genre] {
    static LIBRARY: OnceLock<Vec<Genre>> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        let mut genres = Vec::new();
        for file in FILES {
            parse(file, &mut genres);
        }
        genres
    })
}

/// The groove `index` (0-based) of genre `genre`, if there is one.
pub fn groove(genre: usize, index: usize) -> Option<&'static Groove> {
    library().get(genre)?.grooves.get(index)
}

/// Reads a grooves file into `genres`; a genre named again adds to the
/// one already there. Lines it doesn't understand are skipped.
fn parse(text: &str, genres: &mut Vec<Genre>) {
    let mut genre: Option<usize> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix("genre ") {
            let name = name.trim();
            genre = Some(match genres.iter().position(|g| g.name == name) {
                Some(i) => i,
                None => {
                    genres.push(Genre {
                        name: name.to_string(),
                        grooves: Vec::new(),
                    });
                    genres.len() - 1
                }
            });
        } else if let Some(rest) = line.strip_prefix("groove ") {
            let Some(g) = genre else { continue };
            let (name, about) = rest.split_once('|').unwrap_or((rest, ""));
            genres[g].grooves.push(Groove {
                name: name.trim().to_string(),
                about: about.trim().to_string(),
                rows: Default::default(),
            });
        } else if let Some((key, steps)) = line.split_once(char::is_whitespace) {
            let Some(groove) = genre.and_then(|g| genres[g].grooves.last_mut()) else {
                continue;
            };
            let role = ROLES
                .iter()
                .position(|r| r.to_ascii_lowercase().replace(' ', "-") == key);
            if let Some(r) = role {
                groove.rows[r] = Steps::parse(steps.trim());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_keep_their_levels() {
        let s = Steps::parse("x.o.X...........");
        assert_eq!(s.hits, 0b10101);
        assert_eq!(s.level(0), Level::Normal);
        assert_eq!(s.level(2), Level::Soft);
        assert_eq!(s.level(4), Level::Accent);
    }

    #[test]
    fn the_library_reads_every_genre_and_groove() {
        let lib = library();
        assert_eq!(lib[0].name, "Basics");
        assert_eq!(lib[0].grooves[0].name, "Four to the floor");
        for g in lib {
            assert!(!g.grooves.is_empty(), "{} is empty", g.name);
            for groove in &g.grooves {
                assert!(
                    groove.rows.iter().any(|r| r.hits != 0),
                    "{} / {} has no hits",
                    g.name,
                    groove.name
                );
            }
        }
    }

    #[test]
    fn rows_are_read_by_sound_name() {
        let mut genres = Vec::new();
        parse(
            "genre Test\ngroove One | about\nkick x...\nopen-hat ..x.\nnope xxxx\n",
            &mut genres,
        );
        let g = &genres[0].grooves[0];
        assert_eq!(g.about, "about");
        assert_eq!(g.rows[0].hits, 1);
        assert_eq!(g.rows[3].hits, 4);
        assert_eq!(g.rows.iter().filter(|r| r.hits != 0).count(), 2);
    }

    #[test]
    fn brazilian_sets_have_valid_bundled_defaults() {
        let catalog: serde_json::Value = serde_json::from_str(SET_CATALOG).unwrap();
        let families = catalog["families"].as_array().unwrap();
        assert_eq!(families.len(), 7);
        for family in families {
            let name = family["genreName"].as_str().unwrap();
            let genre = library().iter().position(|g| g.name == name).unwrap();
            for set in family["sets"].as_array().unwrap() {
                let index = set["librarySetIndex"].as_u64().unwrap() as usize;
                let defaults = set_defaults(genre, index).expect("valid set defaults");
                assert_eq!(defaults.kit, set["kitIndex"].as_u64().unwrap() as usize);
                assert_eq!(defaults.levels[0], set["rowLevels"][0].as_f64().unwrap());
            }
        }
        assert!(
            set_defaults(0, 0).is_none(),
            "generic genres keep their settings"
        );
        assert!(parse_set_defaults(&serde_json::json!({"kitIndex": 999})).is_none());
    }
}
