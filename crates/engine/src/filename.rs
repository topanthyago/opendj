//! Conservative file-name evidence for metadata lookup. Never writes tags.
use std::path::Path;

/// A downloader's terminal video id, proved by brackets, parentheses or a dash.
/// An arbitrary eleven-character artist name alone is never an id.
pub fn youtube_suffix(stem: &str) -> Option<(&str, &str)> {
    let stem = stem.trim();
    let (prefix, id) = if let Some(inner) = stem.strip_suffix(']') {
        inner.rsplit_once('[')?
    } else if let Some(inner) = stem.strip_suffix(')') {
        inner.rsplit_once('(')?
    } else {
        let at = stem.len().checked_sub(12)?;
        if !stem.is_char_boundary(at) || stem.as_bytes()[at] != b'-' {
            return None;
        }
        (&stem[..at], &stem[at + 1..])
    };
    if id.len() != 11
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        return None;
    }
    let prefix = prefix
        .trim()
        .strip_suffix(" -")
        .unwrap_or(prefix.trim())
        .trim();
    (!prefix.is_empty()).then_some((prefix, id))
}

/// Artist - Title, with an explicitly marked leading track number removed.
/// Punctuation and numbers in artist names are kept verbatim.
pub fn from_file_name(path: &Path) -> Option<(String, String)> {
    let stem = path.file_stem()?.to_string_lossy();
    let mut rest = youtube_suffix(&stem).map_or(stem.trim(), |(prefix, _)| prefix);
    let digits = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(0);
    let number = &rest[..digits];
    let tail = &rest[digits..];
    let marked = tail.starts_with(" - ")
        || tail
            .strip_prefix(['.', ')'])
            .is_some_and(|s| s.starts_with(char::is_whitespace))
        || (number.starts_with('0') || number.contains('-'))
            && tail.starts_with(char::is_whitespace);
    if digits > 0 && digits <= 5 && marked {
        rest = tail.trim_start_matches(['.', ' ', ')']);
        rest = rest.strip_prefix("- ").unwrap_or(rest).trim();
    }
    let (artist, title) = rest.split_once(" - ")?;
    let (artist, title) = (artist.trim(), title.trim());
    (!artist.is_empty() && !title.is_empty()).then(|| (artist.to_string(), title.to_string()))
}

/// Names for searches, removing only the id proved by this file's basename.
/// Recovers a missing title from downloader names when the existing artist
/// corroborates its position. Existing independent metadata is preserved.
pub fn lookup_names(path: &Path, artist: &str, title: &str) -> (String, String) {
    let unchanged = || (artist.to_string(), title.to_string());
    let Some(stem) = path.file_stem() else {
        return unchanged();
    };
    let stem = stem.to_string_lossy();
    let Some((prefix, id)) = youtube_suffix(&stem) else {
        return unchanged();
    };
    // VirtualDJ has historically rendered underscores in this id as spaces.
    let legacy = id.replace('_', " ");
    let clean = |original: &str| {
        let value = original.trim();
        for id in [id, legacy.as_str()] {
            for suffix in [format!("[{id}]"), format!("({id})"), format!("-{id}")] {
                if let Some(before) = value.strip_suffix(&suffix) {
                    return before
                        .trim()
                        .strip_suffix(" -")
                        .unwrap_or(before.trim())
                        .trim()
                        .to_string();
                }
            }
            if value == id {
                return String::new();
            }
        }
        original.to_string()
    };
    let mut a = clean(artist);
    let mut t = clean(title);
    // The dated downloader template explicitly orders Title - Artist - (date).
    let dated = prefix.rsplit_once(" - (").and_then(|(names, date)| {
        let date = date.strip_suffix(')')?;
        (date.len() == 10
            && date.bytes().enumerate().all(|(i, c)| {
                if matches!(i, 4 | 7) {
                    c == b'-'
                } else {
                    c.is_ascii_digit()
                }
            }))
        .then_some(names)
    });
    if let Some((song, performer)) = dated.and_then(|s| s.split_once(" - ")) {
        if !performer.contains(" - ")
            && (a == song || a == performer || a.is_empty())
            && (t.is_empty() || title.trim() == stem.trim())
        {
            return (performer.trim().to_string(), song.trim().to_string());
        }
    }
    if t.is_empty() || title.trim() == stem.trim() {
        if let Some(song) = prefix.strip_prefix(&format!("{} - ", a)) {
            t = song.to_string();
        } else if a.is_empty() {
            if let Some((performer, song)) = from_file_name(path) {
                a = performer;
                t = song;
            } else {
                t = prefix.to_string();
            }
        } else if !prefix.contains(" - ") {
            t = prefix.to_string();
        }
    }
    if t.is_empty() {
        return unchanged();
    }
    (a, t)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_artists_and_uses_only_proven_ids() {
        let p = Path::new("OneRepublic - Counting Stars [hT_nvWreIhg].mp4");
        assert_eq!(
            lookup_names(p, "OneRepublic", " (hT nvWreIhg)"),
            ("OneRepublic".into(), "Counting Stars".into())
        );
        let p = Path::new("Celebration - Kool & The Gang - (2009-10-05) - [3GwjfUFyY6M].mp4");
        assert_eq!(
            lookup_names(p, "Celebration", " (3GwjfUFyY6M)"),
            ("Kool & The Gang".into(), "Celebration".into())
        );
        for name in ["OneRepublic", "Discopraise", "Blackstreet"] {
            assert!(youtube_suffix(name).is_none());
            assert_eq!(
                lookup_names(Path::new("Song.mp3"), name, "Real title"),
                (name.into(), "Real title".into())
            );
        }
        for name in ["50 Cent", "808 State", "Lane_8", "a-ha", "2Pac"] {
            assert_eq!(
                from_file_name(Path::new(&format!("{name} - Song [hT_nvWreIhg].mp3"))),
                Some((name.into(), "Song".into()))
            );
        }
        assert_eq!(
            from_file_name(Path::new("01 - Bicep - Glue.mp3")),
            Some(("Bicep".into(), "Glue".into()))
        );
        assert_eq!(
            youtube_suffix("Song-hT_nvWreIhg"),
            Some(("Song", "hT_nvWreIhg"))
        );
        assert!(youtube_suffix("Song [short]").is_none());
        assert_eq!(
            lookup_names(p, "Real Artist", "Real title"),
            ("Real Artist".into(), "Real title".into())
        );
    }
}
