//! The embedded Maschine's screen reads the actual DECKARD project. All edits
//! pass through its document, including the document's undo history.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use opendj_control::{ControlHandle, ControlRegistry};
use opendj_controllers::{
    maschine_surface::{Action, Browse, BrowserView},
    MaschineProvider,
};
use opendj_engine::maschine::{
    instrument_defaults, Chain, Color, Document, FileItem, MaschineHandle, NewModule, Note,
    Project, SoundInfo, Target, PPQ,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

pub(crate) use crate::maschine_projects::write_new;
use crate::maschine_projects::{fresh_path, NewProject, Projects};
use crate::maschine_surface::{MaschineSurfaceProvider, UiRequest};
use crate::{
    prefs, AppWindow, MaschineBrowserFilter, MaschineBrowserItem, MaschineGroup,
    MaschineInstrument, MaschineNote, MaschineParam, MaschinePattern, MaschineScene, MaschineSound,
    MaschineUi,
};

#[path = "maschine_launch.rs"]
mod launch;

const DECK: usize = 2;

pub const NATIVE_INSTRUMENTS: &[(&str, &str)] = &[
    ("Sampler", "Sampler"),
    ("Audio", "AudioModule"),
    ("Kick", "Kick"),
    ("Snare", "Snare"),
    ("Hi-Hat", "Hihat"),
    ("Tom", "Tom"),
    ("Percussion", "Perc"),
    ("Cymbal", "Cymbal"),
    ("Bass Synth", "BassSynth"),
    ("Poly Synth", "PolySynth"),
];

pub fn native_module(class: &str) -> Option<NewModule> {
    match class {
        "Sampler" => Some(NewModule::Sampler),
        "AudioModule" => Some(NewModule::Audio),
        _ => Some(NewModule::Instrument {
            class: class.into(),
            params: instrument_defaults(class)?
                .into_iter()
                .map(|(field, value)| (field.into(), value))
                .collect(),
        }),
    }
}

pub fn load_native(
    doc: &mut Document,
    group: usize,
    sound: usize,
    class: &str,
) -> Result<(), String> {
    let module = native_module(class).ok_or("Unknown native instrument")?;
    let previous = doc
        .project()
        .groups
        .get(group)
        .and_then(|g| g.sounds.get(sound))
        .ok_or("No Sound selected")?;
    let rename = previous.name.is_empty() || previous.name == previous.instrument;
    let replace = doc
        .chain_slots(Chain::Sound(group, sound))
        .and_then(|slots| slots.first())
        .is_some_and(|slot| slot.is_instrument());
    if replace {
        doc.remove_slot(group, sound, 0)?;
    }
    doc.add_slot(group, sound, 0, &module)?;
    if rename {
        let name = NATIVE_INSTRUMENTS
            .iter()
            .find(|(_, c)| *c == class)
            .map(|(name, _)| *name)
            .unwrap_or(class);
        doc.rename(Target::Sound(group, sound), name)?;
    }
    Ok(())
}

// The same palette used by DECKARD's software and hardware views.
const PALETTE: [[u8; 3]; 17] = [
    [0xff, 0x42, 0x3e],
    [0xff, 0x74, 0x42],
    [0xfc, 0x98, 0x24],
    [0xff, 0xdd, 0x1a],
    [0xff, 0xff, 0x19],
    [0xa3, 0xff, 0x18],
    [0x53, 0xff, 0x53],
    [0x19, 0xea, 0xa3],
    [0x19, 0xea, 0xeb],
    [0x18, 0xce, 0xff],
    [0x4d, 0x94, 0xff],
    [0x84, 0x72, 0xff],
    [0xb6, 0x6e, 0xff],
    [0xdf, 0x5f, 0xff],
    [0xff, 0x53, 0xf0],
    [0xff, 0x48, 0x80],
    [0xff, 0xff, 0xff],
];

#[derive(Clone)]
struct Selection {
    group: ControlHandle,
    sound: ControlHandle,
    pattern: ControlHandle,
    scene: ControlHandle,
    tempo: ControlHandle,
}

impl Selection {
    fn new(registry: &ControlRegistry) -> Self {
        let get = |key| {
            registry
                .get("[Maschine3]", key)
                .unwrap_or_else(|| panic!("missing Maschine control {key}"))
        };
        Self {
            group: get("group"),
            sound: get("sound"),
            pattern: get("pattern"),
            scene: get("scene"),
            tempo: get("tempo"),
        }
    }

    fn indices(&self) -> (usize, usize, usize, usize) {
        (
            (self.group.get().round() as usize).min(7),
            (self.sound.get().round() as usize).min(15),
            self.pattern.get().round() as usize,
            self.scene.get().round() as usize,
        )
    }
}

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}

fn color(value: Color, default: usize) -> slint::Color {
    let slot = match value {
        Color::Default => default % 16,
        Color::Palette(index) => usize::from(index).min(16),
    };
    let [r, g, b] = PALETTE[slot];
    slint::Color::from_rgb_u8(r, g, b)
}

fn report(app: &AppWindow, result: Result<(), String>) {
    app.global::<MaschineUi>()
        .set_status(result.err().unwrap_or_default().into());
}

type Command = Box<dyn FnOnce(&MaschineHandle) -> Result<(), String> + Send>;

#[derive(Clone)]
struct Editor {
    commands: mpsc::Sender<Command>,
}

impl Editor {
    fn new(app: &AppWindow, handle: MaschineHandle, surface: Arc<MaschineSurfaceProvider>) -> Self {
        let (commands, rx) = mpsc::channel::<Command>();
        let weak = app.as_weak();
        std::thread::spawn(move || {
            while let Ok(command) = rx.recv() {
                let result = command(&handle);
                surface.report_result(DECK + 1, &result);
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak.upgrade() {
                        report(&app, result);
                    }
                });
            }
        });
        Self { commands }
    }

    fn command(
        &self,
        command: impl FnOnce(&MaschineHandle) -> Result<(), String> + Send + 'static,
    ) {
        let _ = self.commands.send(Box::new(command));
    }

    fn edit(&self, edit: impl FnOnce(&mut Document) -> Result<(), String> + Send + 'static) {
        self.command(move |handle| handle.edit(edit));
    }
}

struct ShownPattern {
    project: Arc<Project>,
    group: usize,
    pattern: usize,
}

fn note_edit(
    editor: &Editor,
    shown: &RefCell<Option<ShownPattern>>,
    index: i32,
    edit: impl FnOnce(Note, i64) -> Option<Note> + Send + 'static,
) -> Result<(), String> {
    let index = usize::try_from(index).map_err(|_| "No note selected")?;
    let shown = shown.borrow();
    let shown = shown.as_ref().ok_or("Project is updating")?;
    let (group, pattern) = (shown.group, shown.pattern);
    let start = shown
        .project
        .groups
        .get(group)
        .and_then(|g| g.patterns.get(pattern))
        .map(|p| p.start)
        .ok_or("The pattern changed")?;
    let old = shown
        .project
        .groups
        .get(group)
        .and_then(|g| g.patterns.get(pattern))
        .and_then(|p| p.notes.get(index))
        .copied()
        .ok_or("The note changed")?;
    editor.edit(move |doc| {
        let present = doc
            .project()
            .groups
            .get(group)
            .and_then(|g| g.patterns.get(pattern))
            .is_some_and(|p| p.notes.contains(&old));
        if !present {
            return Err("The note changed".into());
        }
        let add = edit(old, start).into_iter().collect::<Vec<_>>();
        doc.edit_notes("Edit Note", group, pattern, &[old], &add)
    });
    Ok(())
}

/// Installs the on-screen instrument on deck 3. Keep the returned timer alive.
pub fn install(
    app: &AppWindow,
    registry: Arc<ControlRegistry>,
    handle: MaschineHandle,
    surface: Arc<MaschineSurfaceProvider>,
) -> slint::Timer {
    let selection = Selection::new(&registry);
    let shown = Rc::new(RefCell::new(None));
    let editor = Editor::new(app, handle.clone(), surface.clone());
    let projects = surface.projects();
    let ui = app.global::<MaschineUi>();
    let parameters = Rc::new(VecModel::from(vec![MaschineParam::default(); 8]));
    ui.set_parameters(ModelRc::from(parameters.clone()));
    let browser_rows = Rc::new(VecModel::from(Vec::<MaschineBrowserItem>::new()));
    ui.set_browser_results(ModelRc::from(browser_rows.clone()));
    ui.set_instruments(model(
        NATIVE_INSTRUMENTS
            .iter()
            .map(|(name, class)| MaschineInstrument {
                name: (*name).into(),
                detail: match *class {
                    "Sampler" => "Sample player",
                    "AudioModule" => "Audio loop player",
                    "BassSynth" | "PolySynth" => "Native synthesizer",
                    _ => "Native drum synthesizer",
                }
                .into(),
            })
            .collect(),
    ));

    {
        let editor = editor.clone();
        let selection = selection.clone();
        let handle = handle.clone();
        let weak = app.as_weak();
        ui.on_sound_launch(move |action| {
            let (group, sound, _, _) = selection.indices();
            let generation = handle.project_generation();
            let version = handle.snapshot_version();
            let bpm = selection.tempo.get();
            let action = action.to_string();
            if let Some(app) = weak.upgrade() {
                app.global::<MaschineUi>()
                    .set_status("Updating selected Sound…".into());
            }
            editor.command(move |handle| {
                let sampler = handle.snapshot_cached().is_some_and(|project| {
                    project
                        .groups
                        .get(group)
                        .and_then(|g| g.sounds.get(sound))
                        .is_some_and(|s| s.instrument == "Sampler")
                });
                let bars = action
                    .strip_prefix("fit-")
                    .and_then(|s| s.parse::<u32>().ok())
                    .filter(|_| sampler);
                if let Some(bars) = bars {
                    return launch::fit(handle, generation, version, group, sound, bars, bpm);
                }
                handle
                    .edit_deferred_for_project(generation, move |doc| {
                        launch::apply(doc, group, sound, &action)
                    })?
                    .recv()
                    .map_err(|_| "Maschine editor stopped".to_string())??;
                Ok(())
            });
        });
    }

    {
        let editor = editor.clone();
        let selection = selection.clone();
        let surface = surface.clone();
        ui.on_load_instrument(move |index| {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let Some((_, class)) = NATIVE_INSTRUMENTS.get(index) else {
                return;
            };
            let class = *class;
            let (group, sound, _, _) = selection.indices();
            let surface = surface.clone();
            editor.command(move |handle| {
                handle.edit(move |doc| load_native(doc, group, sound, class))?;
                surface.dispatch(
                    DECK + 1,
                    vec![Action::Level(2), Action::ModulesVisible(true)],
                );
                Ok(())
            });
        });
    }

    {
        let editor = editor.clone();
        let projects = projects.clone();
        ui.on_open_project(move || {
            let editor = editor.clone();
            let projects = projects.clone();
            prefs::choose_file("Open Maschine project", move |path| {
                editor.command(move |handle| open_project(handle, &projects, &path));
            });
        });
    }
    {
        let editor = editor.clone();
        let projects = projects.clone();
        ui.on_save_project(move || {
            let editor = editor.clone();
            let projects = projects.clone();
            prefs::choose_folder("Save Maschine project in folder", move |folder| {
                editor.command(move |handle| save_project(handle, &projects, Some(folder)));
            });
        });
    }
    {
        let editor = editor.clone();
        let selection = selection.clone();
        ui.on_pick_pattern(move |pattern| {
            let (group, _, _, scene) = selection.indices();
            let Ok(pattern) = usize::try_from(pattern) else {
                return;
            };
            editor.command(move |handle| handle.select_pattern(scene, group, Some(pattern)));
        });
    }
    {
        let selection = selection.clone();
        ui.on_edit_pattern(move |pattern| {
            if pattern >= 0 {
                selection.pattern.set(f64::from(pattern));
            }
        });
    }
    {
        let editor = editor.clone();
        let selection = selection.clone();
        ui.on_toggle_group_pattern(move || {
            let (group, _, pattern, scene) = selection.indices();
            editor.edit(move |doc| {
                let assigned = doc
                    .project()
                    .scenes
                    .get(scene)
                    .is_some_and(|s| s.patterns.iter().any(|(g, _)| *g == group));
                doc.assign_pattern(scene, group, if assigned { None } else { Some(pattern) })
            });
        });
    }
    {
        let editor = editor.clone();
        let selection = selection.clone();
        ui.on_add_pattern(move || {
            let (group, _, from, scene) = selection.indices();
            let selected = selection.pattern.clone();
            editor.edit(move |doc| {
                let count = doc
                    .project()
                    .groups
                    .get(group)
                    .ok_or("No group selected")?
                    .patterns
                    .len();
                let index = doc.add_pattern(group, from, &format!("Pattern {}", count + 1))?;
                doc.assign_pattern(scene, group, Some(index))?;
                selected.set(index as f64);
                Ok(())
            });
        });
    }
    {
        let editor = editor.clone();
        let selection = selection.clone();
        ui.on_add_scene(move || {
            let (group, _, pattern, _) = selection.indices();
            let selected = selection.scene.clone();
            editor.edit(move |doc| {
                let scene = doc.add_scene()?;
                doc.assign_pattern(scene, group, Some(pattern))?;
                selected.set(scene as f64);
                Ok(())
            });
        });
    }
    {
        let editor = editor.clone();
        let selection = selection.clone();
        ui.on_add_note(move |sound, pitch, start, length| {
            let (group, _, pattern, _) = selection.indices();
            if !(0..16).contains(&sound) || !start.is_finite() || !length.is_finite() {
                return;
            }
            let note = Note {
                sound: sound as usize,
                pitch: i64::from(pitch.clamp(0, 127)),
                position: (f64::from(start.max(0.0)) * PPQ as f64).round() as i64,
                length: (f64::from(length.max(0.25)) * PPQ as f64).round() as i64,
                velocity: 100,
                probability: 100,
                expr: None,
            };
            editor.edit(move |doc| {
                let start = doc
                    .project()
                    .groups
                    .get(group)
                    .and_then(|g| g.patterns.get(pattern))
                    .map(|p| p.start)
                    .ok_or("No pattern selected")?;
                let note = Note {
                    position: note.position + start,
                    ..note
                };
                doc.add_note(group, pattern, note)
            });
        });
    }
    {
        let editor = editor.clone();
        let shown = shown.clone();
        let weak = app.as_weak();
        ui.on_edit_note(move |index, start, length, pitch, sound| {
            if !start.is_finite() || !length.is_finite() {
                return;
            }
            let result = note_edit(&editor, &shown, index, move |mut note, pattern_start| {
                note.position =
                    pattern_start + (f64::from(start.max(0.0)) * PPQ as f64).round() as i64;
                note.length = (f64::from(length.max(0.25)) * PPQ as f64).round() as i64;
                note.pitch = i64::from(pitch.clamp(0, 127));
                note.sound = sound.clamp(0, 15) as usize;
                Some(note)
            });
            if let Err(error) = result {
                if let Some(app) = weak.upgrade() {
                    report(&app, Err(error));
                }
            }
        });
    }
    {
        let editor = editor.clone();
        let shown = shown.clone();
        let weak = app.as_weak();
        ui.on_delete_note(move |index| {
            if let Err(error) = note_edit(&editor, &shown, index, |_, _| None) {
                if let Some(app) = weak.upgrade() {
                    report(&app, Err(error));
                }
            }
        });
    }
    {
        let editor = editor.clone();
        let shown = shown.clone();
        let weak = app.as_weak();
        ui.on_note_velocity(move |index, velocity| {
            let result = note_edit(&editor, &shown, index, move |mut note, _| {
                note.velocity = i64::from(velocity.clamp(1, 127));
                Some(note)
            });
            if let Err(error) = result {
                if let Some(app) = weak.upgrade() {
                    report(&app, Err(error));
                }
            }
        });
    }
    {
        let surface = surface.clone();
        ui.on_set_parameter(move |knob, value| {
            if let Ok(knob) = usize::try_from(knob) {
                surface.set_parameter(DECK + 1, knob, f64::from(value));
            }
        });
    }
    {
        let surface = surface.clone();
        ui.on_parameter_page_step(move |by| surface.parameter_page_step(DECK + 1, by));
    }
    {
        let surface = surface.clone();
        ui.on_browser_kind_pick(move |kind| {
            if (0..7).contains(&kind) {
                surface.dispatch(DECK + 1, vec![Action::Browse(Browse::Kind(kind as usize))]);
            }
        });
    }
    {
        let surface = surface.clone();
        ui.on_browser_filter(move |which, index| {
            let selected = (index > 0).then(|| (index - 1) as usize);
            let action = match which {
                0 => Browse::Product(selected),
                1..=3 => Browse::Attribute((which - 1) as usize, selected),
                _ => return,
            };
            surface.dispatch(DECK + 1, vec![Action::Browse(action)]);
        });
    }
    {
        let surface = surface.clone();
        ui.on_browser_focus_result(move |index, load| {
            if let Ok(index) = usize::try_from(index) {
                surface.browser_focus(DECK + 1, index, load);
            }
        });
    }
    {
        let surface = surface.clone();
        ui.on_browser_action(move |action| {
            let action = match action.as_str() {
                "load" => Browse::Load,
                "prehear" => Browse::Prehear,
                "favorite" => Browse::SetFavorite,
                "favorites" => Browse::Favorites,
                "source" => {
                    Browse::User(!surface.view(DECK + 1).is_some_and(|view| view.browser.user))
                }
                _ => return,
            };
            surface.dispatch(DECK + 1, vec![Action::Browse(action)]);
        });
    }

    let weak = app.as_weak();
    let seen = Cell::new(None);
    let seen_browser = RefCell::new(None);
    let needs_main = registry.get("[Maschine3]", "needs_main");
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            let Some(app) = weak.upgrade() else { return };
            for request in surface.take_ui_requests() {
                match request {
                    UiRequest::Instrument(deck) if deck == DECK + 1 => {
                        show_panel(&app, 0);
                        app.global::<MaschineUi>().set_instrument_menu_open(true);
                    }
                    UiRequest::Sampling(deck) if deck == DECK + 1 => show_panel(&app, 2),
                    UiRequest::Browse(deck) if deck == DECK + 1 => show_panel(&app, 3),
                    UiRequest::PluginEditor(deck, chain, slot) if deck == DECK + 1 => {
                        let level = match chain { Chain::Master => 0, Chain::Group(_) => 1, Chain::Sound(_, _) => 2 };
                        show_panel(&app, 0);
                        surface.dispatch(DECK + 1, vec![Action::Level(level), Action::ModulesVisible(true), Action::Slot(slot as i64)]);
                        if handle.snapshot_cached().is_some_and(|project| {
                            let slots = match chain {
                                Chain::Master => Some(project.master_slots.as_slice()),
                                Chain::Group(g) => project.groups.get(g).map(|g| g.slots.as_slice()),
                                Chain::Sound(g, s) => project.groups.get(g).and_then(|g| g.sounds.get(s)).map(|s| s.slots.as_slice()),
                            };
                            slots.and_then(|slots| slots.get(slot)).is_some_and(|slot| slot.plugin.is_some())
                        }) {
                            report(&app, Err("Hosted plug-in windows are unavailable in the embedded instrument".into()));
                        }
                    }
                    UiRequest::File(deck, command) if deck == DECK + 1 => {
                        file_request(&app, &editor, &selection, &projects, command);
                    }
                    UiRequest::LoadProject(deck, path) if deck == DECK + 1 => {
                        let projects = projects.clone();
                        editor.command(move |handle| {
                            if !path.is_file() {
                                return Err(format!("Recent Maschine project is missing or unavailable: {}", path.display()));
                            }
                            open_project(handle, &projects, &path)
                        });
                    }
                    _ => report(
                        &app,
                        Err("The Maschine editor is assigned to deck 3".into()),
                    ),
                }
            }
            if needs_main.as_ref().is_some_and(ControlHandle::get_bool) {
                report(&app, handle.prepare_on_main());
            }
            let indices = selection.indices();
            let key = (handle.snapshot_version(), indices);
            let ui = app.global::<MaschineUi>();
            if seen.get().is_some_and(|(_, previous)| previous != indices) {
                ui.set_editing_note(false);
            }
            if seen.get() != Some(key) && !ui.get_editing_note() {
                if let Some(project) = handle.snapshot_cached() {
                    show(&app, &project, indices);
                    shown.replace(Some(ShownPattern {
                        project,
                        group: indices.0,
                        pattern: indices.2,
                    }));
                    seen.set(Some(key));
                }
            }
            let view = surface.view(DECK + 1).unwrap_or_default();
            for i in 0..8 {
                let parameter = match view.control.knobs.get(i).and_then(Option::as_ref) {
                    Some(knob) => MaschineParam {
                        name: knob.name.as_str().into(),
                        display: knob.text.as_str().into(),
                        value: knob.value as f32,
                        enabled: true,
                    },
                    None => MaschineParam::default(),
                };
                if parameters.row_data(i).as_ref() != Some(&parameter) {
                    parameters.set_row_data(i, parameter);
                }
            }
            show_browser(&app, &view.browser, &browser_rows, &seen_browser);
            if let Some(error) = surface.last_error(DECK + 1).or_else(|| handle.last_error()) {
                ui.set_status(error.into());
            }
            ui.set_parameter_page(
                view.control
                    .pages
                    .get(view.control.page)
                    .map(String::as_str)
                    .unwrap_or("Sound")
                    .into(),
            );
        },
    );
    timer
}

fn show_browser(
    app: &AppWindow,
    view: &BrowserView,
    rows: &VecModel<MaschineBrowserItem>,
    seen: &RefCell<Option<BrowserView>>,
) {
    let mut previous = seen.borrow_mut();
    if previous.as_ref() == Some(view) {
        return;
    }
    let ui = app.global::<MaschineUi>();
    if previous
        .as_ref()
        .is_none_or(|old| old.results != view.results)
    {
        rows.set_vec(
            view.results
                .iter()
                .map(|(name, favorite)| MaschineBrowserItem {
                    name: name.as_str().into(),
                    favorite: *favorite,
                })
                .collect::<Vec<_>>(),
        );
    }
    if previous.as_ref().is_none_or(|old| {
        old.products != view.products
            || old.product != view.product
            || old.attributes != view.attributes
    }) {
        let labels = [
            "All products",
            "All types",
            "All sub-types",
            "All characters",
        ];
        let filters = (0..4)
            .map(|index| {
                let (names, selected) = if index == 0 {
                    (&view.products, view.product)
                } else {
                    (&view.attributes[index - 1].0, view.attributes[index - 1].1)
                };
                MaschineBrowserFilter {
                    options: model(
                        std::iter::once(SharedString::from(labels[index]))
                            .chain(names.iter().map(|name| SharedString::from(name.as_str())))
                            .collect(),
                    ),
                    current: selected.map_or(0, |n| n as i32 + 1),
                }
            })
            .collect();
        ui.set_browser_filters(model(filters));
    }
    ui.set_browser_focus(view.focus as i32);
    ui.set_browser_kind(view.kind.min(6) as i32);
    ui.set_browser_favorites(view.favorites);
    ui.set_browser_busy(view.busy);
    ui.set_browser_user(view.user);
    *previous = Some(view.clone());
}

fn show_panel(app: &AppWindow, context: i32) {
    if app.get_layout() != 2 {
        app.set_layout(2);
        app.invoke_layout_changed(2);
    }
    app.set_library_expanded(false);
    app.global::<MaschineUi>().set_context(context);
}

fn file_request(
    app: &AppWindow,
    editor: &Editor,
    selection: &Selection,
    projects: &Arc<Projects>,
    command: &str,
) {
    match command {
        "new" => editor.command(MaschineHandle::new_project),
        "open" => app.global::<MaschineUi>().invoke_open_project(),
        "save" => {
            let projects = projects.clone();
            editor.command(move |handle| save_project(handle, &projects, None));
        }
        "save_copy" => {
            let projects = projects.clone();
            editor.command(move |handle| {
                let (_, current) = handle.snapshot_with_path()?;
                let folder = match current.and_then(|p| p.parent().map(Path::to_path_buf)) {
                    Some(folder) => folder,
                    None => projects.folder()?,
                };
                save_project(handle, &projects, Some(folder))
            });
        }
        "save_group" | "save_sound" => {
            let (group, sound, _, _) = selection.indices();
            let sound_file = command == "save_sound";
            let editor = editor.clone();
            prefs::choose_folder("Save Maschine item in folder", move |folder| {
                editor.edit(move |doc| {
                    let selected = doc.project().groups.get(group).ok_or("Group unavailable")?;
                    let (item, name, extension) = if sound_file {
                        let sound_name =
                            &selected.sounds.get(sound).ok_or("Sound unavailable")?.name;
                        (FileItem::Sound(group, sound), sound_name.clone(), "mxsnd")
                    } else {
                        (FileItem::Group(group), selected.name.clone(), "mxgrp")
                    };
                    let mut info = SoundInfo::default();
                    info.name = name.clone();
                    let bytes = doc.file_bytes(item, &info)?;
                    write_new(&fresh_path(&folder, &name, extension), &bytes)
                });
            });
        }
        "export_audio" => {
            let (_, _, _, scene) = selection.indices();
            let tempo = selection.tempo.get();
            let editor = editor.clone();
            prefs::choose_folder("Export Maschine audio into folder", move |folder| {
                editor.command(move |handle| {
                    crate::maschine_export::export_scene(handle, scene, folder, tempo)
                });
            });
        }
        _ => report(app, Err(format!("Unknown Maschine file command {command}"))),
    }
}

fn open_project(handle: &MaschineHandle, projects: &Projects, path: &Path) -> Result<(), String> {
    handle.open(path)?;
    projects
        .remember(path)
        .map_err(|e| format!("Project opened, but recent history could not be saved: {e}"))
}

fn save_project(
    handle: &MaschineHandle,
    projects: &Projects,
    copy_to: Option<PathBuf>,
) -> Result<(), String> {
    let (project, current) = handle.snapshot_with_path()?;
    let reservation;
    let path = match (copy_to, current) {
        (None, Some(path)) => path,
        (folder, _) => {
            let folder = match folder {
                Some(folder) => folder,
                None => projects.folder()?,
            };
            reservation = NewProject::reserve(&folder, &project.name)?;
            reservation.path.clone()
        }
    };
    handle.save_as(&path)?;
    projects
        .remember(&path)
        .map_err(|e| format!("Project saved, but recent history could not be saved: {e}"))
}

fn show(
    app: &AppWindow,
    project: &Project,
    (group, sound, pattern, scene): (usize, usize, usize, usize),
) {
    let ui = app.global::<MaschineUi>();
    let launch_sound = project.groups.get(group).and_then(|g| g.sounds.get(sound));
    ui.set_sound_launch_mode(launch::mode(launch_sound).into());
    ui.set_sound_launch_audio(launch_sound.is_some_and(|s| s.instrument == "AudioModule"));
    ui.set_sound_launch_bars(launch::audio_bars(project, launch_sound) as f32);
    let slot = launch_sound.and_then(|s| s.slots.first());
    ui.set_sound_launch_audio_mode(slot.and_then(|s| s.param("mode")).unwrap_or(0.0) as i32);
    ui.set_sound_launch_audio_engine(slot.and_then(|s| s.param("engine")).unwrap_or(1.0) as i32);
    ui.set_project_name(project.name.as_str().into());
    ui.set_groups(model(
        (0..8)
            .map(|i| {
                let item = project.groups.get(i);
                MaschineGroup {
                    name: item.map(|g| g.name.as_str()).unwrap_or("").into(),
                    color: color(item.map(|g| g.color).unwrap_or_default(), i * 2),
                }
            })
            .collect(),
    ));
    let selected = project.groups.get(group);
    ui.set_sounds(model(
        (0..16)
            .map(|i| {
                let item = selected.and_then(|g| g.sounds.get(i));
                let inherited = selected
                    .map(|g| color(g.color, group * 2))
                    .unwrap_or_default();
                MaschineSound {
                    name: item
                        .filter(|s| !s.name.is_empty())
                        .map(|s| SharedString::from(s.name.as_str()))
                        .unwrap_or_else(|| format!("Sound {}", i + 1).into()),
                    instrument: item.map(|s| s.instrument.as_str()).unwrap_or("").into(),
                    color: item
                        .filter(|s| s.color != Color::Default)
                        .map(|s| color(s.color, i * 2))
                        .unwrap_or(inherited),
                    loaded: item.is_some_and(|s| !s.instrument.is_empty() || !s.slots.is_empty()),
                    muted: item.is_some_and(|s| s.muted),
                    base_key: item.map_or(60, |s| s.channel.base_key) as i32,
                }
            })
            .collect(),
    ));
    ui.set_selected_sound_name(
        selected
            .and_then(|g| g.sounds.get(sound))
            .map(|s| s.name.as_str())
            .unwrap_or("")
            .into(),
    );
    ui.set_selected_instrument_name(
        selected
            .and_then(|g| g.sounds.get(sound))
            .map(|s| s.instrument.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Instrument")
            .into(),
    );
    let assigned = project.scenes.get(scene).and_then(|s| {
        s.patterns
            .iter()
            .find(|(g, _)| *g == group)
            .map(|(_, p)| *p)
    });
    ui.set_assigned_pattern(assigned.map_or(-1, |p| p as i32));
    ui.set_assigned_pattern_name(
        assigned
            .and_then(|p| selected?.patterns.get(p))
            .map_or("No pattern", |p| p.name.as_str())
            .into(),
    );
    ui.set_patterns(model(
        selected
            .map(|g| {
                g.patterns
                    .iter()
                    .map(|p| MaschinePattern {
                        name: p.name.as_str().into(),
                        detail: format!(
                            "{} bars · {} events",
                            p.length as f64 / project.bar_length().max(1) as f64,
                            p.notes.len()
                        )
                        .into(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    ));
    ui.set_scenes(model(
        project
            .scenes
            .iter()
            .map(|scene| MaschineScene {
                name: scene.name.as_str().into(),
                references: scene
                    .patterns
                    .iter()
                    .filter_map(|(g, p)| {
                        let pattern = project.groups.get(*g)?.patterns.get(*p)?;
                        Some(format!(
                            "{} {}",
                            char::from(b'A' + (*g).min(25) as u8),
                            pattern.name
                        ))
                    })
                    .collect::<Vec<_>>()
                    .join(" · ")
                    .into(),
            })
            .collect(),
    ));
    let pattern = selected.and_then(|g| g.patterns.get(pattern));
    ui.set_pattern_beats(
        pattern
            .map(|p| p.length as f32 / PPQ as f32)
            .unwrap_or(4.0)
            .max(0.25),
    );
    ui.set_notes(model(
        pattern
            .map(|p| {
                p.notes
                    .iter()
                    .enumerate()
                    .map(|(i, n)| MaschineNote {
                        index: i as i32,
                        sound: n.sound as i32,
                        pitch: n.pitch as i32,
                        start: (n.position - p.start) as f32 / PPQ as f32,
                        length: n.length as f32 / PPQ as f32,
                        velocity: n.velocity as i32,
                        probability: n.probability as i32,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    ));
}
