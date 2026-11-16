//! Native MK2 library state. Construct and call this only on the surface worker.
//! The explicit JSON catalogs are read once. No filesystem indexing is performed.
//! Products select packs, attributes select role/category/format, and result focus
//! stays separate from LOAD. Prehear uses the cue preview queue without edits.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use opendj_controllers::maschine_surface as surface;
use opendj_engine::maschine::{sample_edit, MaschineHandle, Sample, Target, ZoneFrom, ZoneSpec};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    pack: String,
    role: String,
    category: String,
    format: String,
    path: PathBuf,
    sha256: String,
    looped: bool,
    user: bool,
    instrument: Option<&'static str>,
    // Alternate source paths for byte-identical audio from other pack entries.
    sources: Vec<(String, PathBuf)>,
}

impl Entry {
    fn key(&self) -> String {
        self.instrument.map_or_else(
            || format!("sha256:{}", self.sha256),
            |s| format!("native:{s}"),
        )
    }
    fn attribute(&self, index: usize) -> &str {
        match index {
            0 => &self.role,
            1 => &self.category,
            _ => &self.format,
        }
    }
    fn in_pack(&self, pack: &str) -> bool {
        self.pack == pack || self.sources.iter().any(|(name, _)| name == pack)
    }
    fn kind(&self, kind: usize, user: bool) -> bool {
        if self.user != user {
            return false;
        }
        match kind {
            2 => self.instrument.is_none(),
            3 => self.instrument.is_some(),
            5 => self.instrument.is_none() && self.looped,
            6 => self.instrument.is_none() && !self.looped,
            _ => false,
        }
    }
}

struct Catalog {
    entries: Vec<Entry>,
    error: Option<String>,
}

#[derive(Default)]
struct Favorites {
    paths: BTreeSet<String>,
    path: Option<PathBuf>,
    error: Option<String>,
    revision: u64,
}

static CATALOG: OnceLock<Arc<Catalog>> = OnceLock::new();
static FAVORITES: OnceLock<Arc<Mutex<Favorites>>> = OnceLock::new();

pub struct MaschineBrowser {
    catalog: Arc<Catalog>,
    favorites: Arc<Mutex<Favorites>>,
    kind: usize,
    user: bool,
    product: Option<String>,
    attributes: [Option<String>; 3],
    focus: usize,
    only_favorites: bool,
    routing: bool,
    patterns: bool,
    cached: RefCell<Option<(u64, surface::BrowserView)>>,
}

impl MaschineBrowser {
    /// Called by Deck::new on the surface worker. Metadata and favorites are
    /// shared across decks, while each deck keeps its own filters and focus.
    pub fn new() -> Self {
        let catalog = CATALOG.get_or_init(|| Arc::new(load_catalog())).clone();
        let favorites = FAVORITES
            .get_or_init(|| Arc::new(Mutex::new(load_favorites())))
            .clone();
        Self {
            catalog,
            favorites,
            kind: 6,
            user: true,
            product: None,
            attributes: Default::default(),
            focus: 0,
            only_favorites: false,
            routing: true,
            patterns: true,
            cached: RefCell::new(None),
        }
    }

    fn base(&self) -> impl Iterator<Item = &Entry> {
        self.catalog
            .entries
            .iter()
            .filter(|entry| entry.kind(self.kind, self.user))
    }

    fn products(&self) -> Vec<String> {
        self.base()
            .flat_map(|e| {
                std::iter::once(e.pack.clone()).chain(e.sources.iter().map(|(p, _)| p.clone()))
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn attribute_values(&self, index: usize) -> Vec<String> {
        self.base()
            .filter(|e| self.product.as_ref().is_none_or(|p| e.in_pack(p)))
            .map(|e| e.attribute(index).to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn matches(&self, favorites: &BTreeSet<String>) -> Vec<&Entry> {
        self.base()
            .filter(|e| {
                self.product.as_ref().is_none_or(|p| e.in_pack(p))
                    && self
                        .attributes
                        .iter()
                        .enumerate()
                        .all(|(i, value)| value.as_ref().is_none_or(|v| e.attribute(i) == v))
                    && (!self.only_favorites || favorites.contains(&e.key()))
            })
            .collect()
    }

    pub fn view(&self) -> surface::BrowserView {
        let favorites = self.favorites.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((revision, view)) = self.cached.borrow().as_ref() {
            if *revision == favorites.revision {
                return view.clone();
            }
        }
        let products = self.products();
        let product = self
            .product
            .as_ref()
            .and_then(|p| products.iter().position(|v| v == p));
        let attributes = std::array::from_fn(|i| {
            let values = self.attribute_values(i);
            let chosen = self.attributes[i]
                .as_ref()
                .and_then(|v| values.iter().position(|s| s == v));
            (values, chosen)
        });
        let results: Vec<_> = self
            .matches(&favorites.paths)
            .into_iter()
            .map(|e| {
                (
                    if e.sources.is_empty() {
                        e.name.clone()
                    } else {
                        format!("{} · {} sources", e.name, e.sources.len() + 1)
                    },
                    favorites.paths.contains(&e.key()),
                )
            })
            .collect();
        let view = surface::BrowserView {
            kind: self.kind,
            user: self.user,
            products,
            product,
            attributes,
            focus: self.focus.min(results.len().saturating_sub(1)),
            results,
            busy: false,
            favorites: self.only_favorites,
            routing: self.routing,
            patterns: self.patterns,
        };
        *self.cached.borrow_mut() = Some((favorites.revision, view.clone()));
        view
    }

    /// Dispatch after the hardware action queue reaches the surface worker.
    /// Sample decoding, hashing, cache writes and document edits happen here.
    pub fn action(
        &mut self,
        action: surface::Browse,
        handle: &MaschineHandle,
        group: usize,
        sound: usize,
    ) -> Result<(), String> {
        use surface::Browse;
        self.cached.get_mut().take();
        match action {
            Browse::Kind(kind) => {
                self.kind = kind;
                if kind == 3 {
                    self.user = false;
                }
                self.reset();
            }
            Browse::User(user) => {
                self.user = user;
                self.reset();
            }
            Browse::Product(index) => {
                self.product = index
                    .map(|i| self.products().get(i).cloned().ok_or("Pack unavailable"))
                    .transpose()?;
                self.attributes = Default::default();
                self.focus = 0;
            }
            Browse::Attribute(index, selected) => {
                if index >= 3 {
                    return Err("Unknown browser attribute".into());
                }
                self.attributes[index] = selected
                    .map(|i| {
                        self.attribute_values(index)
                            .get(i)
                            .cloned()
                            .ok_or("Filter unavailable")
                    })
                    .transpose()?;
                self.focus = 0;
            }
            Browse::Focus(by) => {
                let favorites = self.favorites.lock().map_err(|_| "Favorites unavailable")?;
                let length = self.matches(&favorites.paths).len();
                self.focus = (self.focus as i128 + i128::from(by))
                    .clamp(0, length.saturating_sub(1) as i128)
                    as usize;
            }
            Browse::Favorites => {
                self.only_favorites = !self.only_favorites;
                self.focus = 0;
            }
            Browse::Routing(on) => self.routing = on,
            Browse::Patterns(on) => self.patterns = on,
            Browse::SetFavorite => {
                let mut favorites = self.favorites.lock().map_err(|_| "Favorites unavailable")?;
                if let Some(error) = &favorites.error {
                    return Err(error.clone());
                }
                let matches = self.matches(&favorites.paths);
                let entry = matches
                    .get(self.focus.min(matches.len().saturating_sub(1)))
                    .copied()
                    .ok_or("No sample selected")?;
                let mut next = favorites.paths.clone();
                let key = entry.key();
                if !next.remove(&key) {
                    next.insert(key);
                }
                save_favorites(
                    favorites.path.as_deref().ok_or("No favorites directory")?,
                    &next,
                )?;
                favorites.paths = next;
                favorites.revision = favorites.revision.wrapping_add(1);
                self.focus = self
                    .focus
                    .min(self.matches(&favorites.paths).len().saturating_sub(1));
            }
            Browse::Load | Browse::Prehear => {
                let entry = {
                    let favorites = self.favorites.lock().map_err(|_| "Favorites unavailable")?;
                    let matches = self.matches(&favorites.paths);
                    matches
                        .get(self.focus.min(matches.len().saturating_sub(1)))
                        .copied()
                        .cloned()
                        .ok_or_else(|| {
                            self.catalog
                                .error
                                .clone()
                                .unwrap_or_else(|| "No matching result selected".into())
                        })?
                };
                if let Some(class) = entry.instrument {
                    if action == Browse::Prehear {
                        return Err("Load the instrument and play a pad to audition it".into());
                    }
                    return handle.edit(move |doc| {
                        doc.transaction("Load browser instrument", |doc| {
                            crate::maschine_ui::load_native(doc, group, sound, class)
                        })
                    });
                }
                let (path, frames) = prepare_sample(&entry)?;
                if action == Browse::Prehear {
                    return handle.preview_file(&path);
                }
                handle.stop_preview()?;
                // Document ranges are inclusive, the decoded frame count is not.
                let last = frames.saturating_sub(1) as f64;
                handle.edit(move |doc| {
                    doc.transaction("Load browser sample", |doc| {
                        crate::maschine_ui::load_native(doc, group, sound, "Sampler")?;
                        doc.set_zones(
                            group,
                            sound,
                            &[ZoneSpec {
                                from: ZoneFrom::Template,
                                file: Some(path),
                                set: vec![
                                    ("play_range/low", 0.0),
                                    ("play_range/high", last),
                                    ("loop_range/low", 0.0),
                                    ("loop_range/high", last),
                                ],
                            }],
                            "Load browser sample",
                        )?;
                        doc.rename(Target::Sound(group, sound), &entry.name)
                    })
                })?;
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.product = None;
        self.attributes = Default::default();
        self.focus = 0;
    }
}

fn data_root() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
        .map(|p| p.join("opendj/maschine"))
}

fn load_favorites() -> Favorites {
    let path = data_root().map(|p| p.join("browser-favorites.json"));
    let mut favorites = Favorites {
        path,
        ..Default::default()
    };
    if let Some(path) = &favorites.path {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<String>>(&bytes) {
                Ok(paths) => favorites.paths = paths.into_iter().collect(),
                Err(e) => {
                    favorites.error =
                        Some(format!("Invalid favorites file {}: {e}", path.display()))
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => favorites.error = Some(format!("Couldn't read favorites: {e}")),
        }
    }
    favorites
}

fn save_favorites(path: &Path, favorites: &BTreeSet<String>) -> Result<(), String> {
    let parent = path.parent().ok_or("No favorites directory")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = path.with_extension(format!("json.part-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        serde_json::to_writer_pretty(&mut file, favorites).map_err(|e| e.to_string())?;
        file.write_all(b"\n")
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, path).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn load_catalog() -> Catalog {
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    for variable in [
        "OPENDJ_MASCHINE_SAMPLE_CATALOG",
        "OPENDJ_MASCHINE_FACTORY_KITS",
    ] {
        let Some(path) = std::env::var_os(variable).map(PathBuf::from) else {
            continue;
        };
        let result = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
            .and_then(|value| catalog_records(&value));
        match result {
            Ok(items) => entries.extend(items),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    if entries.is_empty() {
        errors.push("Set OPENDJ_MASCHINE_SAMPLE_CATALOG to an audio inventory JSON file".into());
    }
    for (name, class) in crate::maschine_ui::NATIVE_INSTRUMENTS {
        entries.push(Entry {
            name: (*name).into(),
            pack: "openDJ native instruments".into(),
            role: if name.contains("Synth") {
                "Synth"
            } else {
                "Instrument"
            }
            .into(),
            category: (*class).into(),
            format: "Native".into(),
            path: PathBuf::new(),
            sha256: String::new(),
            looped: false,
            user: false,
            instrument: Some(class),
            sources: Vec::new(),
        });
    }
    let entries = unique_entries(entries);
    Catalog {
        entries,
        error: (!errors.is_empty()).then(|| errors.join(" · ")),
    }
}

fn unique_entries(entries: Vec<Entry>) -> Vec<Entry> {
    let mut seen: HashMap<(bool, String), usize> = HashMap::new();
    let mut unique: Vec<Entry> = Vec::new();
    for entry in entries {
        let identity = (
            entry.user,
            entry
                .instrument
                .map_or_else(|| entry.sha256.clone(), |s| format!("native:{s}")),
        );
        if let Some(&index) = seen.get(&identity) {
            if unique[index].path != entry.path {
                unique[index].sources.push((entry.pack, entry.path));
            }
        } else {
            seen.insert(identity, unique.len());
            unique.push(entry);
        }
    }
    let mut entries = unique;
    entries.sort_by_key(|e| (e.pack.to_lowercase(), e.name.to_lowercase()));
    entries
}

fn catalog_records(value: &Value) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    if let Some(rows) = value.as_array() {
        for row in rows {
            if row["probe_error"].as_str().is_some_and(|s| !s.is_empty()) {
                continue;
            }
            if row["extension"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(".wma"))
            {
                continue;
            }
            entries.push(sample_record(row, None)?);
        }
    } else if let Some(kits) = value["kits"].as_array() {
        for kit in kits {
            let name = format!(
                "Curated {} · {}",
                kit["bank"].as_str().unwrap_or(""),
                kit["style"].as_str().unwrap_or("Kit")
            );
            for row in kit["slots"].as_array().ok_or("Kit is missing slots")? {
                entries.push(sample_record(row, Some(&name))?);
            }
        }
    } else {
        return Err("Expected an audio inventory array or a curated kits manifest".into());
    }
    Ok(entries)
}

fn sample_record(row: &Value, curated: Option<&str>) -> Result<Entry, String> {
    let get = |key: &str| row[key].as_str().unwrap_or_default();
    let path = PathBuf::from(if curated.is_some() {
        get("selected_path")
    } else {
        get("path")
    });
    if !path.is_absolute() {
        return Err("Catalog sample paths must be absolute".into());
    }
    let sha256 = get("sha256").to_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("Invalid SHA-256 for {}", path.display()));
    }
    let name = if curated.is_some() {
        get("name")
    } else {
        get("filename")
    };
    let name = if name.is_empty() {
        path.file_name()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("Sample")
    } else {
        name
    };
    let category = if curated.is_some() {
        get("source_category")
    } else {
        get("category")
    };
    let format = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_uppercase();
    let labels = format!("{name} {category}").to_lowercase();
    let role = if get("role").is_empty() {
        infer_role(&labels)
    } else {
        get("role")
    };
    Ok(Entry {
        name: name.into(),
        pack: curated.unwrap_or_else(|| get("pack_id")).into(),
        role: role.into(),
        category: category.into(),
        format,
        path,
        sha256,
        looped: labels.contains("loop") || labels.contains("batida"),
        user: curated.is_none(),
        instrument: None,
        sources: Vec::new(),
    })
}

// These are source-label filters, not audio analysis or style classifications.
fn infer_role(label: &str) -> &'static str {
    for (role, words) in [
        ("kick", &["kick", "bumbo"][..]),
        ("snare", &["snare", "caixa"]),
        ("hats", &["hat", "chimbal"]),
        ("clap", &["clap", "palma"]),
        ("toms", &["tom", "atabaque"]),
        ("graves", &["bass", "808", "grave"]),
        ("fx", &["fx", "effect", "efeito"]),
        ("vocals", &["voz", "vocal", "acapela"]),
        ("percs", &["perc", "pandeiro", "cuica", "cuíca", "conga"]),
        ("synth", &["synth", "melod", "piano"]),
    ] {
        if words.iter().any(|word| label.contains(word)) {
            return role;
        }
    }
    "other"
}

fn prepare_sample(entry: &Entry) -> Result<(PathBuf, usize), String> {
    let source = std::iter::once(&entry.path)
        .chain(entry.sources.iter().map(|(_, path)| path))
        .find(|path| path.is_file())
        .unwrap_or(&entry.path);
    let mut file = std::fs::File::open(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let sha256: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if sha256 != entry.sha256 {
        return Err("Sample changed since catalog verification (SHA-256 mismatch)".into());
    }
    let extension = source
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(extension.as_str(), "wav" | "aif" | "aiff" | "aifc" | "ogg") {
        let sample = Sample::load(source)?;
        if sample.frames.is_empty() {
            return Err("Sample contains no audio frames".into());
        }
        return Ok((source.clone(), sample.frames.len()));
    }
    if !opendj_engine::decode::EXTENSIONS.contains(&extension.as_str()) {
        return Err(format!("Unsupported sample format: {extension}"));
    }
    let cache = data_root()
        .ok_or("No sample cache directory")?
        .join("browser-cache-v1")
        .join(format!("{}.wav", entry.sha256));
    if cache.exists() {
        let sample = Sample::load(&cache)?;
        if sample.frames.is_empty() {
            return Err("Cached sample contains no audio frames".into());
        }
        return Ok((cache, sample.frames.len()));
    }
    let audio = opendj_engine::decode::decode(source)?;
    let sample = Sample {
        rate: audio.sample_rate,
        frames: audio.decoded_chunks().flatten().copied().collect(),
    };
    if sample.frames.is_empty() {
        return Err("Sample contains no audio frames".into());
    }
    let temporary = cache.with_extension("wav.part");
    sample_edit::write_wav(&temporary, &sample)?;
    std::fs::rename(&temporary, &cache).map_err(|e| e.to_string())?;
    Ok((cache, sample.frames.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn browser() -> MaschineBrowser {
        let value = json!([
            {"filename":"Kick.wav","pack_id":"Pack A","category":"Kicks","path":"/samples/kick.wav","sha256":"a".repeat(64)},
            {"filename":"Hat.wav","pack_id":"Pack A","category":"Hats","path":"/samples/hat.wav","sha256":"b".repeat(64)},
            {"filename":"Loop.wav","pack_id":"Pack B","category":"Loops","path":"/samples/loop.wav","sha256":"c".repeat(64)}
        ]);
        MaschineBrowser {
            catalog: Arc::new(Catalog {
                entries: catalog_records(&value).unwrap(),
                error: None,
            }),
            favorites: Arc::new(Mutex::new(Favorites::default())),
            kind: 6,
            user: true,
            product: None,
            attributes: Default::default(),
            focus: 0,
            only_favorites: false,
            routing: true,
            patterns: true,
            cached: RefCell::new(None),
        }
    }

    #[test]
    fn catalog_and_hardware_view_have_actual_filterable_samples() {
        let mut browser = browser();
        let view = browser.view();
        assert_eq!(view.results.len(), 2);
        assert_eq!(view.products, vec!["Pack A"]);
        assert!(view.attributes[0].0.contains(&"kick".into()));
        browser.attributes[0] = Some("hats".into());
        browser.cached.get_mut().take();
        assert_eq!(browser.view().results, vec![("Hat.wav".into(), false)]);
        browser.attributes[0] = None;
        browser.kind = 5;
        browser.cached.get_mut().take();
        assert_eq!(browser.view().results, vec![("Loop.wav".into(), false)]);
    }

    #[test]
    fn curated_catalog_and_invalid_paths_are_explicit() {
        let value = json!({"kits":[{"bank":"A","style":"Tamborzão","slots":[{"name":"Kick","selected_path":"/kits/kick.wav","role":"kick","sha256":"a".repeat(64)}]}]});
        let entries = catalog_records(&value).unwrap();
        assert_eq!(entries[0].pack, "Curated A · Tamborzão");
        assert!(!entries[0].user);
        assert!(catalog_records(&json!([{"path":"relative.wav"}])).is_err());
        assert!(catalog_records(&json!({})).is_err());
    }

    #[test]
    fn duplicate_audio_keeps_all_pack_sources_and_bad_probes_are_excluded() {
        let rows = json!([
            {"filename":"Kick.wav","pack_id":"A","path":"/A/kick.wav","sha256":"a".repeat(64)},
            {"filename":"Same.wav","pack_id":"B","path":"/B/same.wav","sha256":"a".repeat(64)},
            {"path":"/failed.wav","probe_error":"broken header"},
            {"path":"/unsupported.wma","extension":".wma"}
        ]);
        let entries = unique_entries(catalog_records(&rows).unwrap());
        assert_eq!(entries.len(), 1);
        assert!(entries[0].in_pack("A"));
        assert!(entries[0].in_pack("B"));
        assert_eq!(entries[0].sources[0].1, PathBuf::from("/B/same.wav"));
        let mut browser = browser();
        browser.catalog = Arc::new(Catalog {
            entries,
            error: None,
        });
        assert_eq!(browser.view().products, vec!["A", "B"]);
        assert!(browser.view().results[0].0.contains("2 sources"));
    }

    #[test]
    fn missing_sample_reports_error_without_a_document_edit() {
        let browser = browser();
        assert!(prepare_sample(&browser.catalog.entries[0])
            .unwrap_err()
            .contains("kick.wav"));
    }

    #[test]
    fn focus_and_shared_favorite_revision_update_the_hardware_view() {
        let mut browser = browser();
        browser.focus = usize::MAX;
        assert_eq!(browser.view().focus, 1);
        {
            let mut favorites = browser.favorites.lock().unwrap();
            favorites.paths.insert(browser.catalog.entries[0].key());
            favorites.revision += 1;
        }
        assert!(browser.view().results[0].1);
        browser.only_favorites = true;
        browser.cached.get_mut().take();
        assert_eq!(browser.view().results.len(), 1);
        assert_eq!(browser.view().focus, 0);
    }

    #[test]
    fn favorites_are_atomic_and_changed_samples_are_rejected() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("opendj-browser-test-{stamp}"));
        let path = directory.join("favorites.json");
        let favorites = BTreeSet::from(["native:Kick".into(), "/samples/kick.wav".into()]);
        save_favorites(&path, &favorites).unwrap();
        let saved: BTreeSet<String> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved, favorites);
        let mut entry = browser().catalog.entries[0].clone();
        entry.path = directory.join("changed.wav");
        std::fs::write(&entry.path, b"different bytes").unwrap();
        assert!(prepare_sample(&entry)
            .unwrap_err()
            .contains("SHA-256 mismatch"));
        entry.sha256 = "538a741c622733afe147819e939d82cc071edfbeb3eb49ed701a55975ac288b3".into();
        let unsupported = entry.path.with_extension("unsupported");
        std::fs::rename(&entry.path, &unsupported).unwrap();
        entry.path = unsupported;
        assert!(prepare_sample(&entry)
            .unwrap_err()
            .contains("Unsupported sample format"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
