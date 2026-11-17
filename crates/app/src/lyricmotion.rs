//! Isolated visual bridge. Only background workers read lyric files or use HTTP.
//! The UI reads a latest-only snapshot and submits bounded visual commands.
use crate::{AppWindow, MotionUi};
use opendj_control::ControlRegistry;
use opendj_engine::{lyrics, LoadedTracks, Source, Track, VisualPositions, NUM_DECKS};
use serde_json::{json, Value};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    mpsc::{self, SyncSender},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const BASE: &str = "http://127.0.0.1:9055";
const PERIOD: Duration = Duration::from_millis(50);

#[derive(Default, Clone)]
struct View {
    status: Value,
    universes: Vec<Value>,
    scenes: Vec<Value>,
    targets: Vec<Value>,
    assets: Vec<Value>,
    history: Vec<String>,
    note: String,
    online: bool,
    catalog_labels: [String; 2],
    catalog_pages: [usize; 2],
}
#[derive(Default)]
struct Shared {
    enabled: AtomicBool,
    starting: AtomicBool,
    launch_note: Mutex<String>,
    catalog: Mutex<[(String, usize); 2]>,
    input_online: AtomicBool,
    source: AtomicUsize,
    revision: AtomicU64,
    alive: AtomicBool,
    frame: Mutex<Option<Value>>,
    view: Mutex<View>,
}
struct Lifetime(Arc<Shared>);
impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::Relaxed);
    }
}

fn strings(values: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        values
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
}
fn label(row: &Value) -> String {
    row["name"]
        .as_str()
        .or_else(|| row["label"].as_str())
        .or_else(|| row["id"].as_str())
        .unwrap_or("—")
        .into()
}
fn rows(value: &Value, key: &str) -> Vec<Value> {
    value[key].as_array().cloned().unwrap_or_default()
}
fn text(value: &Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| {
        if value.is_null() {
            String::new()
        } else {
            value.to_string()
        }
    })
}
fn token() -> Option<String> {
    if let Ok(value) =
        std::env::var("OPENDJ_LYRICMOTION_TOKEN").or_else(|_| std::env::var("LM_DIRECTOR_TOKEN"))
    {
        return (!value.trim().is_empty()).then(|| value.trim().into());
    }
    let path = std::env::var_os("OPENDJ_LYRICMOTION_TOKEN_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            let state = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state"))
                })?;
            Some(state.join("lyricmotion/director/access-token"))
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(&path).ok()?.permissions().mode() & 0o077 != 0 {
            return None;
        }
    }
    let value = std::fs::read_to_string(path).ok()?;
    (!value.trim().is_empty()).then(|| value.trim().into())
}
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(450)))
        .build()
        .into()
}
fn get(agent: &ureq::Agent, route: &str) -> Result<Value, String> {
    get_url(agent, &format!("{BASE}{route}"))
}
fn get_url(agent: &ureq::Agent, url: &str) -> Result<Value, String> {
    agent
        .get(url)
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_json()
        .map_err(|e| e.to_string())
}
fn post(agent: &ureq::Agent, route: &str, value: &Value) -> Result<Value, String> {
    post_url(agent, &format!("{BASE}{route}"), value, token().as_deref())
}
fn post_url(
    agent: &ureq::Agent,
    url: &str,
    value: &Value,
    token: Option<&str>,
) -> Result<Value, String> {
    let mut request = agent.post(url).header("X-Lyric-Motion", "local");
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    request
        .send_json(value)
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_json()
        .map_err(|e| e.to_string())
}

/// Preserve native starts. The bridge estimates missing ends without writing LRC.
fn lyric_packet(lyrics: &lyrics::Lyrics, duration: f64) -> Value {
    let lines = lyrics.lines.iter().enumerate().take(512).filter_map(|(i, line)| {
        let start = line.start?;
        let end = line.end.or_else(|| lyrics.lines.get(i + 1).and_then(|l| l.start)).unwrap_or(duration);
        if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start || line.words.len() > 128 { return None; }
        // Plain line timing is passed as such. Node can derive display words.
        if line.words.is_empty() { return Some(json!({"start": start, "end": end, "text": line.text, "estimated": true})); }
        let words: Vec<Value> = line.words.iter().enumerate().filter_map(|(j, word)| {
            let stop = line.words.get(j + 1).map(|w| w.start).unwrap_or(end);
            let content = word.text.trim();
            (word.start >= start && stop <= end && stop > word.start && !content.is_empty() && content.chars().count() <= 120)
                .then(|| json!({"start":word.start,"end":stop,"text":content,"estimated":true}))
        }).collect();
        (!words.is_empty()).then(|| json!({"start":start,"end":end,"words":words,"estimated":line.end.is_none()}))
    }).collect::<Vec<_>>();
    // Respect the external source's bounded contract, keeping whole lines.
    let mut count = 0;
    let lines: Vec<_> = lines
        .into_iter()
        .take_while(|line| {
            count += line["words"].as_array().map_or_else(
                || {
                    line["text"]
                        .as_str()
                        .unwrap_or("")
                        .split_whitespace()
                        .count()
                },
                Vec::len,
            );
            count <= 4000
        })
        .collect();
    json!({"source":"opendj-native-lyrics","timing":"native-starts-estimated-ends","lines":lines})
}

fn native_beat(track: &Track, position: f64) -> Option<f64> {
    let known_grid = matches!(track.source, Source::Synth { .. })
        || track.info.fixed_grid
        || track.info.grid_locked
        || track.info.grid_from == "analysis"
        || !track.grid.is_empty();
    (known_grid && !track.info.grid_unsure && track.bpm > 0.0 && track.first_beat.is_finite())
        .then(|| track.beat_at(position))
        .filter(|beat| beat.is_finite())
}

#[derive(Default)]
struct ProducerClock {
    sequence: u64,
    epoch: u64,
    last: Option<(usize, String, f64, f64, Instant)>,
}
impl ProducerClock {
    fn observe(&mut self, deck: usize, id: &str, position: f64, speed: f64, now: Instant) {
        let changed = self
            .last
            .as_ref()
            .is_none_or(|(old_deck, old_id, seconds, rate, at)| {
                let expected = seconds + now.duration_since(*at).as_secs_f64() * rate;
                *old_deck != deck
                    || old_id != id
                    || position < expected - 0.25
                    || position > expected + 1.5
            });
        if changed {
            self.epoch += 1;
        }
        self.sequence += 1;
        self.last = Some((deck, id.into(), position, speed, now));
    }
}

fn producer(
    shared: Arc<Shared>,
    registry: Arc<ControlRegistry>,
    loaded: LoadedTracks,
    visual: Arc<VisualPositions>,
) {
    let play: Vec<_> = (0..NUM_DECKS)
        .map(|d| registry.toggle(&format!("[Channel{}]", d + 1), "play"))
        .collect();
    let mut random = [0u8; 16];
    if getrandom::fill(&mut random).is_err() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            ^ u128::from(std::process::id());
        random = stamp.to_be_bytes();
    }
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    let hex = format!("{:032x}", u128::from_be_bytes(random));
    let session = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    let mut master = None;
    let mut previous: Option<(usize, Arc<Track>)> = None;
    let mut lyrics = Value::Null;
    let mut clock = ProducerClock::default();
    while shared.alive.load(Ordering::Relaxed) {
        if shared.enabled.load(Ordering::Relaxed) {
            let tracks = loaded.try_read().ok().map(|t| t.clone());
            if let Some(tracks) = tracks {
                let source = shared.source.load(Ordering::Relaxed);
                master = if source > 0 {
                    Some((source - 1).min(NUM_DECKS - 1))
                } else {
                    master
                        .filter(|&d| play[d].get_bool())
                        .or_else(|| (0..NUM_DECKS).find(|&d| play[d].get_bool()))
                        .or_else(|| (0..NUM_DECKS).find(|&d| tracks[d].is_some()))
                };
                if let Some((deck, track)) = master.and_then(|d| tracks[d].clone().map(|t| (d, t)))
                {
                    let changed = !previous
                        .as_ref()
                        .is_some_and(|(d, t)| *d == deck && Arc::ptr_eq(t, &track));
                    if changed {
                        lyrics = match &track.source {
                            Source::File(path) => lyrics::find_with(
                                path,
                                crate::settings::config_dir()
                                    .map(|d| d.join("lyrics"))
                                    .as_deref(),
                            )
                            .map(|l| lyric_packet(&l, track.duration))
                            .unwrap_or_else(|| json!({"lines":[]})),
                            _ => json!({"lines":[]}),
                        };
                        previous = Some((deck, track.clone()));
                    }
                    let (position, speed) = visual.position(deck, Instant::now());
                    let id = match &track.source {
                        Source::File(path) => path.to_string_lossy().into_owned(),
                        _ => format!("opendj:synth:{}", track.id),
                    };
                    let beat = native_beat(&track, position);
                    clock.observe(
                        deck,
                        &id,
                        position,
                        if play[deck].get_bool() { speed } else { 0.0 },
                        Instant::now(),
                    );
                    let packet = json!({
                        "version": 1,
                        "providerId": "opendj",
                        "providerSession": session,
                        "sequence": clock.sequence,
                        "epoch": clock.epoch,
                        "trackId": id,
                        "deck": deck + 1,
                        "position": position.max(0.0),
                        "playing": play[deck].get_bool(),
                        "rate": speed.abs().max(0.25),
                        "bpm": track.bpm_at(position),
                        "beatpos": beat,
                        "beatSource": if beat.is_some() { "opendj-native-grid" } else { "unavailable" },
                        "duration": track.duration,
                        "title": track.title,
                        "artist": track.artist,
                        "lyrics": lyrics,
                    });
                    if let Ok(mut latest) = shared.frame.try_lock() {
                        *latest = Some(packet);
                    }
                } else if let Ok(mut latest) = shared.frame.try_lock() {
                    *latest = None;
                }
            }
        }
        std::thread::sleep(PERIOD);
    }
}

fn input_worker(shared: Arc<Shared>) {
    let agent = agent();
    let mut sent_lyrics: Option<Value> = None;
    let mut active = false;
    let mut session = String::new();
    while shared.alive.load(Ordering::Relaxed) {
        let enabled = shared.enabled.load(Ordering::Relaxed);
        if enabled {
            let packet = shared.frame.try_lock().ok().and_then(|f| f.clone());
            if let Some(mut packet) = packet {
                session = text(&packet["providerSession"]);
                let key = json!([packet["trackId"], packet["deck"], packet["lyrics"]]);
                if sent_lyrics.as_ref() == Some(&key) {
                    packet.as_object_mut().unwrap().remove("lyrics");
                }
                match post(&agent, "/api/input/frame", &packet) {
                    Ok(_) => {
                        sent_lyrics = Some(key);
                        active = true;
                        shared.input_online.store(true, Ordering::Relaxed);
                    }
                    Err(_) => {
                        shared.input_online.store(false, Ordering::Relaxed);
                        sent_lyrics = None;
                    }
                }
            } else if active {
                active = post(
                    &agent,
                    "/api/input/frame",
                    &json!({"version":1,"providerId":"opendj","enabled":false,"providerSession":session}),
                ).is_err();
                shared.input_online.store(false, Ordering::Relaxed);
                sent_lyrics = None;
            }
        } else if active {
            active = post(
                &agent,
                "/api/input/frame",
                &json!({"version":1,"providerId":"opendj","enabled":false,"providerSession":session}),
            ).is_err();
            shared.input_online.store(false, Ordering::Relaxed);
            sent_lyrics = None;
        }
        std::thread::sleep(PERIOD);
    }
    if active {
        let _ = post(
            &agent,
            "/api/input/frame",
            &json!({"version":1,"providerId":"opendj","enabled":false,"providerSession":session}),
        );
    }
}

const CATALOG_PAGE_SIZE: usize = 128;
fn catalog_page(rows: &[Value], query: &str, page: usize) -> (Vec<Value>, String, usize) {
    let query = query.trim().to_lowercase();
    let matches: Vec<_> = rows
        .iter()
        .filter(|row| {
            query.is_empty()
                || ["id", "name", "universeId", "family"].iter().any(|key| {
                    row[*key]
                        .as_str()
                        .is_some_and(|value| value.to_lowercase().contains(&query))
                })
        })
        .collect();
    let pages = matches.len().div_ceil(CATALOG_PAGE_SIZE).max(1);
    let page = page.min(pages - 1);
    let selected = matches
        .iter()
        .skip(page * CATALOG_PAGE_SIZE)
        .take(CATALOG_PAGE_SIZE)
        .map(|row| (*row).clone())
        .collect();
    (
        selected,
        format!("{} matches · page {}/{}", matches.len(), page + 1, pages),
        pages,
    )
}

fn worker(shared: Arc<Shared>, commands: mpsc::Receiver<Value>) {
    let agent = agent();
    let mut poll = Instant::now() - Duration::from_secs(2);
    let mut catalog_poll = Instant::now() - Duration::from_secs(31);
    let mut view = View::default();
    let mut published_note = String::new();
    let mut catalogs: [Vec<Value>; 2] = Default::default();
    let mut previous_catalog: Option<[(String, usize); 2]> = None;
    while shared.alive.load(Ordering::Relaxed) {
        let mut publish = false;
        for command in commands.try_iter().take(8) {
            view.note = match post(&agent, "/api/director/command", &command) {
                Ok(result) => format!(
                    "{} · {}",
                    text(&result["commandId"]),
                    text(&result["status"])
                ),
                Err(error) => format!("Command response unavailable · {error} · check queue"),
            };
            poll = Instant::now() - Duration::from_secs(2);
        }
        if poll.elapsed() >= Duration::from_secs(1) {
            publish = true;
            poll = Instant::now();
            match get(&agent, "/api/director/status") {
                Ok(status) => {
                    view.online = true;
                    view.status = status;
                    if let Ok(data) = get(&agent, "/api/director/decisions?limit=24") {
                        view.history = rows(&data, "commands")
                            .iter()
                            .rev()
                            .take(12)
                            .map(|c| {
                                format!(
                                    "{} · {} · {} {} · {}",
                                    text(&c["operation"]),
                                    text(&c["status"]),
                                    text(&c["schedule"]["clock"]),
                                    text(&c["schedule"]["at"]),
                                    text(&c["reason"])
                                )
                            })
                            .collect();
                        view.history.extend(
                            rows(&data, "decisions")
                                .iter()
                                .rev()
                                .take(4)
                                .map(|d| format!("Director · {}", text(&d["reason"]))),
                        );
                    }
                    if catalog_poll.elapsed() >= Duration::from_secs(30) {
                        catalog_poll = Instant::now();
                        if let Ok(data) = get(&agent, "/api/director/universes") {
                            view.universes = rows(&data, "universes");
                        }
                        if let Ok(data) = get(&agent, "/api/director/scenes?scope=all") {
                            catalogs[0] = rows(&data, "scenes");
                            previous_catalog = None;
                        }
                        if let Ok(data) = get(&agent, "/api/director/effects") {
                            catalogs[1] = rows(&data, "effects");
                            previous_catalog = None;
                        }
                    }
                    view.targets = rows(&view.status["renderer"], "targets");
                }
                Err(error) => {
                    view.online = false;
                    view.note = format!("Director offline · {error}");
                }
            }
        }
        let selection = shared
            .catalog
            .try_lock()
            .ok()
            .map(|catalog| catalog.clone());
        if let Some(catalog) = selection {
            if previous_catalog.as_ref() != Some(&catalog) {
                let (scenes, scene_label, scene_pages) =
                    catalog_page(&catalogs[0], &catalog[0].0, catalog[0].1);
                let (assets, asset_label, asset_pages) =
                    catalog_page(&catalogs[1], &catalog[1].0, catalog[1].1);
                view.scenes = scenes;
                view.assets = assets;
                view.catalog_labels = [scene_label, asset_label];
                view.catalog_pages = [scene_pages, asset_pages];
                previous_catalog = Some(catalog.clone());
                publish = true;
            }
        }
        if publish || view.note != published_note {
            if let Ok(mut snapshot) = shared.view.try_lock() {
                *snapshot = view.clone();
                published_note = view.note.clone();
                shared.revision.fetch_add(1, Ordering::Relaxed);
            }
        }
        std::thread::sleep(PERIOD);
    }
}

fn number(value: SharedString) -> Result<f64, String> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| "Enter a finite number".into())
}
fn selected(rows: &[Value], index: i32) -> Result<&Value, String> {
    rows.get(index.max(0) as usize)
        .ok_or_else(|| "Choose an available item".into())
}
fn command(ui: &MotionUi<'_>, view: &View, operation: &str) -> Result<Value, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut command =
        json!({"operation":operation,"commandId":format!("opendj-{}-{stamp}",std::process::id())});
    if operation == "policy.update" {
        let universe = selected(&view.universes, ui.get_universe_index())?;
        let weight = number(ui.get_weight())?;
        if !(0.0..=10.0).contains(&weight) {
            return Err("Weight must be 0–10".into());
        }
        let id = universe["id"].as_str().ok_or("Universe has no id")?;
        let mut weights = view.status["policy"]["universeWeights"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        weights.insert(id.into(), json!(weight));
        command["params"] = json!({"mode":if ui.get_mode()==1 {"StrictMeaning"}else{"ExploreBalanced"},"lockedUniverse":if ui.get_lock_universe(){json!(id)}else{Value::Null},"universeWeights":weights});
        return Ok(command);
    }
    if !view.online {
        return Err("Director offline".into());
    }
    let transport = &view.status["transport"];
    command["expected"] = json!({"trackId":transport["trackId"],"epoch":transport["epoch"]});
    if operation == "operator.release" {
        return Ok(command);
    }
    if ui.get_clock() > 0 {
        if ui.get_clock() == 2 && !transport["beatPos"].is_number() {
            return Err("Native beat unavailable for this source".into());
        }
        let at = number(ui.get_at())?;
        if at < 0.0 {
            return Err("Cue time must be positive".into());
        }
        command["schedule"] =
            json!({"clock":if ui.get_clock()==2{"native-beat"}else{"track-seconds"},"at":at});
    }
    if operation == "scene.select" || operation == "operator.override" {
        command["params"] = json!({"sceneId":selected(&view.scenes,ui.get_scene_index())?["id"],"durationBeats":16});
    } else if operation == "action.schedule" {
        let target = selected(&view.targets, ui.get_target_index())?;
        let capability = target["capabilities"]
            .as_array()
            .and_then(|caps| caps.get(ui.get_capability_index().max(0) as usize))
            .and_then(Value::as_str)
            .ok_or("Choose a registered capability")?;
        command["target"] =
            json!({"type":target["type"],"id":target["id"],"capability":capability});
        let strength = number(ui.get_strength())?;
        command["params"] = match capability {
            "word.motion" => {
                json!({"motion":(["rise","drop","shiver","freeze","ripple","heartbeat","stamp","bounce"][ui.get_motion().clamp(0,7) as usize]),"strength":strength})
            }
            "effect.play" => {
                json!({"catalogId":selected(&view.assets,ui.get_asset_index())?["id"],"strength":strength,"duration":number(ui.get_duration())?})
            }
            "light.intensity" => json!({"intensity":strength}),
            "scene.select" => {
                json!({"sceneId":selected(&view.scenes,ui.get_scene_index())?["id"],"durationBeats":16})
            }
            "entity.motion" | "layer.transform" | "camera.pose" => {
                json!({"x":number(ui.get_x())?,"y":number(ui.get_y())?,"scale":number(ui.get_scale())?})
            }
            _ => return Err("Unsupported capability".into()),
        };
    }
    Ok(command)
}

fn saved_options(settings: &Value) -> (bool, usize) {
    let enabled = settings["lyricmotion"]["enabled"]
        .as_bool()
        .unwrap_or(false);
    let source = settings["lyricmotion"]["source"]
        .as_u64()
        .filter(|value| *value <= 4)
        .unwrap_or(0) as usize;
    (enabled, source)
}

fn request_launch(shared: Arc<Shared>, launcher: std::ffi::OsString) -> bool {
    if shared
        .starting
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    shared.revision.fetch_add(1, Ordering::Relaxed);
    let state = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("lyricmotion launcher".into())
        .spawn(move || {
            let result = std::process::Command::new(launcher)
                .arg("--no-panel")
                .spawn()
                .and_then(|mut child| child.wait());
            let note = match result {
                Ok(status) if status.success() => "Visual launcher requested".into(),
                Ok(status) => format!("Visual launcher exited · {status}"),
                Err(error) => format!("Visual launcher unavailable · {error}"),
            };
            if let Ok(mut latest) = state.launch_note.lock() {
                *latest = note;
            }
            state.starting.store(false, Ordering::Relaxed);
            state.revision.fetch_add(1, Ordering::Relaxed);
        });
    if let Err(error) = spawned {
        if let Ok(mut note) = shared.launch_note.lock() {
            *note = format!("Visual launcher unavailable · {error}");
        }
        shared.starting.store(false, Ordering::Relaxed);
        shared.revision.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

pub fn install(
    app: &AppWindow,
    registry: Arc<ControlRegistry>,
    loaded: LoadedTracks,
    visual: Arc<VisualPositions>,
) -> slint::Timer {
    let (enabled, source) = saved_options(&crate::settings::load());
    app.global::<MotionUi>().set_enabled(enabled);
    app.global::<MotionUi>().set_source(source as i32);
    let launcher = std::env::var_os("OPENDJ_LYRICMOTION_LAUNCHER").filter(|path| !path.is_empty());
    app.global::<MotionUi>()
        .set_launcher_available(launcher.is_some());
    let shared = Arc::new(Shared {
        enabled: AtomicBool::new(enabled),
        starting: AtomicBool::new(false),
        launch_note: Mutex::new(String::new()),
        catalog: Mutex::new(Default::default()),
        input_online: AtomicBool::new(false),
        source: AtomicUsize::new(source),
        revision: AtomicU64::new(0),
        alive: AtomicBool::new(true),
        frame: Mutex::new(None),
        view: Mutex::new(View::default()),
    });
    let (sender, receiver): (SyncSender<Value>, _) = mpsc::sync_channel(8);
    let producer_shared = shared.clone();
    let _ = std::thread::Builder::new()
        .name("lyricmotion clock".into())
        .spawn(move || producer(producer_shared, registry, loaded, visual));
    let input_shared = shared.clone();
    let _ = std::thread::Builder::new()
        .name("lyricmotion input".into())
        .spawn(move || input_worker(input_shared));
    let worker_shared = shared.clone();
    let _ = std::thread::Builder::new()
        .name("lyricmotion HTTP".into())
        .spawn(move || worker(worker_shared, receiver));
    let weak = app.as_weak();
    let state = shared.clone();
    app.global::<MotionUi>().on_command(move |operation| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let ui = app.global::<MotionUi>();
        let result = state
            .view
            .try_lock()
            .map_err(|_| "Director busy".into())
            .and_then(|view| command(&ui, &view, &operation))
            .and_then(|value| {
                sender
                    .try_send(value)
                    .map_err(|_| "Visual command queue full".into())
            });
        ui.set_feedback(
            result
                .err()
                .unwrap_or_else(|| "Queued locally".into())
                .into(),
        );
    });
    let weak = app.as_weak();
    let state = shared.clone();
    app.global::<MotionUi>().on_target_changed(move || {
        if let Some(app) = weak.upgrade() {
            if let Ok(view) = state.view.try_lock() {
                let ui = app.global::<MotionUi>();
                let caps = view
                    .targets
                    .get(ui.get_target_index().max(0) as usize)
                    .and_then(|t| t["capabilities"].as_array());
                ui.set_capabilities(strings(
                    caps.into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                ));
                ui.set_capability_index(0);
            }
        }
    });
    let weak = app.as_weak();
    let state = shared.clone();
    app.global::<MotionUi>().on_universe_changed(move || {
        if let Some(app) = weak.upgrade() {
            if let Ok(view) = state.view.try_lock() {
                let ui = app.global::<MotionUi>();
                if let Ok(universe) = selected(&view.universes, ui.get_universe_index()) {
                    ui.set_weight(
                        universe["weight"]
                            .as_f64()
                            .unwrap_or(1.0)
                            .to_string()
                            .into(),
                    );
                    ui.set_lock_universe(universe["locked"].as_bool().unwrap_or(false));
                }
            }
        }
    });
    let weak = app.as_weak();
    app.global::<MotionUi>().on_options_changed(move || {
        if let Some(app) = weak.upgrade() {
            let ui = app.global::<MotionUi>();
            crate::settings::set(
                "lyricmotion",
                json!({"enabled": ui.get_enabled(), "source": ui.get_source().clamp(0,4)}),
            );
        }
    });
    let weak = app.as_weak();
    let state = shared.clone();
    app.global::<MotionUi>().on_catalog_search(move |kind| {
        if let Some(app) = weak.upgrade() {
            let ui = app.global::<MotionUi>();
            let kind = kind.clamp(0, 1) as usize;
            if let Ok(mut catalog) = state.catalog.try_lock() {
                catalog[kind] = (
                    if kind == 0 {
                        ui.get_scene_query()
                    } else {
                        ui.get_asset_query()
                    }
                    .to_string(),
                    0,
                );
                if kind == 0 {
                    ui.set_scene_index(0);
                } else {
                    ui.set_asset_index(0);
                }
            }
        }
    });
    let weak = app.as_weak();
    let state = shared.clone();
    app.global::<MotionUi>()
        .on_catalog_page(move |kind, delta| {
            if let Some(app) = weak.upgrade() {
                let ui = app.global::<MotionUi>();
                let kind = kind.clamp(0, 1) as usize;
                if let (Ok(mut catalog), Ok(view)) =
                    (state.catalog.try_lock(), state.view.try_lock())
                {
                    let last = view.catalog_pages[kind].saturating_sub(1);
                    catalog[kind].1 = catalog[kind]
                        .1
                        .saturating_add_signed(delta as isize)
                        .min(last);
                    if kind == 0 {
                        ui.set_scene_index(0);
                    } else {
                        ui.set_asset_index(0);
                    }
                }
            }
        });
    let state = shared.clone();
    app.global::<MotionUi>().on_start_visuals(move || {
        if let Some(launcher) = launcher.as_ref() {
            request_launch(state.clone(), launcher.clone());
        }
    });
    app.global::<MotionUi>().on_open_preview(|| {
        std::thread::spawn(|| {
            #[cfg(target_os = "windows")]
            let _ = std::process::Command::new("cmd")
                .args(["/C", "start", "", BASE])
                .spawn();
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("open").arg(BASE).spawn();
            #[cfg(all(unix, not(target_os = "macos")))]
            let _ = std::process::Command::new("xdg-open").arg(BASE).spawn();
        });
    });
    let timer = slint::Timer::default();
    let weak = app.as_weak();
    let lifetime = Lifetime(shared.clone());
    let mut previous_note = String::new();
    let mut previous_launch_note = String::new();
    let mut previous_revision = u64::MAX;
    let mut previous_enabled = false;
    let mut previous_input = false;
    let mut policy_initialized = false;
    timer.start(slint::TimerMode::Repeated, PERIOD, move || {
        let _keep_alive = &lifetime;
        let Some(app) = weak.upgrade() else {
            shared.alive.store(false, Ordering::Relaxed);
            return;
        };
        let ui = app.global::<MotionUi>();
        shared.enabled.store(ui.get_enabled(), Ordering::Relaxed);
        shared
            .source
            .store(ui.get_source().clamp(0, 4) as usize, Ordering::Relaxed);
        let revision = shared.revision.load(Ordering::Relaxed);
        let input_online = shared.input_online.load(Ordering::Relaxed);
        if previous_revision == revision
            && previous_enabled == ui.get_enabled()
            && previous_input == input_online
        {
            return;
        }
        if let Ok(view) = shared.view.try_lock() {
            previous_revision = revision;
            ui.set_starting(shared.starting.load(Ordering::Relaxed));
            ui.set_renderer_offline(!view.online || view.status["renderer"]["connected"] != true);
            previous_enabled = ui.get_enabled();
            previous_input = input_online;
            ui.set_status(
                if !ui.get_enabled() {
                    "Visual bridge off"
                } else if !input_online {
                    "Visual input offline"
                } else if view.online && view.status["renderer"]["connected"] == true {
                    "Director connected"
                } else if view.online {
                    "Director ready · renderer offline"
                } else {
                    "Visuals offline"
                }
                .into(),
            );
            if let Ok(frame) = shared.frame.try_lock() {
                if let Some(packet) = frame.as_ref() {
                    ui.set_origin(
                        format!(
                            "Deck {} · {} · {}",
                            text(&packet["deck"]),
                            text(&packet["title"]),
                            text(&packet["beatSource"])
                        )
                        .into(),
                    );
                }
            }
            let transport = &view.status["transport"];
            ui.set_clock_status(
                format!(
                    "{} s · beat {} · queue {}",
                    text(&transport["positionSeconds"]),
                    text(&transport["beatPos"]),
                    text(&view.status["queue"]["pending"])
                )
                .into(),
            );
            ui.set_scene(format!("Scene · {}", text(&view.status["renderer"]["scene"])).into());
            ui.set_explanation(text(&view.status["decision"]["reason"]).into());
            ui.set_universes(strings(view.universes.iter().map(label)));
            ui.set_policy_status(
                format!(
                    "Policy · {} · {}",
                    text(&view.status["policy"]["mode"]),
                    view.status["policy"]["lockedUniverse"]
                        .as_str()
                        .unwrap_or("all universes")
                )
                .into(),
            );
            if !policy_initialized && !view.universes.is_empty() {
                ui.set_mode(if view.status["policy"]["mode"] == "StrictMeaning" {
                    1
                } else {
                    0
                });
                let index = view
                    .universes
                    .iter()
                    .position(|u| u["locked"].as_bool() == Some(true))
                    .unwrap_or(0);
                ui.set_universe_index(index as i32);
                ui.set_weight(
                    view.universes[index]["weight"]
                        .as_f64()
                        .unwrap_or(1.0)
                        .to_string()
                        .into(),
                );
                ui.set_lock_universe(view.universes[index]["locked"].as_bool().unwrap_or(false));
                policy_initialized = true;
            }

            ui.set_scenes(strings(view.scenes.iter().map(label)));
            ui.set_scene_page(view.catalog_labels[0].clone().into());
            ui.set_asset_page(view.catalog_labels[1].clone().into());
            ui.set_targets(strings(
                view.targets
                    .iter()
                    .map(|t| format!("{} · {}", text(&t["type"]), text(&t["id"]))),
            ));
            let caps = view
                .targets
                .get(ui.get_target_index().max(0) as usize)
                .and_then(|t| t["capabilities"].as_array());
            ui.set_capabilities(strings(
                caps.into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            ));
            ui.set_assets(strings(view.assets.iter().map(label)));
            ui.set_history(strings(view.history.clone()));
            if previous_note != view.note {
                previous_note = view.note.clone();
                ui.set_feedback(view.note.clone().into());
            }
            if let Ok(note) = shared.launch_note.try_lock() {
                if *note != previous_launch_note {
                    previous_launch_note = note.clone();
                    ui.set_feedback(note.clone().into());
                }
            }
        }
    });
    timer
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_bridge_options_restore_valid_values_and_default_legacy_settings() {
        assert_eq!(saved_options(&json!({"unrelated":"preserved"})), (false, 0));
        assert_eq!(
            saved_options(&json!({"lyricmotion":{"enabled":true,"source":4}})),
            (true, 4)
        );
        assert_eq!(
            saved_options(&json!({"lyricmotion":{"enabled":"invalid","source":99}})),
            (false, 0)
        );
        assert_eq!(
            saved_options(&json!({"lyricmotion":{"enabled":true,"source":-1}})),
            (true, 0)
        );
    }
    #[test]
    fn catalog_search_and_paging_expose_rows_beyond_the_previous_512_limit() {
        let data = json!({"scenes":(0..1908).map(|id|json!({"id":format!("scene-{id}"),"name":format!("Scene {id}"),"universeId":"illustrated"})).collect::<Vec<_>>()});
        let all = rows(&data, "scenes");
        assert_eq!(all.len(), 1908);
        let (last, label, pages) = catalog_page(&all, "", usize::MAX);
        assert_eq!(pages, 15);
        assert_eq!(last.last().unwrap()["id"], "scene-1907");
        assert!(label.contains("1908 matches"));
        let (found, _, _) = catalog_page(&all, "SCENE 1907", 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["id"], "scene-1907");
        assert_eq!(catalog_page(&all, "ILLUSTRATED", 0).2, 15);
        assert_eq!(catalog_page(&all, "missing", 0).0.len(), 0);
    }
    #[cfg(unix)]
    #[test]
    fn manual_launcher_is_guarded_and_only_retries_after_another_request() {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("opendj-visual-launch-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("launcher");
        let count = directory.join("count");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n[ \"$1\" = --no-panel ] || exit 1\nprintf x >> '{}'\nsleep 0.15\n",
                count.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let state = Arc::new(Shared::default());
        assert!(request_launch(
            state.clone(),
            script.clone().into_os_string()
        ));
        assert!(!request_launch(
            state.clone(),
            script.clone().into_os_string()
        ));
        let wait = || {
            let at = Instant::now();
            while state.starting.load(Ordering::Relaxed) && at.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!state.starting.load(Ordering::Relaxed));
        };
        wait();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(std::fs::read_to_string(&count).unwrap(), "x");
        assert!(request_launch(state.clone(), script.into_os_string()));
        wait();
        assert_eq!(std::fs::read_to_string(&count).unwrap(), "xx");
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn queued_visual_command_preserves_expected_epoch_and_tracks_execution() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for status in ["queued", "executed"] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let headers = String::from_utf8(request).unwrap().to_lowercase();
                if status == "queued" {
                    assert!(headers.contains("x-lyric-motion: local"));
                    assert!(headers.contains("authorization: bearer fixture-token"));
                    let size = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    let mut body = vec![0; size];
                    socket.read_exact(&mut body).unwrap();
                    let value: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(value["expected"]["epoch"], 3);
                    assert_eq!(value["operation"], "action.schedule");
                }
                let body = json!({"commandId":"fixture-command","status":status}).to_string();
                let response=format!("HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        let agent = agent();
        let value = json!({"commandId":"fixture-command","operation":"action.schedule","expected":{"trackId":"external:track","epoch":3},"target":{"type":"word","id":"active","capability":"word.motion"},"params":{"motion":"rise","strength":1}});
        assert_eq!(
            post_url(
                &agent,
                &format!("http://{address}/api/director/command"),
                &value,
                Some("fixture-token")
            )
            .unwrap()["status"],
            "queued"
        );
        assert_eq!(
            get_url(
                &agent,
                &format!("http://{address}/api/director/commands/fixture-command")
            )
            .unwrap()["status"],
            "executed"
        );
        server.join().unwrap();
    }
    #[test]
    fn unknown_or_uncertain_grids_do_not_publish_a_guessed_beat() {
        let mut track = (*opendj_engine::sample_library()[0]).clone();
        track.bpm = 120.0;
        track.first_beat = 0.2;
        track.info.grid_unsure = false;
        assert_eq!(native_beat(&track, 1.2), Some(2.0));
        track.info.grid_unsure = true;
        assert_eq!(native_beat(&track, 1.2), None);
        track.info.grid_unsure = false;
        track.source = Source::File("unknown.mp3".into());
        track.info.fixed_grid = false;
        track.info.grid_locked = false;
        track.info.grid_from.clear();
        track.grid = Default::default();
        assert_eq!(native_beat(&track, 1.2), None);
    }
    #[test]
    fn producer_sequences_and_epochs_follow_track_deck_and_seek() {
        let mut clock = ProducerClock::default();
        let at = Instant::now();
        clock.observe(0, "track-a", 2.0, 1.0, at);
        assert_eq!((clock.sequence, clock.epoch), (1, 1));
        clock.observe(0, "track-a", 2.05, 1.0, at + PERIOD);
        assert_eq!((clock.sequence, clock.epoch), (2, 1));
        clock.observe(0, "track-a", 20.0, 1.0, at + PERIOD * 2);
        assert_eq!(clock.epoch, 2);
        clock.observe(1, "track-a", 20.05, 1.0, at + PERIOD * 3);
        assert_eq!(clock.epoch, 3);
        clock.observe(1, "track-b", 0.0, 0.0, at + PERIOD * 4);
        assert_eq!((clock.sequence, clock.epoch), (5, 4));
    }
    #[test]
    fn stalled_http_is_bounded_and_reconnects_to_a_local_fixture() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stalled, _) = listener.accept().unwrap();
            let mut data = [0; 1024];
            let _ = stalled.read(&mut data);
            std::thread::sleep(Duration::from_millis(650));
            drop(stalled);
            let (mut retry, _) = listener.accept().unwrap();
            let _ = retry.read(&mut data);
            retry.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
        });
        let agent = agent();
        let at = Instant::now();
        assert!(get_url(&agent, &format!("http://{address}/status")).is_err());
        assert!(at.elapsed() < Duration::from_millis(900));
        assert_eq!(
            get_url(&agent, &format!("http://{address}/status")).unwrap()["ok"],
            true
        );
        server.join().unwrap();
    }
    #[test]
    fn native_lyric_starts_remain_original_and_ends_are_estimated() {
        let lyrics = lyrics::Lyrics {
            origin: lyrics::Origin::Tag,
            auto: false,
            lines: vec![
                lyrics::Line {
                    start: Some(2.0),
                    end: None,
                    text: "hello world".into(),
                    words: vec![
                        lyrics::Word {
                            start: 2.0,
                            text: "hello ".into(),
                        },
                        lyrics::Word {
                            start: 2.6,
                            text: "world".into(),
                        },
                    ],
                },
                lyrics::Line {
                    start: Some(4.0),
                    end: Some(5.0),
                    text: "next".into(),
                    words: vec![],
                },
            ],
        };
        let packet = lyric_packet(&lyrics, 8.0);
        assert_eq!(packet["lines"][0]["words"][0]["start"], 2.0);
        assert_eq!(packet["lines"][0]["words"][0]["end"], 2.6);
        assert_eq!(packet["lines"][0]["words"][1]["end"], 4.0);
        assert_eq!(packet["lines"][0]["words"][1]["estimated"], true);
        assert_eq!(lyrics.lines[0].words[1].start, 2.6);
    }
    #[test]
    fn visual_queue_is_bounded() {
        let (tx, _rx) = mpsc::sync_channel(8);
        for _ in 0..8 {
            assert!(tx.try_send(json!({})).is_ok());
        }
        assert!(tx.try_send(json!({})).is_err());
    }
}
