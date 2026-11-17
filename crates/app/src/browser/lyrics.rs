//! The side panel's Lyrics tab: the lyrics of the track playing (or the
//! one selected, or a deck's), read from an `.lrc` file beside it or from
//! its tags (see [`opendj_engine::lyrics`]). Timed lyrics follow the deck
//! the track is on: the line sung now is lit, and a click on a line plays
//! from it. Find searches LRCLIB and Letras when enabled. A selection is
//! previewed before explicit import, with a backup of existing lyric files.
//!
//! The same lyrics show large in Player mode when the Player bar's Lyrics
//! button is on, and on the karaoke screen (see [`karaoke`]); both always
//! follow the track playing.

mod karaoke;
mod timing;
pub(super) mod wave;

use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use opendj_control::{ControlHandle, ControlRegistry};
use opendj_engine::lyrics::{self, Lyrics, Origin, Timing};
use opendj_engine::{deck_group, Source, Track, NUM_DECKS};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::Browser;
use crate::letras;
use crate::lrclib::{self, Found, Miss, Query};
use crate::{settings, AppWindow, LyricLine, LyricMatch, LyricsUi};

/// The side panel's Lyrics tab. Past Prep's Cues (4) and Checklist (5),
/// which come and go with the layout.
pub(super) const LYRICS_TAB: usize = 6;

/// What the tab follows.
const PLAYING: i32 = 0;
const SELECTED: i32 = 1;
/// 2 and 3 are decks A and B.
const FIRST_DECK: i32 = 2;

/// Results and per-source status, including partial success.
struct Looked {
    found: Vec<Found>,
    note: String,
}

/// A deck's controls the tab reads and sets.
struct DeckPlay {
    play: ControlHandle,
    playposition: ControlHandle,
    duration: ControlHandle,
}

pub(super) struct LyricsState {
    subject: i32,
    decks: Vec<DeckPlay>,
    /// The deck the Playing choice last followed, kept while it plays on.
    playing: Option<usize>,
    /// The track shown, where it is in `library`, and the deck it is on.
    shown: Option<(usize, Arc<Track>)>,
    deck: Option<usize>,
    /// The lyrics being read for the track shown, by generation.
    job: Option<(u64, Receiver<Option<Lyrics>>)>,
    generation: u64,
    lyrics: Option<Lyrics>,
    current: i32,
    /// The current line split into sung and to come, as last shown.
    sung: (String, String),
    /// Online look-up running, by generation, and what it found.
    finding: Option<(u64, Receiver<Looked>)>,
    found: Vec<Found>,
    query: Query,
    /// File and track generation that own the search and its editable preview.
    lookup_target: Option<(u64, std::path::PathBuf)>,
    preview: Option<(usize, Receiver<Result<String, letras::Error>>)>,
    selected: Option<usize>,
    karaoke: karaoke::Karaoke,
    wave: wave::WaveState,
    /// Words being timed with Whisper (Time words).
    timing: Option<timing::Job>,
}

impl LyricsState {
    pub(super) fn new(registry: &ControlRegistry) -> Self {
        let decks = (0..NUM_DECKS)
            .map(|d| {
                let g = deck_group(d);
                let c = |k: &str| registry.get(&g, k).expect("deck control");
                DeckPlay {
                    play: c("play"),
                    playposition: c("playposition"),
                    duration: c("duration"),
                }
            })
            .collect();
        Self {
            subject: PLAYING,
            decks,
            playing: None,
            shown: None,
            deck: None,
            job: None,
            generation: 0,
            lyrics: None,
            current: -1,
            sung: Default::default(),
            finding: None,
            found: Vec::new(),
            query: Query::default(),
            lookup_target: None,
            preview: None,
            selected: None,
            karaoke: karaoke::Karaoke::new(registry),
            wave: Default::default(),
            timing: None,
        }
    }
}

fn lookup_target_matches(
    target_generation: u64,
    target: &std::path::Path,
    generation: u64,
    current: Option<&std::path::Path>,
) -> bool {
    target_generation == generation && current == Some(target)
}

/// m:ss for a line's time.
fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0).floor() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// What the tab says about where the lyrics came from.
fn describe(l: &Lyrics) -> String {
    let timing = match l.timing() {
        Timing::Words => "Timed by word",
        Timing::Lines => "Timed by line",
        Timing::Plain => "Not timed",
    };
    let mut from = match &l.origin {
        Origin::Lrc(p) => p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "an .lrc file".into()),
        Origin::Tag => "the file's tags".into(),
        Origin::Saved(_) => "saved by openDJ".into(),
    };
    if let Origin::Lrc(path) | Origin::Saved(path) = &l.origin {
        use std::io::Read;
        let mut header = Vec::new();
        if std::fs::File::open(path)
            .and_then(|file| file.take(2048).read_to_end(&mut header))
            .is_ok()
        {
            let header = String::from_utf8_lossy(&header);
            if let Some(source) = header
                .lines()
                .take(12)
                .find_map(|line| {
                    line.strip_prefix("[source:")
                        .and_then(|s| s.strip_suffix(']'))
                })
                .filter(|source| matches!(*source, "Letras" | "LRCLIB"))
            {
                from = format!("{source} · {from}");
            }
        }
    }
    if l.auto {
        format!("Transcribed by openDJ · {from}")
    } else {
        format!("{timing} · {from}")
    }
}

/// Where openDJ keeps lyrics for tracks whose folder can't be written.
fn store() -> Option<std::path::PathBuf> {
    settings::config_dir().map(|d| d.join("lyrics"))
}

/// A row of the Find list.
fn match_row(f: &Found, q: &Query) -> LyricMatch {
    let mut detail = f.artist.clone();
    if !f.album.is_empty() {
        detail.push_str(" · ");
        detail.push_str(&f.album);
    }
    if f.duration > 0.0 {
        detail.push_str(" · ");
        detail.push_str(&clock(f.duration));
    }
    LyricMatch {
        title: f.title.clone().into(),
        detail: detail.into(),
        kind: if f.instrumental {
            2
        } else if f.is_synced() {
            0
        } else {
            1
        },
        fits: f.duration <= 0.0 || f.fits(q.duration),
        source: f.source.into(),
        quality: match_quality(f, q).into(),
    }
}

/// Keep every file that save() could replace. create_new protects earlier
/// backups, and failure aborts import before any lyrics are changed.
fn backup_lyrics(track: &std::path::Path, store: Option<&std::path::Path>) -> std::io::Result<()> {
    use std::io::Write;
    let mut paths = vec![track.with_extension("lrc")];
    if let Some(store) = store {
        paths.push(lyrics::stored_path(store, track));
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for path in paths {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let name = format!(
            "{}.opendj-backup-{stamp}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        let backup = path.with_file_name(name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(backup)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    Ok(())
}

fn match_quality(f: &Found, q: &Query) -> String {
    let normalized = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let names = normalized(&f.title) == normalized(&q.title)
        && (q.artist.trim().is_empty() || normalized(&f.artist) == normalized(&q.artist));
    let length = if f.duration <= 0.0 {
        "length unavailable"
    } else if f.fits(q.duration) {
        "length fits"
    } else {
        "other length"
    };
    format!(
        "{} · {length}",
        if names {
            "Artist/title match"
        } else {
            "Check artist/title"
        }
    )
}

fn lookup(q: &Query, source: i32) -> Looked {
    let mut found = Vec::new();
    let mut notes = Vec::new();
    std::thread::scope(|scope| {
        let lrclib = (source != 2).then(|| scope.spawn(|| lrclib::find(q)));
        let letras = (source != 1).then(|| scope.spawn(|| letras::find(q)));
        if let Some(job) = lrclib {
            match job.join().unwrap_or(Err(Miss::Offline)) {
                Ok(items) => found.extend(items),
                Err(Miss::None) => notes.push("LRCLIB has no matching lyrics."),
                Err(Miss::Offline) => notes.push("Couldn't reach LRCLIB."),
            }
        }
        if let Some(job) = letras {
            match job.join().unwrap_or(Err(letras::Error::Offline)) {
                Ok(items) if items.is_empty() => notes.push("Letras has no matching songs."),
                Ok(items) => found.extend(items),
                Err(e) => notes.push(e.message()),
            }
        }
    });
    Looked {
        found,
        note: notes.join(" "),
    }
}

impl Browser {
    pub(super) fn install_lyrics(
        app: &AppWindow,
        with: &(impl Fn(&dyn Fn(&mut Browser)) + Clone + 'static),
    ) {
        let ui = app.global::<LyricsUi>();
        let w = with.clone();
        ui.on_set_subject(move |s| {
            w(&|b| {
                b.lyrics.subject = s.clamp(PLAYING, FIRST_DECK + 1);
                if let Some(app) = b.app.upgrade() {
                    app.global::<LyricsUi>().set_subject(b.lyrics.subject);
                    b.poll_lyrics(&app, LYRICS_TAB);
                }
            })
        });
        let w = with.clone();
        ui.on_seek(move |line| w(&|b| b.seek_to_line(line)));
        let w = with.clone();
        ui.on_find(move || w(&|b| b.open_find_lyrics()));
        let w = with.clone();
        ui.on_search(move || w(&|b| b.find_lyrics()));
        let w = with.clone();
        ui.on_preview_found(move |i| w(&|b| b.preview_found(i)));
        let w = with.clone();
        ui.on_open_found(move || w(&|b| b.open_found()));
        let w = with.clone();
        ui.on_use_found(move |i| w(&|b| b.use_found(i)));
        let w = with.clone();
        ui.on_close_found(move || w(&|b| b.close_found()));
        let w = with.clone();
        ui.on_enable_lookup(move || {
            crate::settings::set(lrclib::ENABLED, serde_json::json!(true));
            w(&|b| b.find_lyrics())
        });
        let w = with.clone();
        ui.on_time_words(move || w(&|b| b.time_words()));
        Self::install_karaoke(app, with);
    }

    /// The deck `index` (in `library`) is loaded on, the first if several.
    fn deck_with(&self, index: usize) -> Option<usize> {
        let id = self.library[index].id;
        (0..NUM_DECKS).find(|&d| self.loaded[d] == Some(id))
    }

    /// The track the tab is to show, and the deck it is on.
    fn lyrics_subject(&mut self, app: &AppWindow) -> (Option<usize>, Option<usize>) {
        // Player mode's large lyrics and the karaoke screen follow the
        // track playing.
        let ui = app.global::<LyricsUi>();
        let subject = if ui.get_big() || ui.get_karaoke() {
            PLAYING
        } else {
            self.lyrics.subject
        };
        match subject {
            PLAYING => {
                let playing =
                    |d: usize| self.loaded[d].is_some() && self.lyrics.decks[d].play.get_bool();
                // Automix's live deck; else stay with the deck followed while
                // it plays; else the first deck playing; else the last one
                // followed, paused; else the first deck with a track.
                let live = usize::try_from(app.get_automix_live())
                    .ok()
                    .filter(|&d| d < NUM_DECKS && playing(d));
                let deck = live
                    .or(self.lyrics.playing.filter(|&d| playing(d)))
                    .or_else(|| (0..NUM_DECKS).find(|&d| playing(d)))
                    .or(self.lyrics.playing.filter(|&d| self.loaded[d].is_some()))
                    .or_else(|| (0..NUM_DECKS).find(|&d| self.loaded[d].is_some()));
                self.lyrics.playing = deck;
                (deck.and_then(|d| self.deck_track(d)), deck)
            }
            SELECTED => {
                let index = usize::try_from(app.get_current_track())
                    .ok()
                    .and_then(|r| self.rows.get(r).copied().flatten());
                (index, index.and_then(|i| self.deck_with(i)))
            }
            d => {
                let deck = (d - FIRST_DECK) as usize;
                (self.deck_track(deck), Some(deck))
            }
        }
    }

    /// Keeps the Lyrics tab up to date; called every frame.
    pub(super) fn poll_lyrics(&mut self, app: &AppWindow, tab: usize) {
        self.poll_timing(app);
        let ui = app.global::<LyricsUi>();
        if tab != LYRICS_TAB && !ui.get_big() && !ui.get_karaoke() {
            return;
        }
        let (index, deck) = self.lyrics_subject(app);
        let now = index.map(|i| (i, self.library[i].clone()));
        let other = match (&now, &self.lyrics.shown) {
            (Some((_, t)), Some((_, u))) => {
                t.id != u.id || super::file_path(t) != super::file_path(u)
            }
            (None, None) => false,
            _ => true,
        };
        if other {
            self.lyrics.shown = now.clone();
            self.lyrics.lyrics = None;
            self.lyrics.current = -1;
            self.lyrics.generation += 1;
            self.lyrics.job = None;
            self.close_found_ui(&ui);
            ui.set_time_note(SharedString::new());
            ui.set_can_time(false);
            ui.set_can_transcribe(false);
            ui.set_has_track(now.is_some());
            ui.set_can_find(
                now.as_ref()
                    .is_some_and(|(_, t)| matches!(t.source, Source::File(_))),
            );
            ui.set_title(
                now.as_ref()
                    .map(|(_, t)| {
                        if t.artist.is_empty() {
                            t.title.clone()
                        } else {
                            format!("{} · {}", t.artist, t.title)
                        }
                    })
                    .unwrap_or_default()
                    .into(),
            );
            ui.set_source(SharedString::new());
            ui.set_lines(ModelRc::default());
            ui.set_current(-1);
            if let Some((_, t)) = &now {
                if let Source::File(path) = &t.source {
                    let (tx, rx) = mpsc::channel();
                    let path = path.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(lyrics::find_with(&path, store().as_deref()));
                    });
                    self.lyrics.job = Some((self.lyrics.generation, rx));
                }
            }
        }
        // Library refreshes can move a track without changing its identity.
        self.lyrics.shown = now;
        if deck != self.lyrics.deck {
            self.lyrics.deck = deck;
        }
        if let Some((generation, rx)) = &self.lyrics.job {
            if let Ok(found) = rx.try_recv() {
                if *generation == self.lyrics.generation {
                    self.show_lyrics(&ui, found);
                }
                self.lyrics.job = None;
            }
        }
        // The line sung now, while the track is on a deck.
        let current = match (&self.lyrics.lyrics, deck) {
            (Some(l), Some(d)) => {
                let p = &self.lyrics.decks[d];
                l.line_at(p.playposition.get() * p.duration.get())
                    .map_or(-1, |i| i as i32)
            }
            _ => -1,
        };
        ui.set_can_seek(
            deck.is_some()
                && self
                    .lyrics
                    .lyrics
                    .as_ref()
                    .is_some_and(|l| l.timing() != Timing::Plain),
        );
        if current != self.lyrics.current {
            self.lyrics.current = current;
            ui.set_current(current);
        }
        // The current line, sung and to come, for the large view.
        let sung = match (&self.lyrics.lyrics, deck, usize::try_from(current)) {
            (Some(l), Some(d), Ok(i)) => {
                let p = &self.lyrics.decks[d];
                l.lines[i].sung_at(p.playposition.get() * p.duration.get())
            }
            _ => Default::default(),
        };
        if sung != self.lyrics.sung {
            ui.set_sung(sung.0.as_str().into());
            ui.set_to_come(sung.1.as_str().into());
            self.lyrics.sung = sung;
        }
        self.poll_karaoke(app);
        self.poll_find(&ui);
        self.poll_lyric_preview(&ui);
    }

    fn open_find_lyrics(&mut self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        self.poll_lyrics(&app, LYRICS_TAB);
        let Some((_, track)) = &self.lyrics.shown else {
            return;
        };
        let (artist, title) = super::file_path(track)
            .map(|path| {
                opendj_engine::filename::lookup_with_remix(
                    path,
                    &track.artist,
                    &track.title,
                    &track.info.remix,
                )
            })
            .unwrap_or_else(|| (track.artist.clone(), track.title.clone()));
        let ui = app.global::<LyricsUi>();
        ui.set_query_artist(artist.into());
        ui.set_query_title(title.into());
        self.find_lyrics();
    }

    /// Search changes only the result list. Existing lyrics keep playing.
    fn find_lyrics(&mut self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        self.poll_lyrics(&app, LYRICS_TAB);
        let ui = app.global::<LyricsUi>();
        self.close_found_ui(&ui);
        ui.set_finding(true);
        ui.set_lookup_off(!lrclib::enabled());
        if !lrclib::enabled() {
            ui.set_find_note(
                "Finding lyrics online is off. LRCLIB receives artist/title/album/length. \
                 Letras receives the song title. Selecting a Letras result requests its page. \
                 Enable this here or in Settings › Collection › Health."
                    .into(),
            );
            return;
        }
        let Some((_, t)) = &self.lyrics.shown else {
            ui.set_finding(false);
            return;
        };
        let q = Query {
            title: ui.get_query_title().trim().to_string(),
            artist: ui.get_query_artist().trim().to_string(),
            album: t.album.clone(),
            duration: t.duration,
        };
        if q.title.is_empty() {
            ui.set_find_note("Enter a song title to search.".into());
            return;
        }
        let Some(path) = super::file_path(t) else {
            ui.set_find_note("Select a local audio file to import lyrics.".into());
            return;
        };
        self.lyrics.lookup_target = Some((self.lyrics.generation, path.to_path_buf()));
        ui.set_find_track(ui.get_title());
        self.lyrics.query = q.clone();
        let source = ui.get_find_source().clamp(0, 2);
        ui.set_find_note("Searching…".into());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(lookup(&q, source));
        });
        self.lyrics.finding = Some((self.lyrics.generation, rx));
    }

    fn poll_find(&mut self, ui: &LyricsUi) {
        let Some((generation, rx)) = &self.lyrics.finding else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        let current = *generation == self.lyrics.generation;
        self.lyrics.finding = None;
        if !current {
            return;
        }
        let rows: Vec<LyricMatch> = result
            .found
            .iter()
            .map(|f| match_row(f, &self.lyrics.query))
            .collect();
        ui.set_find_note(result.note.into());
        ui.set_found(ModelRc::new(VecModel::from(rows)));
        self.lyrics.found = result.found;
    }

    fn preview_found(&mut self, row: i32) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let ui = app.global::<LyricsUi>();
        let Some((index, f)) = usize::try_from(row)
            .ok()
            .and_then(|i| self.lyrics.found.get(i).map(|f| (i, f)))
        else {
            return;
        };
        self.lyrics.preview = None;
        self.lyrics.selected = Some(index);
        ui.set_selected(row);
        ui.set_preview(f.text().into());
        ui.set_preview_url(f.url.as_str().into());
        ui.set_preview_title(format!("{} · {} · {}", f.artist, f.title, f.source).into());
        ui.set_preview_note(
            if f.instrumental {
                "Instrumental — no lyrics to import."
            } else if f.source == "Letras" {
                "Select the original lyrics below, or open the site and paste its lyrics here."
            } else if f.is_synced() {
                "Synced lyrics. Review timing and track version before importing."
            } else {
                "Plain text. Timing is unavailable."
            }
            .into(),
        );
        if f.source == "Letras" && f.plain.is_empty() {
            if !lrclib::enabled() {
                ui.set_preview_note(
                    "Online lookup is off. Enable it to load the song page.".into(),
                );
                return;
            }
            ui.set_preview_note("Loading original lyrics from Letras…".into());
            let url = f.url.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(letras::lyrics(&url));
            });
            self.lyrics.preview = Some((index, rx));
        }
    }

    fn poll_lyric_preview(&mut self, ui: &LyricsUi) {
        let Some((index, rx)) = &self.lyrics.preview else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        let index = *index;
        self.lyrics.preview = None;
        if self.lyrics.selected != Some(index) {
            return;
        }
        match result {
            Ok(text) => {
                if let Some(f) = self.lyrics.found.get_mut(index) {
                    f.plain = text.clone();
                }
                // A user may already have pasted text while the request ran.
                if ui.get_preview().trim().is_empty() {
                    ui.set_preview(text.into());
                }
                ui.set_preview_note(
                    "Original plain text from Letras. Timing is unavailable.".into(),
                );
            }
            Err(e) => ui.set_preview_note(
                format!(
                    "{} Click Open on site, copy the original lyrics, paste them in the editor, then click Import selection.",
                    e.message()
                )
                .into(),
            ),
        }
    }

    fn open_found(&mut self) {
        let Some(index) = self.lyrics.selected else {
            return;
        };
        let Some(f) = self.lyrics.found.get(index) else {
            return;
        };
        let safe = if f.source == "Letras" {
            letras::valid_url(&f.url)
        } else {
            f.url == "https://lrclib.net/"
                || f.url
                    .strip_prefix("https://lrclib.net/api/get/")
                    .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        };
        if safe {
            crate::spotify::open_in_browser(&f.url);
        }
    }

    /// Saves the picked lyrics beside the track and shows them.
    fn use_found(&mut self, i: i32) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let ui = app.global::<LyricsUi>();
        let Some(f) = usize::try_from(i)
            .ok()
            .and_then(|i| self.lyrics.found.get(i))
            .cloned()
        else {
            return;
        };
        // A click can arrive before the next frame observes a new selection.
        // Compare the live subject and the captured path before any file write.
        let (current, _) = self.lyrics_subject(&app);
        let current_path = current
            .and_then(|i| self.library.get(i))
            .and_then(|t| super::file_path(t));
        let Some((generation, path)) = self.lyrics.lookup_target.as_ref() else {
            ui.set_preview_note("Select the track and search again before importing.".into());
            return;
        };
        if !lookup_target_matches(*generation, path, self.lyrics.generation, current_path) {
            ui.set_preview_note(
                "The track changed. Select the intended track and search again before importing."
                    .into(),
            );
            return;
        }
        let path = path.clone();
        if self.lyrics.selected != usize::try_from(i).ok() {
            ui.set_preview_note("Preview the result you want to import first.".into());
            return;
        }
        if self.lyrics.timing.is_some() {
            ui.set_preview_note("Wait for word timing to finish before importing lyrics.".into());
            return;
        }
        let text = ui.get_preview().to_string();
        if f.instrumental {
            ui.set_preview_note("This result is marked instrumental.".into());
            return;
        }
        if lyrics::parse(&text).is_none() {
            ui.set_preview_note(
                "Paste the original lyrics in the editor, then click Import selection.".into(),
            );
            return;
        }
        let store = store();
        if let Err(e) = backup_lyrics(&path, store.as_deref()) {
            ui.set_preview_note(format!("Couldn't back up existing lyrics: {e}").into());
            return;
        }
        let clean_tag = |value: &str| value.replace(['\r', '\n', '[', ']'], " ");
        let imported = format!(
            "[re:openDJ import]\n[source:{}]\n[url:{}]\n[ti:{}]\n[ar:{}]\n{}",
            f.source,
            f.url,
            clean_tag(&f.title),
            clean_tag(&f.artist),
            text.trim()
        );
        match lyrics::save(&path, store.as_deref(), &imported) {
            Ok(_) => {
                self.close_found_ui(&ui);
                self.lyrics.wave.reload();
                // Read again, from the file just written.
                self.lyrics.shown = None;
                self.poll_lyrics(&app, LYRICS_TAB);
            }
            Err(e) => ui.set_preview_note(format!("Couldn't save the lyrics: {e}").into()),
        }
    }

    fn close_found(&mut self) {
        if let Some(app) = self.app.upgrade() {
            self.close_found_ui(&app.global::<LyricsUi>());
        }
    }

    fn close_found_ui(&mut self, ui: &LyricsUi) {
        self.lyrics.finding = None;
        self.lyrics.found.clear();
        self.lyrics.preview = None;
        self.lyrics.selected = None;
        self.lyrics.lookup_target = None;
        ui.set_find_track(SharedString::new());
        ui.set_selected(-1);
        ui.set_preview(SharedString::new());
        ui.set_preview_url(SharedString::new());
        ui.set_preview_title(SharedString::new());
        ui.set_preview_note(SharedString::new());
        ui.set_finding(false);
        ui.set_lookup_off(false);
        ui.set_find_note(SharedString::new());
        ui.set_found(ModelRc::default());
    }

    fn show_lyrics(&mut self, ui: &LyricsUi, found: Option<Lyrics>) {
        match &found {
            Some(l) => {
                let lines: Vec<LyricLine> = l
                    .lines
                    .iter()
                    .map(|line| LyricLine {
                        time: line.start.map(clock).unwrap_or_default().into(),
                        text: line.text.clone().into(),
                    })
                    .collect();
                ui.set_source(describe(l).into());
                ui.set_lines(ModelRc::new(VecModel::from(lines)));
            }
            None => {
                ui.set_source(SharedString::new());
                ui.set_lines(ModelRc::default());
            }
        }
        self.lyrics.current = -1;
        ui.set_current(-1);
        self.lyrics.lyrics = found;
        self.show_timing(ui);
    }

    /// Plays the deck the track is on from the start of `line`.
    fn seek_to_line(&mut self, line: i32) {
        let (Some(l), Some(d)) = (&self.lyrics.lyrics, self.lyrics.deck) else {
            return;
        };
        let Some(start) = usize::try_from(line)
            .ok()
            .and_then(|i| l.lines.get(i))
            .and_then(|l| l.start)
        else {
            return;
        };
        let p = &self.lyrics.decks[d];
        let duration = p.duration.get();
        if duration > 0.0 {
            p.playposition.set((start / duration).clamp(0.0, 1.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn import_requires_the_search_track_even_before_the_next_frame() {
        let a = std::path::Path::new("/music/a.mp3");
        let b = std::path::Path::new("/music/b.mp3");
        assert!(lookup_target_matches(3, a, 3, Some(a)));
        assert!(!lookup_target_matches(3, a, 3, Some(b)));
        assert!(!lookup_target_matches(3, a, 4, Some(a)));
        assert!(!lookup_target_matches(3, a, 3, None));
    }

    #[test]
    fn clock_reads_as_minutes() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(61.9), "1:01");
        assert_eq!(clock(600.0), "10:00");
    }

    #[test]
    fn backs_up_exact_enhanced_lrc_and_saved_copy_before_import() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("opendj-lyric-backup-{stamp}"));
        let saved = root.join("saved");
        std::fs::create_dir_all(&saved).unwrap();
        let track = root.join("Song.flac");
        let sidecar = track.with_extension("lrc");
        let stored = lyrics::stored_path(&saved, &track);
        let original = b"\xef\xbb\xbf[00:01.00]<00:01.00>One <00:01.50>word\r\n";
        std::fs::write(&sidecar, original).unwrap();
        std::fs::write(&stored, b"[00:02.00]Previous stored copy").unwrap();
        backup_lyrics(&track, Some(&saved)).unwrap();
        lyrics::save(&track, Some(&saved), "[source:Letras]\nPlain imported text").unwrap();
        let backups: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".opendj-backup-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), original);
        assert_eq!(std::fs::read_dir(&saved).unwrap().count(), 2);
        let imported = lyrics::find_with(&track, Some(&saved)).unwrap();
        assert_eq!(imported.timing(), Timing::Plain);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn describes_where_from() {
        let lines = lyrics::parse("[00:01.00]<00:01.00>Hi <00:01.50>there").unwrap();
        let l = Lyrics {
            lines,
            origin: Origin::Lrc(PathBuf::from("/music/Song.lrc")),
            auto: false,
        };
        assert_eq!(describe(&l), "Timed by word · Song.lrc");
        let l = Lyrics {
            lines: lyrics::parse("Just words").unwrap(),
            origin: Origin::Tag,
            auto: false,
        };
        assert_eq!(describe(&l), "Not timed · the file's tags");
        let l = Lyrics {
            lines: lyrics::parse("[00:01.00]<00:01.00>Hi <00:01.50>there").unwrap(),
            origin: Origin::Lrc(PathBuf::from("/music/Song.lrc")),
            auto: true,
        };
        assert_eq!(describe(&l), "Transcribed by openDJ · Song.lrc");
    }
}
