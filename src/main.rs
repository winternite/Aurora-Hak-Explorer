#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod archive;
#[cfg(target_os = "linux")]
mod drag_out;
#[cfg(target_os = "windows")]
#[path = "drag_out_windows.rs"]
mod drag_out;
mod resource_types;

use archive::{Archive, ArchiveKind, ArchiveVersion};
use eframe::egui::{self, Color32, RichText};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

const MAX_IMAGE_FILE_SIZE: u64 = 128 * 1024 * 1024;
const MAX_IMAGE_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_IMAGE_SIDE: u32 = 16_384;
const MAX_TEXTURE_SIDE: u32 = 4096;
const MAX_RECENT_ARCHIVES: usize = 8;
const MAX_MODEL_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EXTRACTED_MODEL_STRINGS: usize = 2_000;

fn main() -> eframe::Result {
    let arguments: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if arguments
        .first()
        .is_some_and(|argument| argument == "--repack")
    {
        if arguments.len() != 3 {
            eprintln!("usage: aurora-hak-explorer --repack INPUT OUTPUT");
            std::process::exit(2);
        }
        match Archive::open(&arguments[1]).and_then(|mut archive| archive.save(&arguments[2])) {
            Ok(()) => println!(
                "Repacked {} to {}",
                arguments[1].display(),
                arguments[2].display()
            ),
            Err(error) => {
                eprintln!("Could not repack archive: {error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "--validate")
    {
        let mut failed = false;
        for path in arguments.iter().skip(1) {
            match Archive::open(path) {
                Ok(archive) => println!(
                    "OK\t{}\t{} resources\t{}",
                    path.display(),
                    archive.entries.len(),
                    archive.version.label()
                ),
                Err(error) => {
                    eprintln!("ERROR\t{}\t{error}", path.display());
                    failed = true;
                }
            }
        }
        if failed {
            std::process::exit(1);
        }
        return Ok(());
    }
    #[allow(unused_mut)]
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1050.0, 700.0])
        .with_min_inner_size([760.0, 480.0])
        .with_icon(application_icon());
    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    // KDE's Wayland file-drop negotiation rejects inbound drops on some
    // versions, while XWayland's XDND bridge works reliably. Force only the
    // window to X11, leaving WAYLAND_DISPLAY available so arboard can share
    // the real desktop clipboard with native Wayland file managers.
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_some() {
        use winit::platform::x11::EventLoopBuilderExtX11;
        options.event_loop_builder = Some(Box::new(|builder| {
            builder.with_x11();
        }));
    }
    eframe::run_native(
        "Aurora Hak Explorer",
        options,
        Box::new(move |cc| {
            let mut editor = HakEditor::new(cc);
            for path in arguments.iter().filter(|path| path.is_file()) {
                editor.open_path(path.clone());
            }
            Ok(Box::new(editor))
        }),
    )
}

fn application_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../assets/aheicon-256.png"))
        .expect("the bundled application icon must be a valid PNG")
        .into_rgba8();
    egui::IconData {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
}

struct HakEditor {
    archive: Option<Archive>,
    selected: BTreeSet<usize>,
    filter: String,
    status: String,
    error: Option<String>,
    dirty: bool,
    show_about: bool,
    show_new: bool,
    new_kind: ArchiveKind,
    new_version: ArchiveVersion,
    show_description: bool,
    description_buffer: String,
    category: Option<String>,
    sort_column: SortColumn,
    sort_ascending: bool,
    selection_anchor: Option<usize>,
    selection_cursor: Option<usize>,
    hovered_drop_files: Vec<String>,
    pending_drop_files: Vec<PathBuf>,
    typeahead: String,
    typeahead_last: Option<Instant>,
    typeahead_pending: bool,
    tabs: Vec<TabState>,
    active_tab: Option<usize>,
    confirm_close_tab: Option<usize>,
    quit_in_progress: bool,
    force_quit: bool,
    pending_add: Option<AddBatch>,
    clipboard: Option<arboard::Clipboard>,
    clipboard_exports: Vec<tempfile::TempDir>,
    pending_clipboard_command: Option<ClipboardCommand>,
    cut_key_was_down: bool,
    paste_key_was_down: bool,
    compact_mode: bool,
    appearance: Appearance,
    recent_archives: Vec<PathBuf>,
    resource_middle_scroll_active: bool,
    image_preview: Option<ImagePreviewCache>,
    model_preview: Option<ModelPreviewCache>,
    model_view: ModelView,
}

struct ImagePreviewCache {
    key: String,
    result: Result<CachedImage, String>,
}

struct CachedImage {
    texture: egui::TextureHandle,
    width: usize,
    height: usize,
}

struct ModelPreviewCache {
    key: String,
    result: Result<ModelPreview, String>,
}

enum ModelPreview {
    Uncompiled {
        text: String,
        truncated: bool,
    },
    Compiled {
        name: Option<String>,
        structured_size: u64,
        raw_size: u64,
        strings: Vec<String>,
        truncated: bool,
    },
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ModelView {
    Summary,
    Strings,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SortColumn {
    Name,
    Type,
    Size,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Appearance {
    System,
    Dark,
    Light,
}

impl Appearance {
    fn preference(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Dark => egui::ThemePreference::Dark,
            Self::Light => egui::ThemePreference::Light,
        }
    }

    fn storage_value(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ConflictPolicy {
    Ask,
    ReplaceAll,
    SkipAll,
}

enum ConflictAction {
    Continue,
    Replace,
    Skip,
    ReplaceAll,
    SkipAll,
    Cancel,
}

#[derive(Clone, Copy)]
enum ClipboardCommand {
    Copy,
    Cut,
    Paste,
}

struct AddConflict {
    path: PathBuf,
    existing_filename: String,
}

struct AddBatch {
    target_tab: usize,
    selected_keys: BTreeSet<(String, u16)>,
    queue: VecDeque<PathBuf>,
    conflict: Option<AddConflict>,
    policy: ConflictPolicy,
    added: usize,
    replaced: usize,
    skipped: usize,
    failures: Vec<String>,
}

impl AddBatch {
    fn new(paths: Vec<PathBuf>, target_tab: usize, selected_keys: BTreeSet<(String, u16)>) -> Self {
        Self {
            target_tab,
            selected_keys,
            queue: paths.into(),
            conflict: None,
            policy: ConflictPolicy::Ask,
            added: 0,
            replaced: 0,
            skipped: 0,
            failures: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct TabState {
    archive: Archive,
    selected: BTreeSet<usize>,
    filter: String,
    dirty: bool,
    category: Option<String>,
    sort_column: SortColumn,
    sort_ascending: bool,
    selection_anchor: Option<usize>,
    selection_cursor: Option<usize>,
}

impl TabState {
    fn new(archive: Archive, dirty: bool) -> Self {
        Self {
            archive,
            selected: BTreeSet::new(),
            filter: String::new(),
            dirty,
            category: None,
            sort_column: SortColumn::Name,
            sort_ascending: true,
            selection_anchor: None,
            selection_cursor: None,
        }
    }

    fn label(&self) -> String {
        self.archive
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("Untitled")
            .to_owned()
    }
}

impl HakEditor {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let compact_mode = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, "compact_mode"))
            .unwrap_or(false);
        let legacy_dark_mode = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, "dark_mode"))
            .unwrap_or(true);
        let appearance = cc
            .storage
            .and_then(|storage| eframe::get_value::<String>(storage, "appearance"))
            .and_then(|value| match value.as_str() {
                "system" => Some(Appearance::System),
                "dark" => Some(Appearance::Dark),
                "light" => Some(Appearance::Light),
                _ => None,
            })
            .unwrap_or(if legacy_dark_mode {
                Appearance::Dark
            } else {
                Appearance::Light
            });
        let recent_archives = cc
            .storage
            .and_then(|storage| eframe::get_value::<Vec<String>>(storage, "recent_archives"))
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .filter(|path| path.is_file() && is_archive_path(path))
            .take(MAX_RECENT_ARCHIVES)
            .collect();
        cc.egui_ctx.set_theme(appearance.preference());
        Self {
            archive: None,
            selected: BTreeSet::new(),
            filter: String::new(),
            status: "Ready — open an archive or create a new one".into(),
            error: None,
            dirty: false,
            show_about: false,
            show_new: false,
            new_kind: ArchiveKind::Hak,
            new_version: ArchiveVersion::V1_0,
            show_description: false,
            description_buffer: String::new(),
            category: None,
            sort_column: SortColumn::Name,
            sort_ascending: true,
            selection_anchor: None,
            selection_cursor: None,
            hovered_drop_files: Vec::new(),
            pending_drop_files: Vec::new(),
            typeahead: String::new(),
            typeahead_last: None,
            typeahead_pending: false,
            tabs: Vec::new(),
            active_tab: None,
            confirm_close_tab: None,
            quit_in_progress: false,
            force_quit: false,
            pending_add: None,
            clipboard: arboard::Clipboard::new().ok(),
            clipboard_exports: Vec::new(),
            pending_clipboard_command: None,
            cut_key_was_down: false,
            paste_key_was_down: false,
            compact_mode,
            appearance,
            recent_archives,
            resource_middle_scroll_active: false,
            image_preview: None,
            model_preview: None,
            model_view: ModelView::Summary,
        }
    }

    fn show_image_preview(&mut self, ui: &mut egui::Ui, entry: &archive::Entry) {
        let extension = entry.extension();
        let size = entry.size().unwrap_or(0);
        let key = format!(
            "image-preview:{}:{}:{}:{}",
            self.active_tab.unwrap_or(usize::MAX),
            entry.filename(),
            entry.type_id,
            size
        );

        if self
            .image_preview
            .as_ref()
            .is_none_or(|cache| cache.key != key)
        {
            let result = (|| -> Result<CachedImage, String> {
                if size > MAX_IMAGE_FILE_SIZE {
                    return Err(format!(
                        "Image is too large to preview ({} limit)",
                        human_size(MAX_IMAGE_FILE_SIZE)
                    ));
                }
                let bytes = entry
                    .read_prefix(size)
                    .map_err(|error| format!("Could not read image: {error}"))?;
                let decoded = decode_preview_image(&bytes, &extension)?;
                let width = decoded.width() as usize;
                let height = decoded.height() as usize;
                let decoded =
                    if decoded.width() > MAX_TEXTURE_SIDE || decoded.height() > MAX_TEXTURE_SIDE {
                        decoded.thumbnail(MAX_TEXTURE_SIDE, MAX_TEXTURE_SIDE)
                    } else {
                        decoded
                    };
                let rgba = decoded.into_rgba8();
                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                    [rgba.width() as usize, rgba.height() as usize],
                    rgba.as_raw(),
                );
                let texture =
                    ui.ctx()
                        .load_texture(key.clone(), color_image, egui::TextureOptions::LINEAR);
                Ok(CachedImage {
                    texture,
                    width,
                    height,
                })
            })();
            self.image_preview = Some(ImagePreviewCache { key, result });
        }

        match &self.image_preview.as_ref().unwrap().result {
            Ok(image) => {
                let natural_size = image.texture.size_vec2();
                let scale = (ui.available_width().max(1.0) / natural_size.x)
                    .min(420.0 / natural_size.y)
                    .min(1.0);
                ui.vertical_centered(|ui| {
                    ui.add(
                        egui::Image::new(&image.texture).fit_to_exact_size(natural_size * scale),
                    );
                    ui.label(format!("{} × {} pixels", image.width, image.height));
                    if extension.eq_ignore_ascii_case("plt") {
                        ui.small(
                            "Representative PLT layer colors; final colors are chosen in-game.",
                        );
                    }
                });
            }
            Err(error) => {
                ui.vertical_centered(|ui| {
                    ui.add_space(90.0);
                    ui.colored_label(Color32::LIGHT_RED, error);
                });
            }
        }
    }

    fn show_model_preview(&mut self, ui: &mut egui::Ui, entry: &archive::Entry) {
        let size = entry.size().unwrap_or(0);
        let key = format!(
            "model-preview:{}:{}:{}:{}",
            self.active_tab.unwrap_or(usize::MAX),
            entry.filename(),
            entry.type_id,
            size
        );
        if self
            .model_preview
            .as_ref()
            .is_none_or(|cache| cache.key != key)
        {
            let result = entry
                .read_prefix(size.min(MAX_MODEL_PREVIEW_BYTES))
                .map_err(|error| format!("Could not read model: {error}"))
                .and_then(|bytes| decode_model_preview(&bytes, size));
            self.model_preview = Some(ModelPreviewCache { key, result });
            self.model_view = ModelView::Summary;
        }

        match &self.model_preview.as_ref().unwrap().result {
            Ok(ModelPreview::Uncompiled { text, truncated }) => {
                ui.label(RichText::new("Model Uncompiled").size(16.0).strong());
                ui.label("ASCII model source");
                if *truncated {
                    ui.small(format!(
                        "Preview limited to the first {}",
                        human_size(MAX_MODEL_PREVIEW_BYTES)
                    ));
                }
                ui.separator();
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true));
                });
            }
            Ok(ModelPreview::Compiled {
                name,
                structured_size,
                raw_size,
                strings,
                truncated,
            }) => {
                ui.label(RichText::new("Model Compiled").size(16.0).strong());
                ui.label("Aurora binary model");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.model_view, ModelView::Summary, "Summary");
                    ui.selectable_value(
                        &mut self.model_view,
                        ModelView::Strings,
                        "Extracted strings",
                    );
                });
                ui.separator();
                match self.model_view {
                    ModelView::Summary => {
                        egui::Grid::new("compiled_model_summary")
                            .num_columns(2)
                            .show(ui, |ui| {
                                ui.label("Model name:");
                                ui.label(name.as_deref().unwrap_or("Unavailable"));
                                ui.end_row();
                                ui.label("Structured data:");
                                ui.label(human_size(*structured_size));
                                ui.end_row();
                                ui.label("Raw geometry data:");
                                ui.label(human_size(*raw_size));
                                ui.end_row();
                                ui.label("Readable strings:");
                                ui.label(strings.len().to_string());
                                ui.end_row();
                            });
                        if *truncated {
                            ui.add_space(8.0);
                            ui.small(format!(
                                "String extraction limited to the first {}",
                                human_size(MAX_MODEL_PREVIEW_BYTES)
                            ));
                        }
                    }
                    ModelView::Strings => {
                        ui.label(format!("{} readable strings", strings.len()));
                        if *truncated {
                            ui.small(format!(
                                "Scanned the first {} of this model",
                                human_size(MAX_MODEL_PREVIEW_BYTES)
                            ));
                        }
                        egui::ScrollArea::vertical()
                            .auto_shrink(false)
                            .show(ui, |ui| {
                                for string in strings {
                                    ui.add(
                                        egui::Label::new(RichText::new(string).monospace())
                                            .selectable(true),
                                    );
                                }
                            });
                    }
                }
            }
            Err(error) => {
                ui.label(RichText::new("Model format unknown").size(16.0).strong());
                ui.colored_label(Color32::LIGHT_RED, error);
            }
        }
    }

    fn fail(&mut self, context: &str, error: impl std::fmt::Display) {
        self.error = Some(format!("{context}: {error}"));
    }
    fn current_tab_state(&self) -> Option<TabState> {
        Some(TabState {
            archive: self.archive.clone()?,
            selected: self.selected.clone(),
            filter: self.filter.clone(),
            dirty: self.dirty,
            category: self.category.clone(),
            sort_column: self.sort_column,
            sort_ascending: self.sort_ascending,
            selection_anchor: self.selection_anchor,
            selection_cursor: self.selection_cursor,
        })
    }
    fn sync_current_tab(&mut self) {
        let Some(index) = self.active_tab else {
            return;
        };
        let Some(state) = self.current_tab_state() else {
            return;
        };
        if let Some(tab) = self.tabs.get_mut(index) {
            *tab = state;
        }
    }

    fn blocking_dialog_open(&self) -> bool {
        self.confirm_close_tab.is_some()
            || self.pending_add.is_some()
            || self.show_new
            || self.show_description
            || self.show_about
            || self.error.is_some()
    }

    fn selected_resource_keys(&self) -> BTreeSet<(String, u16)> {
        let Some(archive) = self.archive.as_ref() else {
            return BTreeSet::new();
        };
        self.selected
            .iter()
            .filter_map(|index| archive.entries.get(*index))
            .map(|entry| (entry.name.to_ascii_lowercase(), entry.type_id))
            .collect()
    }

    fn restore_selection_by_keys(&mut self, keys: &BTreeSet<(String, u16)>) {
        self.selected = self
            .archive
            .as_ref()
            .map(|archive| {
                archive
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| {
                        keys.contains(&(entry.name.to_ascii_lowercase(), entry.type_id))
                    })
                    .map(|(index, _)| index)
                    .collect()
            })
            .unwrap_or_default();
        self.selection_anchor = self.selected.first().copied();
        self.selection_cursor = self.selected.last().copied();
    }
    fn load_tab(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index).cloned() else {
            return;
        };
        self.archive = Some(tab.archive);
        self.selected = tab.selected;
        self.filter = tab.filter;
        self.dirty = tab.dirty;
        self.category = tab.category;
        self.sort_column = tab.sort_column;
        self.sort_ascending = tab.sort_ascending;
        self.selection_anchor = tab.selection_anchor;
        self.selection_cursor = tab.selection_cursor;
        self.active_tab = Some(index);
        self.typeahead.clear();
        self.typeahead_pending = false;
        self.image_preview = None;
    }
    fn switch_tab(&mut self, index: usize) {
        if self.active_tab == Some(index) {
            return;
        }
        self.sync_current_tab();
        self.load_tab(index);
        if let Some(path) = self
            .archive
            .as_ref()
            .and_then(|archive| archive.path.as_ref())
        {
            self.status = format!("Switched to {}", path.display());
        }
    }
    fn close_tab(&mut self, index: usize) {
        self.sync_current_tab();
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.archive = None;
            self.selected.clear();
            self.selection_anchor = None;
            self.selection_cursor = None;
            self.filter.clear();
            self.category = None;
            self.dirty = false;
            self.active_tab = None;
            self.status = "Ready — open an archive or create a new one".into();
            return;
        }
        let next = match self.active_tab {
            Some(active) if active > index => active - 1,
            Some(active) if active == index => index.min(self.tabs.len() - 1),
            Some(active) => active,
            None => 0,
        };
        self.load_tab(next);
    }
    fn continue_quit(&mut self) {
        self.sync_current_tab();
        let dirty_tab = self
            .active_tab
            .filter(|index| self.tabs.get(*index).is_some_and(|tab| tab.dirty))
            .or_else(|| self.tabs.iter().position(|tab| tab.dirty));
        if let Some(index) = dirty_tab {
            self.quit_in_progress = true;
            self.confirm_close_tab = Some(index);
            self.switch_tab(index);
        } else {
            self.quit_in_progress = false;
            self.confirm_close_tab = None;
            self.force_quit = true;
        }
    }
    fn request_quit(&mut self) {
        if let Some(batch) = self.pending_add.take() {
            self.status = format!(
                "Import canceled — added {}, replaced {}, skipped {}",
                batch.added, batch.replaced, batch.skipped
            );
        }
        if !self.quit_in_progress {
            self.continue_quit();
        }
    }
    fn capture_typeahead(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let now = Instant::now();
        if self
            .typeahead_last
            .is_some_and(|last| now.duration_since(last) > Duration::from_millis(1200))
        {
            self.typeahead.clear();
        }
        let mut changed = false;
        ctx.input(|input| {
            for event in &input.events {
                match event {
                    egui::Event::Text(text) => {
                        for character in text.chars() {
                            if character.is_ascii_alphanumeric() || character == '_' {
                                self.typeahead.push(character.to_ascii_lowercase());
                                changed = true;
                            }
                        }
                    }
                    egui::Event::Key {
                        key: egui::Key::Backspace,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.is_none() => {
                        self.typeahead.pop();
                        changed = true;
                    }
                    egui::Event::Key {
                        key: egui::Key::Escape,
                        pressed: true,
                        ..
                    } => {
                        self.typeahead.clear();
                        changed = true;
                    }
                    _ => {}
                }
            }
        });
        if changed {
            self.typeahead_last = Some(now);
            self.typeahead_pending = true;
        }
    }
    fn open_path(&mut self, path: PathBuf) {
        let path = fs::canonicalize(&path).unwrap_or(path);
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.archive.path.as_deref() == Some(path.as_path()))
        {
            self.remember_recent_archive(&path);
            self.switch_tab(index);
            return;
        }
        match Archive::open(&path) {
            Ok(archive) => {
                let count = archive.entries.len();
                self.sync_current_tab();
                self.tabs.push(TabState::new(archive, false));
                self.load_tab(self.tabs.len() - 1);
                self.status = format!("Opened {} — {count} resources", path.display());
                self.remember_recent_archive(&path);
            }
            Err(e) => self.fail("Could not open archive", e),
        }
    }
    fn remember_recent_archive(&mut self, path: &std::path::Path) {
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.recent_archives.retain(|recent| recent != &path);
        self.recent_archives.insert(0, path);
        self.recent_archives.truncate(MAX_RECENT_ARCHIVES);
    }
    fn open_dialog(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("NWN archives", &["hak", "erf", "mod", "sav"])
            .pick_files()
        {
            for path in paths {
                self.open_path(path);
            }
        }
    }
    fn save(&mut self, save_as: bool) {
        let Some(archive) = self.archive.as_mut() else {
            return;
        };
        let path = if !save_as { archive.path.clone() } else { None }.or_else(|| {
            rfd::FileDialog::new()
                .add_filter("NWN archive", &[archive.kind.extension()])
                .set_file_name(format!("untitled.{}", archive.kind.extension()))
                .save_file()
        });
        let Some(path) = path else {
            return;
        };
        match archive.save(&path) {
            Ok(()) => {
                self.dirty = false;
                self.status = format!("Saved {}", path.display());
                // Saving can change the archive path and replace external
                // entries with archive slices. Update the tab once here
                // instead of cloning the full archive every UI frame.
                self.sync_current_tab();
                self.remember_recent_archive(&path);
            }
            Err(e) => self.fail("Could not save archive", e),
        }
    }
    fn add_files(&mut self) {
        let Some(paths) = rfd::FileDialog::new().pick_files() else {
            return;
        };
        self.add_paths(paths);
    }
    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() || self.archive.is_none() {
            return;
        }
        if let Some(batch) = self.pending_add.as_mut() {
            if self.active_tab == Some(batch.target_tab) {
                batch.queue.extend(paths);
            } else {
                self.pending_drop_files.extend(paths);
            }
            return;
        }
        let Some(target_tab) = self.active_tab else {
            return;
        };
        let selected_keys = self.selected_resource_keys();
        self.pending_add = Some(AddBatch::new(paths, target_tab, selected_keys));
        self.process_add_batch(ConflictAction::Continue);
    }
    fn process_add_batch(&mut self, action: ConflictAction) {
        let Some(mut batch) = self.pending_add.take() else {
            return;
        };
        if self.active_tab != Some(batch.target_tab) {
            self.error = Some(
                "The destination archive changed during import. No remaining files were added."
                    .to_owned(),
            );
            return;
        }
        let Some(archive) = self.archive.as_mut() else {
            return;
        };
        let mut canceled = false;

        if let Some(conflict) = batch.conflict.take() {
            match action {
                ConflictAction::Replace => add_incoming(archive, &mut batch, conflict.path),
                ConflictAction::ReplaceAll => {
                    batch.policy = ConflictPolicy::ReplaceAll;
                    add_incoming(archive, &mut batch, conflict.path);
                }
                ConflictAction::Skip => batch.skipped += 1,
                ConflictAction::SkipAll => {
                    batch.policy = ConflictPolicy::SkipAll;
                    batch.skipped += 1;
                }
                ConflictAction::Cancel => canceled = true,
                ConflictAction::Continue => {
                    batch.conflict = Some(conflict);
                }
            }
        }

        while !canceled && batch.conflict.is_none() {
            let Some(path) = batch.queue.pop_front() else {
                break;
            };
            match archive.conflicting_filename(&path) {
                Ok(Some(existing_filename)) => match batch.policy {
                    ConflictPolicy::Ask => {
                        batch.conflict = Some(AddConflict {
                            path,
                            existing_filename,
                        });
                    }
                    ConflictPolicy::ReplaceAll => add_incoming(archive, &mut batch, path),
                    ConflictPolicy::SkipAll => batch.skipped += 1,
                },
                Ok(None) => add_incoming(archive, &mut batch, path),
                Err(error) => batch.failures.push(format!("{}: {error}", path.display())),
            }
        }

        let changed = batch.added + batch.replaced > 0;
        self.dirty |= changed;
        if changed {
            self.image_preview = None;
            let selected_keys = batch.selected_keys.clone();
            self.restore_selection_by_keys(&selected_keys);
        }
        if batch.conflict.is_some() {
            self.status = format!(
                "Added {}, replaced {}, skipped {} — waiting for overwrite choice",
                batch.added, batch.replaced, batch.skipped
            );
            self.pending_add = Some(batch);
        } else {
            self.status = if canceled {
                format!(
                    "Import canceled — added {}, replaced {}, skipped {}",
                    batch.added, batch.replaced, batch.skipped
                )
            } else {
                format!(
                    "Added {}, replaced {}, skipped {} resources",
                    batch.added, batch.replaced, batch.skipped
                )
            };
            if !batch.failures.is_empty() {
                self.error = Some(batch.failures.join("\n"));
            }
        }
    }
    fn add_directory(&mut self) {
        let Some(path) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) => {
                self.fail("Could not read directory", error);
                return;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) if entry.path().is_file() => paths.push(entry.path()),
                Ok(_) => {}
                Err(error) => {
                    self.fail("Could not read a directory entry", error);
                    return;
                }
            }
        }
        paths.sort();
        if paths.is_empty() {
            self.status = format!("No files found in {}", path.display());
        } else {
            self.add_paths(paths);
        }
    }
    fn merge(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("NWN archives", &["hak", "erf", "mod", "sav"])
            .pick_file()
        else {
            return;
        };
        match Archive::open(&path) {
            Ok(other) => {
                let selected_keys = self.selected_resource_keys();
                let Some(current) = self.archive.as_mut() else {
                    return;
                };
                let (added, replaced) = current.merge(&other);
                self.dirty |= added + replaced > 0;
                if added + replaced > 0 {
                    self.image_preview = None;
                    self.restore_selection_by_keys(&selected_keys);
                }
                self.status = format!(
                    "Merged {}: {added} added, {replaced} replaced",
                    path.display()
                );
            }
            Err(e) => self.fail("Could not merge archive", e),
        }
    }
    fn export_selected(&mut self) {
        let Some(archive) = self.archive.as_ref() else {
            return;
        };
        if self.selected.is_empty() {
            return;
        }
        if self.selected.len() == 1 {
            let index = *self.selected.first().unwrap();
            let entry = &archive.entries[index];
            let filename = match entry.safe_filename() {
                Ok(filename) => filename,
                Err(error) => {
                    self.fail("Could not export resource", error);
                    return;
                }
            };
            if let Some(path) = rfd::FileDialog::new().set_file_name(filename).save_file() {
                match archive.export_entry(index, &path) {
                    Ok(()) => self.status = format!("Exported {}", path.display()),
                    Err(e) => self.fail("Could not export resource", e),
                }
            }
        } else if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            let mut count = 0;
            for index in self.selected.clone() {
                let entry = &archive.entries[index];
                let filename = match entry.safe_filename() {
                    Ok(filename) => filename,
                    Err(error) => {
                        self.fail("Could not export resources", error);
                        return;
                    }
                };
                if let Err(e) = archive.export_entry(index, dir.join(filename)) {
                    self.fail("Could not export resources", e);
                    return;
                }
                count += 1;
            }
            self.status = format!("Exported {count} resources to {}", dir.display());
        }
    }
    fn drag_selected(&mut self, frame: &eframe::Frame) {
        let Some(archive) = self.archive.as_ref() else {
            return;
        };
        if self.selected.is_empty() {
            return;
        }
        let directory = match tempfile::Builder::new().prefix("ahe-drag-").tempdir() {
            Ok(directory) => directory,
            Err(error) => {
                self.fail("Could not prepare resources for dragging", error);
                return;
            }
        };
        let mut paths = Vec::with_capacity(self.selected.len());
        for index in self.selected.iter().copied() {
            let Some(entry) = archive.entries.get(index) else {
                continue;
            };
            let filename = match entry.safe_filename() {
                Ok(filename) => filename,
                Err(error) => {
                    self.fail("Could not prepare resources for dragging", error);
                    return;
                }
            };
            let path = directory.path().join(filename);
            if let Err(error) = archive.export_entry(index, &path) {
                self.fail("Could not prepare resources for dragging", error);
                return;
            }
            paths.push(path);
        }
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        drag_out::release_pointer_grab(frame);
        drag_out::start(frame, paths, directory);
        self.status = format!("Dragging {count} resource(s) — drop them into a folder");
    }
    fn system_clipboard(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.clipboard.is_none() {
            self.clipboard = Some(arboard::Clipboard::new().map_err(|error| error.to_string())?);
        }
        Ok(self.clipboard.as_mut().unwrap())
    }
    fn copy_selected_to_clipboard(&mut self, cut: bool) {
        let Some(archive) = self.archive.as_ref() else {
            return;
        };
        if self.selected.is_empty() {
            return;
        }
        let directory = match tempfile::Builder::new().prefix("ahe-clipboard-").tempdir() {
            Ok(directory) => directory,
            Err(error) => {
                self.fail("Could not prepare clipboard resources", error);
                return;
            }
        };
        let mut paths = Vec::with_capacity(self.selected.len());
        for index in self.selected.iter().copied() {
            let Some(entry) = archive.entries.get(index) else {
                continue;
            };
            let filename = match entry.safe_filename() {
                Ok(filename) => filename,
                Err(error) => {
                    self.fail("Could not prepare clipboard resources", error);
                    return;
                }
            };
            let path = directory.path().join(filename);
            if let Err(error) = archive.export_entry(index, &path) {
                self.fail("Could not prepare clipboard resources", error);
                return;
            }
            paths.push(path);
        }
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        let result = self
            .system_clipboard()
            .and_then(|clipboard| clipboard.set().file_list(&paths).map_err(|e| e.to_string()));
        if let Err(error) = result {
            self.fail("Could not copy resources to the system clipboard", error);
            return;
        }
        // Clipboard file lists refer to these paths, so retain the newest
        // export while it is on the clipboard. Older exports are no longer
        // referenced and can be removed immediately.
        self.clipboard_exports.clear();
        self.clipboard_exports.push(directory);
        if cut {
            self.delete_selected();
            self.status =
                format!("Cut {count} resource(s) to the clipboard — save to commit their removal");
        } else {
            self.status = format!("Copied {count} resource(s) to the clipboard");
        }
    }
    fn paste_from_clipboard(&mut self) {
        if self.archive.is_none() {
            return;
        }
        let clipboard_paths = match self
            .system_clipboard()
            .and_then(|clipboard| clipboard.get().file_list().map_err(|e| e.to_string()))
        {
            Ok(paths) => paths,
            Err(error) => {
                self.fail("Could not read files from the system clipboard", error);
                return;
            }
        };
        if clipboard_paths.is_empty() {
            self.error = Some("The system clipboard does not contain any files.".to_owned());
            return;
        }
        let paths = sanitize_clipboard_paths(clipboard_paths)
            .into_iter()
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        if paths.is_empty() {
            self.error = Some("The files on the system clipboard no longer exist.".to_owned());
            return;
        }
        self.add_paths(paths);
    }
    fn extract_all(&mut self) {
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let Some(archive) = self.archive.as_ref() else {
            return;
        };
        match archive.extract_all(&dir) {
            Ok(n) => self.status = format!("Extracted {n} resources to {}", dir.display()),
            Err(e) => self.fail("Could not extract archive", e),
        }
    }
    fn delete_selected(&mut self) {
        let Some(archive) = self.archive.as_mut() else {
            return;
        };
        if self.selected.is_empty() {
            return;
        }
        let count = self.selected.len();
        archive.entries = archive
            .entries
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.selected.contains(i))
            .map(|(_, e)| e.clone())
            .collect();
        self.selected.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;
        self.category = None;
        self.dirty = true;
        self.status = format!("Removed {count} resources — save to commit changes");
    }
    fn new_archive(&mut self) {
        let archive = Archive::new(self.new_kind, self.new_version);
        self.sync_current_tab();
        self.tabs.push(TabState::new(archive, true));
        self.load_tab(self.tabs.len() - 1);
        self.show_new = false;
        self.status = "Created a new unsaved archive".into();
    }
    fn active_theme(&self, ctx: &egui::Context) -> egui::Theme {
        match self.appearance {
            Appearance::System => ctx.system_theme().unwrap_or(egui::Theme::Dark),
            Appearance::Dark => egui::Theme::Dark,
            Appearance::Light => egui::Theme::Light,
        }
    }
    fn title(&self) -> String {
        let name = self
            .archive
            .as_ref()
            .and_then(|a| a.path.as_ref())
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled");
        format!(
            "{}{} — Aurora Hak Explorer",
            if self.dirty { "*" } else { "" },
            name
        )
    }
}

impl eframe::App for HakEditor {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "compact_mode", &self.compact_mode);
        eframe::set_value(storage, "appearance", &self.appearance.storage_value());
        let recent_archives = self
            .recent_archives
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        eframe::set_value(storage, "recent_archives", &recent_archives);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let resource_context = !ctx.egui_wants_keyboard_input() && !self.blocking_dialog_open();
        if resource_context {
            let has_selection = !self.selected.is_empty();
            let has_archive = self.archive.is_some();
            raw_input.events.retain(|event| match event {
                egui::Event::Copy if has_selection => {
                    self.pending_clipboard_command = Some(ClipboardCommand::Copy);
                    false
                }
                egui::Event::Cut if has_selection => {
                    self.pending_clipboard_command = Some(ClipboardCommand::Cut);
                    false
                }
                egui::Event::Paste(_) if has_archive => {
                    self.pending_clipboard_command = Some(ClipboardCommand::Paste);
                    false
                }
                _ => true,
            });
        }

        // egui-winit consumes Ctrl+V before producing a key event. If the
        // clipboard contains files instead of text it produces no Paste event
        // either, so query the X11 key state while Ctrl is held.
        let (cut_down, paste_down) = if resource_context && raw_input.modifiers.command {
            shortcut_keys_down()
        } else {
            (false, false)
        };
        if resource_context && raw_input.modifiers.command {
            // Keep sampling briefly while Ctrl is held so a file-list paste is
            // caught even though egui-winit does not forward it as an event.
            ctx.request_repaint_after(Duration::from_millis(30));
        }
        if resource_context && !self.selected.is_empty() && cut_down && !self.cut_key_was_down {
            self.pending_clipboard_command = Some(ClipboardCommand::Cut);
        }
        if resource_context && self.archive.is_some() && paste_down && !self.paste_key_was_down {
            self.pending_clipboard_command = Some(ClipboardCommand::Paste);
        }
        self.cut_key_was_down = cut_down;
        self.paste_key_was_down = paste_down;
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.hovered_drop_files = ctx.input(|input| {
            input
                .raw
                .hovered_files
                .iter()
                .map(|file| {
                    file.path
                        .as_ref()
                        .and_then(|path| path.file_name())
                        .and_then(|name| name.to_str())
                        .unwrap_or("file")
                        .to_owned()
                })
                .collect()
        });
        let dropped: Vec<PathBuf> = ctx
            .input(|input| input.raw.dropped_files.clone())
            .into_iter()
            .filter_map(|file| file.path)
            .collect();
        if !dropped.is_empty() {
            self.pending_drop_files.extend(dropped);
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|input| input.viewport().close_requested()) && !self.force_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.request_quit();
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(self.title()));
        if !self.blocking_dialog_open() {
            self.capture_typeahead(&ctx);
        }
        if !self.blocking_dialog_open() && !self.pending_drop_files.is_empty() {
            let dropped = std::mem::take(&mut self.pending_drop_files);
            let (archives, resources): (Vec<_>, Vec<_>) =
                dropped.into_iter().partition(|path| is_archive_path(path));
            for path in archives {
                self.open_path(path);
            }
            if !resources.is_empty() {
                self.add_paths(resources);
            }
        }
        if !self.blocking_dialog_open() && !self.hovered_drop_files.is_empty() {
            egui::Area::new(egui::Id::new("file_drop_overlay"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .inner_margin(32.0)
                        .show(ui, |ui| {
                            ui.heading("Drop files to add to this archive");
                            ui.label(format!("{} file(s) ready", self.hovered_drop_files.len()));
                            for name in self.hovered_drop_files.iter().take(5) {
                                ui.label(name);
                            }
                        });
                });
        }
        let mut request_select_all = false;
        let mut request_copy = false;
        let mut request_cut = false;
        let mut request_paste = false;
        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New…   Ctrl+N").clicked() {
                        self.show_new = true;
                        ui.close();
                    }
                    if ui.button("Open…   Ctrl+O").clicked() {
                        self.open_dialog();
                        ui.close();
                    }
                    ui.separator();
                    let enabled = self.archive.is_some();
                    if ui
                        .add_enabled(enabled, egui::Button::new("Save   Ctrl+S"))
                        .clicked()
                    {
                        self.save(false);
                        ui.close();
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("Save As…"))
                        .clicked()
                    {
                        self.save(true);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(enabled, egui::Button::new("Extract all…"))
                        .clicked()
                    {
                        self.extract_all();
                        ui.close();
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("Close tab   Ctrl+W"))
                        .clicked()
                    {
                        if let Some(index) = self.active_tab {
                            if self.dirty {
                                self.confirm_close_tab = Some(index);
                            } else {
                                self.close_tab(index);
                            }
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        self.request_quit();
                        ui.close();
                    }
                });
                ui.menu_button("Edit", |ui| {
                    let has_selection = !self.selected.is_empty();
                    let has_archive = self.archive.is_some();
                    if ui
                        .add_enabled(has_selection, egui::Button::new("Copy   Ctrl+C"))
                        .clicked()
                    {
                        request_copy = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(has_selection, egui::Button::new("Cut   Ctrl+X"))
                        .clicked()
                    {
                        request_cut = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(has_archive, egui::Button::new("Paste   Ctrl+V"))
                        .clicked()
                    {
                        request_paste = true;
                        ui.close();
                    }
                });
                ui.menu_button("Archive", |ui| {
                    let enabled = self.archive.is_some();
                    if ui
                        .add_enabled(enabled, egui::Button::new("Select all   Ctrl+A"))
                        .clicked()
                    {
                        request_select_all = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(enabled, egui::Button::new("Add files…"))
                        .clicked()
                    {
                        self.add_files();
                        ui.close();
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("Add directory…"))
                        .clicked()
                    {
                        self.add_directory();
                        ui.close();
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("Merge archive…"))
                        .clicked()
                    {
                        self.merge();
                        ui.close();
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("Edit description…"))
                        .clicked()
                    {
                        self.description_buffer = self.archive.as_ref().unwrap().description();
                        self.show_description = true;
                        ui.close();
                    }
                });
                ui.menu_button("View", |ui| {
                    ui.label("Appearance");
                    if ui
                        .radio_value(&mut self.appearance, Appearance::System, "System")
                        .clicked()
                    {
                        ctx.set_theme(self.appearance.preference());
                        ui.close();
                    }
                    if ui
                        .radio_value(&mut self.appearance, Appearance::Dark, "Dark")
                        .clicked()
                    {
                        ctx.set_theme(self.appearance.preference());
                        ui.close();
                    }
                    if ui
                        .radio_value(&mut self.appearance, Appearance::Light, "Light")
                        .clicked()
                    {
                        ctx.set_theme(self.appearance.preference());
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .checkbox(&mut self.compact_mode, "Compact mode")
                        .on_hover_text(
                            "Hide the resource tree and details panes; always show every resource",
                        )
                        .clicked()
                    {
                        ui.close();
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("About Aurora Hak Explorer").clicked() {
                        self.show_about = true;
                        ui.close();
                    }
                });
            });
        });
        let mut requested_switch = None;
        let mut requested_close = None;
        let middle_click = ctx.input(|input| {
            input
                .pointer
                .button_clicked(egui::PointerButton::Middle)
                .then(|| input.pointer.hover_pos())
                .flatten()
        });
        egui::Panel::top("document_tabs").show(ui, |ui| {
            egui::ScrollArea::horizontal()
                .id_salt("open_archive_tabs")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (new_rect, new_response) =
                            ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::click());
                        if new_response.hovered() {
                            ui.painter().rect_filled(
                                new_rect,
                                4.0,
                                ui.visuals().widgets.hovered.bg_fill,
                            );
                        }
                        ui.painter().text(
                            new_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "+",
                            egui::FontId::proportional(18.0),
                            ui.visuals().strong_text_color(),
                        );
                        if new_response.on_hover_text("New archive").clicked() {
                            self.show_new = true;
                        }
                        ui.add_space(2.0);
                        if ui
                            .add_sized([52.0, 26.0], egui::Button::new("Open"))
                            .on_hover_text("Open archive (Ctrl+O)")
                            .clicked()
                        {
                            self.open_dialog();
                        }
                        ui.add_space(4.0);

                        for (index, tab_state) in self.tabs.iter().enumerate() {
                            let active = self.active_tab == Some(index);
                            let dirty = if active { self.dirty } else { tab_state.dirty };
                            let label =
                                format!("{}{}", if dirty { "* " } else { "" }, tab_state.label());
                            let selection = ui.visuals().selection.bg_fill;
                            let tab = egui::Frame::new()
                                .fill(if active {
                                    selection.gamma_multiply(0.28)
                                } else {
                                    Color32::TRANSPARENT
                                })
                                .stroke(if active {
                                    egui::Stroke::new(1.0, selection.gamma_multiply(0.55))
                                } else {
                                    egui::Stroke::NONE
                                })
                                .corner_radius(4)
                                .inner_margin(egui::Margin::symmetric(8, 3))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let title = ui.add(
                                            egui::Label::new(RichText::new(label).color(
                                                if active {
                                                    ui.visuals().strong_text_color()
                                                } else {
                                                    ui.visuals().text_color()
                                                },
                                            ))
                                            .selectable(false)
                                            .sense(egui::Sense::click()),
                                        );
                                        let (close_rect, close) = ui.allocate_exact_size(
                                            egui::vec2(18.0, 18.0),
                                            egui::Sense::click(),
                                        );
                                        if close.hovered() {
                                            ui.painter().rect_filled(
                                                close_rect,
                                                4.0,
                                                selection.gamma_multiply(0.72),
                                            );
                                        }
                                        ui.painter().text(
                                            close_rect.center(),
                                            egui::Align2::CENTER_CENTER,
                                            "×",
                                            egui::FontId::proportional(14.0),
                                            ui.visuals().strong_text_color(),
                                        );
                                        (title, close.on_hover_text("Close tab"))
                                    })
                                    .inner
                                });
                            if active {
                                ui.painter().line_segment(
                                    [
                                        egui::pos2(
                                            tab.response.rect.left() + 4.0,
                                            tab.response.rect.bottom(),
                                        ),
                                        egui::pos2(
                                            tab.response.rect.right() - 4.0,
                                            tab.response.rect.bottom(),
                                        ),
                                    ],
                                    egui::Stroke::new(2.0, selection),
                                );
                            } else if tab.response.hovered() {
                                ui.painter().rect_stroke(
                                    tab.response.rect,
                                    4.0,
                                    egui::Stroke::new(1.0, selection.gamma_multiply(0.75)),
                                    egui::StrokeKind::Inside,
                                );
                            }
                            tab.inner.0.context_menu(|ui| {
                                if ui.button("Close tab").clicked() {
                                    requested_close = Some(index);
                                    ui.close();
                                }
                            });
                            if tab.inner.0.clicked() {
                                requested_switch = Some(index);
                            }
                            if tab.inner.1.clicked() {
                                requested_close = Some(index);
                            }
                            if middle_click
                                .is_some_and(|position| tab.response.rect.contains(position))
                            {
                                requested_close = Some(index);
                            }
                            ui.add_space(2.0);
                        }
                        if self.tabs.is_empty() {
                            ui.label(RichText::new("No archives open").weak());
                        }
                    });
                });
        });
        if !self.blocking_dialog_open()
            && let Some(index) = requested_close
        {
            let dirty = if self.active_tab == Some(index) {
                self.dirty
            } else {
                self.tabs.get(index).is_some_and(|tab| tab.dirty)
            };
            if dirty {
                self.confirm_close_tab = Some(index);
            } else {
                self.close_tab(index);
            }
        } else if !self.blocking_dialog_open()
            && let Some(index) = requested_switch
        {
            self.switch_tab(index);
        }
        let global_shortcuts_enabled =
            !ctx.egui_wants_keyboard_input() && !self.blocking_dialog_open();
        if global_shortcuts_enabled {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::O)) {
                self.open_dialog();
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::S)) {
                self.save(false);
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::N)) {
                self.show_new = true;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A)) {
                request_select_all = true;
            }
        }
        let pending_clipboard_command = if global_shortcuts_enabled {
            self.pending_clipboard_command.take()
        } else {
            self.pending_clipboard_command = None;
            None
        };
        if let Some(command) = pending_clipboard_command {
            match command {
                ClipboardCommand::Copy => request_copy = true,
                ClipboardCommand::Cut => request_cut = true,
                ClipboardCommand::Paste => request_paste = true,
            }
        }
        let clipboard_shortcuts_enabled = global_shortcuts_enabled;
        if clipboard_shortcuts_enabled {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::C)) {
                request_copy = true;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::X)) {
                request_cut = true;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::V)) {
                request_paste = true;
            }
        }
        if request_copy {
            self.copy_selected_to_clipboard(false);
        } else if request_cut {
            self.copy_selected_to_clipboard(true);
        } else if request_paste {
            self.paste_from_clipboard();
        }
        if global_shortcuts_enabled {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::W))
                && let Some(index) = self.active_tab
            {
                if self.dirty {
                    self.confirm_close_tab = Some(index);
                } else {
                    self.close_tab(index);
                }
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Delete)) {
                self.delete_selected();
            }
        }
        let keyboard_navigation = if global_shortcuts_enabled {
            let ctrl_shift = egui::Modifiers {
                ctrl: true,
                shift: true,
                ..Default::default()
            };
            if ctx.input_mut(|i| i.consume_key(ctrl_shift, egui::Key::ArrowUp)) {
                Some((-1_i8, true, true))
            } else if ctx.input_mut(|i| i.consume_key(ctrl_shift, egui::Key::ArrowDown)) {
                Some((1_i8, true, true))
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowUp)) {
                Some((-1_i8, true, false))
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowDown))
            {
                Some((1_i8, true, false))
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::ArrowUp)) {
                Some((-1_i8, false, true))
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::ArrowDown))
            {
                Some((1_i8, false, true))
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                Some((-1_i8, false, false))
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown))
            {
                Some((1_i8, false, false))
            } else {
                None
            }
        } else {
            None
        };
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.status).color(Color32::from_rgb(160, 180, 200)));
                if self.dirty {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new("Unsaved changes").color(Color32::from_rgb(240, 180, 70)),
                        );
                    });
                }
            });
        });
        let mut categories = BTreeMap::<String, usize>::new();
        let new_count = self
            .archive
            .as_ref()
            .map(|archive| {
                archive
                    .entries
                    .iter()
                    .filter(|entry| entry.is_new())
                    .count()
            })
            .unwrap_or(0);
        let archive_info = self.archive.as_ref().map(|a| {
            for entry in &a.entries {
                *categories
                    .entry(category_for(&entry.extension()).to_owned())
                    .or_default() += 1;
            }
            let bytes = a
                .entries
                .iter()
                .filter_map(|entry| entry.size().ok())
                .sum::<u64>();
            let name = a
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("Untitled")
                .to_owned();
            (
                name,
                a.entries.len(),
                bytes,
                a.version.label(),
                String::from_utf8_lossy(a.kind.signature())
                    .trim()
                    .to_owned(),
            )
        });
        let selected_entry = self
            .selected
            .first()
            .and_then(|index| self.archive.as_ref()?.entries.get(*index))
            .cloned();

        if !self.compact_mode
            && let Some((name, count, bytes, version, kind)) = archive_info.clone()
        {
            egui::Panel::left("resource_tree")
                .default_size(215.0)
                .resizable(true)
                .show(ui, |ui| {
                    ui.label(RichText::new("Resource Tree").size(17.0).strong());
                    ui.separator();
                    let root_active = self.category.is_none();
                    if resource_tree_row(ui, root_active, &name, count).clicked() {
                        self.category = None;
                    }
                    ui.indent("categories", |ui| {
                        let active = self.category.as_deref() == Some("New");
                        if resource_tree_row(ui, active, "New", new_count).clicked() {
                            self.category = Some("New".to_owned());
                            self.selected.clear();
                            self.selection_anchor = None;
                            self.selection_cursor = None;
                        }
                        for (category, amount) in &categories {
                            let active = self.category.as_deref() == Some(category);
                            if resource_tree_row(ui, active, category, *amount).clicked() {
                                self.category = Some(category.clone());
                                self.selected.clear();
                                self.selection_anchor = None;
                                self.selection_cursor = None;
                            }
                        }
                    });
                    ui.add_space(18.0);
                    ui.group(|ui| {
                        ui.strong("HAK Info");
                        ui.separator();
                        egui::Grid::new("hak_info").show(ui, |ui| {
                            ui.label("Name:");
                            ui.label(name);
                            ui.end_row();
                            ui.label("Type:");
                            ui.label(format!("{kind} (ERF)"));
                            ui.end_row();
                            ui.label("Version:");
                            ui.label(version);
                            ui.end_row();
                            ui.label("Resources:");
                            ui.label(count.to_string());
                            ui.end_row();
                            ui.label("Payload:");
                            ui.label(human_size(bytes));
                            ui.end_row();
                        });
                    });
                });
            egui::Panel::right("preview")
                .default_size(370.0)
                .resizable(true)
                .show(ui, |ui| {
                    ui.heading("Details");
                    ui.separator();
                    if let Some(entry) = &selected_entry {
                        let entry_size = entry.size().unwrap_or(0);
                        ui.heading(entry.filename());
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("Type: {}", entry.extension().to_ascii_uppercase()));
                            ui.separator();
                            ui.label(format!(
                                "Size: {}",
                                if entry_size == 0 {
                                    "?".into()
                                } else {
                                    human_size(entry_size)
                                }
                            ));
                            ui.separator();
                            ui.label(format!("Type ID: 0x{:04x}", entry.type_id));
                        });
                        ui.add_space(8.0);
                        ui.group(|ui| {
                            ui.set_min_height(300.0);
                            let extension = entry.extension();
                            if is_previewable_image(&extension) {
                                self.show_image_preview(ui, entry);
                            } else if extension == "mdl" {
                                self.show_model_preview(ui, entry);
                            } else {
                                match entry.read_prefix(256 * 1024) {
                                    Ok(bytes) if extension == "2da" => show_2da_preview(ui, &bytes),
                                    Ok(bytes) if extension == "bmu" => {
                                        show_bmu_preview(ui, &bytes, entry_size)
                                    }
                                    Ok(bytes) if extension == "wav" => {
                                        show_wav_resource_preview(ui, &bytes, entry_size)
                                    }
                                    Ok(bytes) if is_text_type(&extension) => {
                                        let text = String::from_utf8_lossy(&bytes);
                                        egui::ScrollArea::both().auto_shrink(false).show(
                                            ui,
                                            |ui| {
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(text).monospace(),
                                                    )
                                                    .selectable(true),
                                                );
                                            },
                                        );
                                    }
                                    Ok(bytes) => {
                                        ui.vertical_centered(|ui| {
                                            ui.add_space(90.0);
                                            ui.heading("Binary resource");
                                            ui.label(format!(
                                                "{} bytes loaded for inspection",
                                                bytes.len()
                                            ));
                                            ui.label(
                                                "Extract it to open it in a specialized editor.",
                                            );
                                        });
                                    }
                                    Err(error) => {
                                        ui.colored_label(Color32::LIGHT_RED, error.to_string());
                                    }
                                }
                            }
                        });
                        ui.add_space(8.0);
                        ui.group(|ui| {
                            ui.strong("Resource Actions");
                            ui.separator();
                            if ui.button("Extract this resource to disk").clicked() {
                                self.export_selected();
                            }
                            if ui.button("Remove from archive").clicked() {
                                self.delete_selected();
                            }
                        });
                    } else {
                        ui.vertical_centered(|ui| {
                            ui.add_space(120.0);
                            ui.label("Select a resource to preview it");
                        });
                    }
                });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some((name, _, _, _, _)) = &archive_info {
                    ui.strong(name);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("Search resources…")
                            .desired_width(260.0),
                    );
                });
            });
            ui.separator();
            if let Some(a) = self.archive.as_ref() {
                let filter = self.filter.to_ascii_lowercase();
                // Compact mode deliberately ignores the tree's active category.  The tree is
                // hidden in this mode, so leaving a prior selection such as "New" active
                // must not make the main list look as if resources have disappeared.
                let category = (!self.compact_mode)
                    .then(|| self.category.clone())
                    .flatten();
                let mut visible_indices: Vec<usize> = a
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|entry| {
                        let ext = entry.1.extension();
                        (filter.is_empty()
                            || entry.1.name.to_ascii_lowercase().contains(&filter)
                            || ext.contains(&filter))
                            && entry_matches_category(entry.1, category.as_deref())
                    })
                    .map(|(index, _)| index)
                    .collect();
                visible_indices.sort_by(|left, right| {
                    let left_entry = &a.entries[*left];
                    let right_entry = &a.entries[*right];
                    let ordering = match self.sort_column {
                        SortColumn::Name => left_entry
                            .name
                            .to_ascii_lowercase()
                            .cmp(&right_entry.name.to_ascii_lowercase()),
                        SortColumn::Type => left_entry.extension().cmp(&right_entry.extension()),
                        SortColumn::Size => left_entry
                            .size()
                            .unwrap_or(0)
                            .cmp(&right_entry.size().unwrap_or(0)),
                    }
                    .then_with(|| {
                        left_entry
                            .name
                            .to_ascii_lowercase()
                            .cmp(&right_entry.name.to_ascii_lowercase())
                    });
                    if self.sort_ascending {
                        ordering
                    } else {
                        ordering.reverse()
                    }
                });
                if request_select_all {
                    self.selected = visible_indices.iter().copied().collect();
                    self.selection_anchor = visible_indices.first().copied();
                    self.selection_cursor = visible_indices.first().copied();
                    self.status = format!("Selected {} resources", self.selected.len());
                }
                let jump_target = if self.typeahead_pending {
                    self.typeahead_pending = false;
                    visible_indices.iter().copied().find(|index| {
                        a.entries[*index]
                            .name
                            .to_ascii_lowercase()
                            .starts_with(&self.typeahead)
                    })
                } else {
                    None
                };
                if let Some(index) = jump_target {
                    self.selected.clear();
                    self.selected.insert(index);
                    self.selection_anchor = Some(index);
                    self.selection_cursor = Some(index);
                }
                let keyboard_target = keyboard_navigation.and_then(|(direction, _, _)| {
                    if visible_indices.is_empty() {
                        return None;
                    }
                    let current = self
                        .selection_cursor
                        .or(self.selection_anchor)
                        .or_else(|| self.selected.first().copied())
                        .and_then(|selected| {
                            visible_indices.iter().position(|index| *index == selected)
                        });
                    let position = match (current, direction) {
                        (Some(position), -1) => position.saturating_sub(1),
                        (Some(position), _) => (position + 1).min(visible_indices.len() - 1),
                        (None, -1) => visible_indices.len() - 1,
                        (None, _) => 0,
                    };
                    Some(visible_indices[position])
                });
                if let Some(index) = keyboard_target {
                    let (_, extend_range, additive) = keyboard_navigation.unwrap();
                    if extend_range {
                        let anchor = self
                            .selection_anchor
                            .and_then(|anchor| {
                                visible_indices.iter().position(|item| *item == anchor)
                            })
                            .or_else(|| {
                                self.selection_cursor.and_then(|cursor| {
                                    visible_indices.iter().position(|item| *item == cursor)
                                })
                            })
                            .unwrap_or_else(|| {
                                visible_indices
                                    .iter()
                                    .position(|item| *item == index)
                                    .unwrap()
                            });
                        let end = visible_indices
                            .iter()
                            .position(|item| *item == index)
                            .unwrap();
                        if self.selection_anchor.is_none()
                            || !visible_indices.contains(&self.selection_anchor.unwrap())
                        {
                            self.selection_anchor = Some(visible_indices[anchor]);
                        }
                        if !additive {
                            self.selected.clear();
                        }
                        let (start, end) = if anchor <= end {
                            (anchor, end)
                        } else {
                            (end, anchor)
                        };
                        self.selected
                            .extend(visible_indices[start..=end].iter().copied());
                    } else if additive {
                        self.selected.insert(index);
                        if self.selection_anchor.is_none() {
                            self.selection_anchor = Some(index);
                        }
                    } else {
                        self.selected.clear();
                        self.selected.insert(index);
                        self.selection_anchor = Some(index);
                    }
                    self.selection_cursor = Some(index);
                    self.typeahead.clear();
                    self.typeahead_pending = false;
                    self.status = format!("Selected {} resources", self.selected.len());
                }
                ui.heading(format!("Resources ({})", visible_indices.len()));
                if !self.typeahead.is_empty()
                    && self
                        .typeahead_last
                        .is_some_and(|last| last.elapsed() <= Duration::from_millis(1200))
                {
                    ui.label(
                        RichText::new(format!("Jump: {}", self.typeahead))
                            .monospace()
                            .color(Color32::from_rgb(120, 190, 240)),
                    );
                }
                ui.separator();
                let mut request_delete = false;
                let mut request_export = false;
                let mut request_drag = None;
                let resource_scroll = egui::ScrollArea::vertical()
                    .id_salt("resource_entries")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        egui::Grid::new("entries")
                            .striped(true)
                            .min_col_width(100.0)
                            .show(ui, |ui| {
                                if sort_button(
                                    ui,
                                    "Name",
                                    SortColumn::Name,
                                    self.sort_column,
                                    self.sort_ascending,
                                ) {
                                    set_sort(
                                        &mut self.sort_column,
                                        &mut self.sort_ascending,
                                        SortColumn::Name,
                                    );
                                }
                                if sort_button(
                                    ui,
                                    "Type",
                                    SortColumn::Type,
                                    self.sort_column,
                                    self.sort_ascending,
                                ) {
                                    set_sort(
                                        &mut self.sort_column,
                                        &mut self.sort_ascending,
                                        SortColumn::Type,
                                    );
                                }
                                if sort_button(
                                    ui,
                                    "Size",
                                    SortColumn::Size,
                                    self.sort_column,
                                    self.sort_ascending,
                                ) {
                                    set_sort(
                                        &mut self.sort_column,
                                        &mut self.sort_ascending,
                                        SortColumn::Size,
                                    );
                                }
                                ui.end_row();
                                for index in visible_indices.iter().copied() {
                                    let entry = &a.entries[index];
                                    let ext = entry.extension();
                                    let selected = self.selected.contains(&index);
                                    let response = ui.add(
                                        egui::Button::selectable(selected, entry.filename())
                                            .sense(egui::Sense::click_and_drag()),
                                    );
                                    if jump_target == Some(index) || keyboard_target == Some(index)
                                    {
                                        response.scroll_to_me(Some(egui::Align::Center));
                                    }
                                    if response.clicked() {
                                        self.typeahead.clear();
                                        self.typeahead_pending = false;
                                        let modifiers = ctx.input(|i| i.modifiers);
                                        if modifiers.shift {
                                            if !modifiers.ctrl {
                                                self.selected.clear();
                                            }
                                            if let Some(anchor) = self.selection_anchor {
                                                if let (Some(start), Some(end)) = (
                                                    visible_indices
                                                        .iter()
                                                        .position(|item| *item == anchor),
                                                    visible_indices
                                                        .iter()
                                                        .position(|item| *item == index),
                                                ) {
                                                    let (start, end) = if start <= end {
                                                        (start, end)
                                                    } else {
                                                        (end, start)
                                                    };
                                                    self.selected.extend(
                                                        visible_indices[start..=end]
                                                            .iter()
                                                            .copied(),
                                                    );
                                                } else {
                                                    self.selected.insert(index);
                                                }
                                            } else {
                                                self.selected.insert(index);
                                                self.selection_anchor = Some(index);
                                            }
                                            self.selection_cursor = Some(index);
                                        } else if modifiers.ctrl {
                                            if selected {
                                                self.selected.remove(&index);
                                            } else {
                                                self.selected.insert(index);
                                            }
                                            self.selection_anchor = Some(index);
                                            self.selection_cursor = Some(index);
                                        } else {
                                            self.selected.clear();
                                            self.selected.insert(index);
                                            self.selection_anchor = Some(index);
                                            self.selection_cursor = Some(index);
                                        }
                                    }
                                    if response.secondary_clicked() && !selected {
                                        self.selected.clear();
                                        self.selected.insert(index);
                                        self.selection_anchor = Some(index);
                                        self.selection_cursor = Some(index);
                                    }
                                    if response.drag_started_by(egui::PointerButton::Primary) {
                                        if !selected {
                                            self.selected.clear();
                                            self.selected.insert(index);
                                            self.selection_anchor = Some(index);
                                            self.selection_cursor = Some(index);
                                        }
                                        request_drag = Some(index);
                                    }
                                    response.context_menu(|ui| {
                                        let count = self.selected.len().max(1);
                                        ui.label(
                                            RichText::new(if count == 1 {
                                                entry.filename()
                                            } else {
                                                format!("{count} selected resources")
                                            })
                                            .strong(),
                                        );
                                        ui.separator();
                                        if ui.button("Export selected…").clicked() {
                                            request_export = true;
                                            ui.close();
                                        }
                                        if ui.button(format!("Delete selected ({count})")).clicked()
                                        {
                                            request_delete = true;
                                            ui.close();
                                        }
                                    });
                                    ui.label(ext.to_ascii_uppercase());
                                    ui.label(
                                        entry.size().map(human_size).unwrap_or_else(|_| "?".into()),
                                    );
                                    ui.end_row();
                                }
                            });
                    });
                let (middle_pressed, middle_down, pointer_position, pointer_delta) =
                    ctx.input(|input| {
                        (
                            input.pointer.button_pressed(egui::PointerButton::Middle),
                            input.pointer.middle_down(),
                            input.pointer.hover_pos(),
                            input.pointer.delta(),
                        )
                    });
                if middle_pressed
                    && pointer_position
                        .is_some_and(|position| resource_scroll.inner_rect.contains(position))
                {
                    self.resource_middle_scroll_active = true;
                }
                if self.resource_middle_scroll_active && middle_down {
                    let mut state = resource_scroll.state;
                    let maximum_offset = (resource_scroll.content_size.y
                        - resource_scroll.inner_rect.height())
                    .max(0.0);
                    state.offset.y = (state.offset.y + pointer_delta.y).clamp(0.0, maximum_offset);
                    state.store(&ctx, resource_scroll.id);
                    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                    ctx.request_repaint();
                } else if !middle_down {
                    self.resource_middle_scroll_active = false;
                }
                if request_drag.is_some() {
                    self.drag_selected(frame);
                } else if request_export {
                    self.export_selected();
                } else if request_delete {
                    self.delete_selected();
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Extract…").clicked() {
                        self.export_selected();
                    }
                    if ui.button("Extract All…").clicked() {
                        self.extract_all();
                    }
                    if ui.button("Add…").clicked() {
                        self.add_files();
                    }
                    if ui
                        .add_enabled(!self.selected.is_empty(), egui::Button::new("Delete"))
                        .clicked()
                    {
                        self.delete_selected();
                    }
                });
            } else {
                egui::ScrollArea::vertical()
                    .id_salt("welcome_screen")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(72.0);
                            ui.heading("Aurora Hak Explorer");
                            ui.label("AHE");
                            ui.label("A native editor for Neverwinter Nights HAK and ERF archives");
                            ui.add_space(16.0);
                            if ui
                                .add_sized(
                                    [200.0, 34.0],
                                    egui::Button::new(
                                        RichText::new("+  Open an archive").size(14.0),
                                    ),
                                )
                                .clicked()
                            {
                                self.open_dialog();
                            }
                            if ui
                                .add_sized(
                                    [200.0, 34.0],
                                    egui::Button::new(
                                        RichText::new("+  Create a new archive").size(14.0),
                                    ),
                                )
                                .clicked()
                            {
                                self.show_new = true;
                            }
                            ui.add_space(10.0);
                            ui.label("You can also drop a .hak, .erf, .mod, or .sav file here.");
                            if !self.recent_archives.is_empty() {
                                ui.add_space(24.0);
                                ui.label(RichText::new("Recently opened").size(16.0).strong());
                                ui.add_space(4.0);
                                let mut requested_recent = None;
                                for path in self.recent_archives.clone() {
                                    let filename = path
                                        .file_name()
                                        .map(|name| name.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| path.display().to_string());
                                    let directory = path
                                        .parent()
                                        .map(|parent| parent.display().to_string())
                                        .unwrap_or_default();
                                    if ui
                                        .add_sized(
                                            [320.0, 34.0],
                                            egui::Button::new(RichText::new(filename).size(14.0)),
                                        )
                                        .on_hover_text(path.display().to_string())
                                        .clicked()
                                    {
                                        requested_recent = Some(path);
                                    }
                                    ui.label(RichText::new(directory).size(10.5).weak());
                                    ui.add_space(3.0);
                                }
                                if ui
                                    .add_sized(
                                        [150.0, 32.0],
                                        egui::Button::new(
                                            RichText::new("Clear recent files").size(13.0),
                                        ),
                                    )
                                    .clicked()
                                {
                                    self.recent_archives.clear();
                                } else if let Some(path) = requested_recent {
                                    if path.is_file() {
                                        self.open_path(path);
                                    } else {
                                        self.recent_archives.retain(|recent| recent != &path);
                                        self.status = format!(
                                            "Removed missing recent archive: {}",
                                            path.display()
                                        );
                                    }
                                }
                            }
                        });
                    });
            }
        });
        if self.show_new {
            let mut create = false;
            let mut cancel = false;
            let theme = self.active_theme(&ctx);
            let modal = egui::Modal::new(egui::Id::new("new_archive_modal"))
                .frame(egui::Frame::popup(&ctx.style_of(theme)).inner_margin(22.0))
                .show(&ctx, |ui| {
                    ui.set_min_width(500.0);
                    ui.label(RichText::new("New archive").size(22.0).strong());
                    ui.separator();
                    ui.label("Archive type");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.new_kind, ArchiveKind::Hak, "HAK");
                        ui.selectable_value(&mut self.new_kind, ArchiveKind::Erf, "ERF");
                        ui.selectable_value(&mut self.new_kind, ArchiveKind::Mod, "MOD");
                        ui.selectable_value(&mut self.new_kind, ArchiveKind::Sav, "SAV");
                    });
                    ui.label("Format version");
                    ui.radio_value(
                        &mut self.new_version,
                        ArchiveVersion::V1_0,
                        "V1.0 — Neverwinter Nights / Enhanced Edition",
                    );
                    ui.radio_value(
                        &mut self.new_version,
                        ArchiveVersion::V1_1,
                        "V1.1 — Neverwinter Nights 2",
                    );
                    ui.separator();
                    ui.horizontal_centered(|ui| {
                        if ui.button("Create").clicked() {
                            create = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            if modal.should_close() {
                cancel = true;
            }
            if create {
                self.new_archive();
            } else if cancel {
                self.show_new = false;
            }
        }
        if self.show_description {
            let mut apply = false;
            let mut cancel = false;
            let theme = self.active_theme(&ctx);
            let modal = egui::Modal::new(egui::Id::new("archive_description_modal"))
                .frame(egui::Frame::popup(&ctx.style_of(theme)).inner_margin(22.0))
                .show(&ctx, |ui| {
                    ui.set_min_width(520.0);
                    ui.label(RichText::new("Archive description").size(22.0).strong());
                    ui.separator();
                    ui.add(
                        egui::TextEdit::multiline(&mut self.description_buffer)
                            .desired_rows(8)
                            .desired_width(480.0),
                    );
                    ui.horizontal_centered(|ui| {
                        if ui.button("Apply").clicked() {
                            apply = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            if modal.should_close() {
                cancel = true;
            }
            if apply {
                if let Some(archive) = self.archive.as_mut() {
                    archive.set_description(self.description_buffer.clone());
                    self.dirty = true;
                }
                self.show_description = false;
            } else if cancel {
                self.show_description = false;
            }
        }
        if self.show_about {
            let mut close = false;
            let theme = self.active_theme(&ctx);
            let modal = egui::Modal::new(egui::Id::new("about_modal"))
                .frame(egui::Frame::popup(&ctx.style_of(theme)).inner_margin(26.0))
                .show(&ctx, |ui| {
                    ui.set_min_width(480.0);
                    ui.spacing_mut().item_spacing.y = 11.0;
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Aurora Hak Explorer (AHE) {}",
                                env!("CARGO_PKG_VERSION")
                            ))
                            .size(23.0)
                            .strong(),
                        );
                        ui.label(
                            RichText::new(
                                "Explore and edit Neverwinter Nights resource archives.",
                            )
                            .size(17.0),
                        );
                        ui.label(
                            RichText::new(
                                "Create, inspect, import, extract, and safely rewrite HAK, ERF, MOD, and SAV files on Linux and Windows.",
                            )
                            .size(15.0)
                            .weak(),
                        );
                        ui.add_space(5.0);
                        ui.label("Copyright © 2026 Winternite");
                        ui.hyperlink_to(
                            "GNU GPL v3 or later",
                            "https://www.gnu.org/licenses/gpl-3.0.html",
                        );
                        ui.add_space(7.0);
                        if ui
                            .add_sized(
                                [120.0, 36.0],
                                egui::Button::new(RichText::new("Close").size(16.0)),
                            )
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
            if modal.should_close() || close {
                self.show_about = false;
            }
        }
        let conflict_details = self.pending_add.as_ref().and_then(|batch| {
            batch.conflict.as_ref().map(|conflict| {
                (
                    conflict.path.clone(),
                    conflict.existing_filename.clone(),
                    batch.queue.len(),
                )
            })
        });
        if let Some((path, existing_filename, remaining)) = conflict_details {
            let incoming_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("incoming resource")
                .to_owned();
            let mut action = None;
            let theme = self.active_theme(&ctx);
            let modal = egui::Modal::new(egui::Id::new("resource_conflict_modal"))
                .frame(egui::Frame::popup(&ctx.style_of(theme)).inner_margin(22.0))
                .show(&ctx, |ui| {
                    ui.set_min_width(520.0);
                    ui.spacing_mut().item_spacing.y = 11.0;
                    ui.label(RichText::new("Resource already exists").size(22.0).strong());
                    ui.separator();
                    ui.label(
                        RichText::new(format!(
                            "{incoming_name} would overwrite {existing_filename}."
                        ))
                        .size(17.0),
                    );
                    if remaining > 0 {
                        ui.label(format!(
                            "There {} {remaining} more file{} in this import.",
                            if remaining == 1 { "is" } else { "are" },
                            if remaining == 1 { "" } else { "s" }
                        ));
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_sized([105.0, 36.0], egui::Button::new("Replace"))
                            .clicked()
                        {
                            action = Some(ConflictAction::Replace);
                        }
                        if ui
                            .add_sized([105.0, 36.0], egui::Button::new("Skip"))
                            .clicked()
                        {
                            action = Some(ConflictAction::Skip);
                        }
                        if ui
                            .add_sized([125.0, 36.0], egui::Button::new("Replace All"))
                            .clicked()
                        {
                            action = Some(ConflictAction::ReplaceAll);
                        }
                        if ui
                            .add_sized([105.0, 36.0], egui::Button::new("Skip All"))
                            .clicked()
                        {
                            action = Some(ConflictAction::SkipAll);
                        }
                    });
                    if ui
                        .add_sized([105.0, 34.0], egui::Button::new("Cancel Import"))
                        .clicked()
                    {
                        action = Some(ConflictAction::Cancel);
                    }
                });
            if modal.should_close() {
                action = Some(ConflictAction::Cancel);
            }
            if let Some(action) = action {
                self.process_add_batch(action);
            }
        }
        if let Some(index) = self.confirm_close_tab {
            let label = if self.active_tab == Some(index) {
                self.archive
                    .as_ref()
                    .and_then(|archive| archive.path.as_ref())
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("Untitled")
                    .to_owned()
            } else {
                self.tabs
                    .get(index)
                    .map(TabState::label)
                    .unwrap_or_else(|| "Untitled".to_owned())
            };
            let mut save = false;
            let mut discard = false;
            let mut cancel = false;
            let theme = self.active_theme(&ctx);
            let modal = egui::Modal::new(egui::Id::new("unsaved_changes_modal"))
                .frame(egui::Frame::popup(&ctx.style_of(theme)).inner_margin(22.0))
                .show(&ctx, |ui| {
                    ui.set_min_width(450.0);
                    ui.spacing_mut().item_spacing.y = 12.0;
                    ui.label(RichText::new("Unsaved changes").size(22.0).strong());
                    ui.separator();
                    ui.label(
                        RichText::new(format!("{label} has unsaved changes."))
                            .size(17.0)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(if self.quit_in_progress {
                            "Save your changes before quitting?"
                        } else {
                            "Save your changes before closing this tab?"
                        })
                        .size(17.0),
                    );
                    ui.add_space(6.0);
                    let button_width = 120.0;
                    let button_spacing = ui.spacing().item_spacing.x;
                    let button_row_width = button_width * 3.0 + button_spacing * 2.0;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - button_row_width) / 2.0).max(0.0));
                        if ui
                            .add_sized(
                                [button_width, 38.0],
                                egui::Button::new(RichText::new("Save").size(16.0)),
                            )
                            .clicked()
                        {
                            save = true;
                        }
                        if ui
                            .add_sized(
                                [button_width, 38.0],
                                egui::Button::new(RichText::new("Discard").size(16.0)),
                            )
                            .clicked()
                        {
                            discard = true;
                        }
                        if ui
                            .add_sized(
                                [button_width, 38.0],
                                egui::Button::new(RichText::new("Cancel").size(16.0)),
                            )
                            .clicked()
                        {
                            cancel = true;
                        }
                    });
                });
            if modal.should_close() {
                cancel = true;
            }
            if save {
                self.switch_tab(index);
                self.save(false);
                if !self.dirty {
                    self.confirm_close_tab = None;
                    if self.quit_in_progress {
                        self.continue_quit();
                    } else {
                        self.close_tab(index);
                    }
                }
            } else if discard {
                self.confirm_close_tab = None;
                self.close_tab(index);
                if self.quit_in_progress {
                    self.continue_quit();
                }
            } else if cancel {
                self.confirm_close_tab = None;
                self.quit_in_progress = false;
            }
        }
        if let Some(message) = self.error.clone() {
            let mut close = false;
            let theme = self.active_theme(&ctx);
            let modal = egui::Modal::new(egui::Id::new("error_modal"))
                .frame(egui::Frame::popup(&ctx.style_of(theme)).inner_margin(24.0))
                .show(&ctx, |ui| {
                    ui.set_min_width(500.0);
                    ui.set_max_width(650.0);
                    ui.spacing_mut().item_spacing.y = 14.0;
                    ui.label(
                        RichText::new("Error")
                            .size(22.0)
                            .strong()
                            .color(Color32::from_rgb(255, 130, 130)),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(message)
                            .size(17.0)
                            .color(Color32::from_rgb(255, 150, 150)),
                    );
                    ui.add_space(6.0);
                    if ui
                        .add_sized(
                            [120.0, 38.0],
                            egui::Button::new(RichText::new("Close").size(16.0)),
                        )
                        .clicked()
                    {
                        close = true;
                    }
                });
            if modal.should_close() || close {
                self.error = None;
            }
        }
        if self.force_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn add_incoming(archive: &mut Archive, batch: &mut AddBatch, path: PathBuf) {
    match archive.add_file(&path) {
        Ok(true) => batch.replaced += 1,
        Ok(false) => batch.added += 1,
        Err(error) => batch.failures.push(format!("{}: {error}", path.display())),
    }
}

#[cfg(target_os = "linux")]
fn shortcut_keys_down() -> (bool, bool) {
    let Ok(xlib) = x11_dl::xlib::Xlib::open() else {
        return (false, false);
    };
    unsafe {
        let display = (xlib.XOpenDisplay)(std::ptr::null());
        if display.is_null() {
            return (false, false);
        }
        let cut_keycode = (xlib.XKeysymToKeycode)(display, b'x' as u64);
        let paste_keycode = (xlib.XKeysymToKeycode)(display, b'v' as u64);
        let mut keys = [0_i8; 32];
        (xlib.XQueryKeymap)(display, keys.as_mut_ptr());
        (xlib.XCloseDisplay)(display);
        let is_down = |keycode: u8| {
            keycode != 0 && (keys[(keycode / 8) as usize] as u8 & (1 << (keycode % 8))) != 0
        };
        (is_down(cut_keycode), is_down(paste_keycode))
    }
}

#[cfg(target_os = "windows")]
fn shortcut_keys_down() -> (bool, bool) {
    let is_down = |key: u8| unsafe {
        windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(
            key.to_ascii_uppercase() as i32
        ) < 0
    };
    (is_down(b'x'), is_down(b'v'))
}

fn sanitize_clipboard_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|path| {
            // KDE/Dolphin uses the RFC-standard CRLF line ending for
            // text/uri-list. arboard 3.6 currently leaves the trailing CR on
            // each decoded path, making an existing file appear to be absent.
            let cleaned = path.to_string_lossy();
            PathBuf::from(cleaned.trim_end_matches(['\r', '\0']))
        })
        .collect()
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn resource_tree_row(ui: &mut egui::Ui, active: bool, label: &str, count: usize) -> egui::Response {
    let selection = ui.visuals().selection.bg_fill;
    let row = egui::Frame::new()
        .fill(if active {
            selection.gamma_multiply(0.25)
        } else {
            Color32::TRANSPARENT
        })
        .stroke(if active {
            egui::Stroke::new(1.0, selection.gamma_multiply(0.5))
        } else {
            egui::Stroke::NONE
        })
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::Label::new(label)
                        .selectable(false)
                        .sense(egui::Sense::click()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(count.to_string()).weak());
                });
                response
            })
            .inner
        });
    ui.interact(
        row.response.rect,
        ui.id().with(("resource_tree_row", label)),
        egui::Sense::click(),
    )
}

fn sort_button(
    ui: &mut egui::Ui,
    label: &str,
    column: SortColumn,
    current: SortColumn,
    ascending: bool,
) -> bool {
    let arrow = if column == current {
        if ascending { " ^" } else { " v" }
    } else {
        ""
    };
    ui.button(format!("{label}{arrow}")).clicked()
}

fn set_sort(current: &mut SortColumn, ascending: &mut bool, requested: SortColumn) {
    if *current == requested {
        *ascending = !*ascending;
    } else {
        *current = requested;
        *ascending = true;
    }
}

fn is_archive_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "hak" | "erf" | "mod" | "sav"
            )
        })
}

fn category_for(extension: &str) -> &'static str {
    match extension {
        "2da" => "2DA",
        "nss" | "ncs" => "Scripts",
        "mdl" | "mtr" | "wok" | "pwk" | "dwk" => "Models",
        "tga" | "dds" | "plt" | "txi" | "bmp" | "jpg" | "png" | "ktx" => "Textures",
        "wav" | "mp3" | "ogg" | "bmu" => "Sounds",
        "are" | "git" | "gic" => "Areas",
        "utc" | "bic" => "Creatures",
        "uti" | "utp" | "utd" | "utw" | "utt" | "uts" | "ute" | "utm" | "utg" => "Blueprints",
        "dlg" => "Dialogs",
        "gui" | "xml" | "ttf" => "UI",
        "tlk" => "Talk Tables",
        "ifo" | "jrl" | "fac" | "itp" => "Module Data",
        _ => "Other",
    }
}

fn entry_matches_category(entry: &archive::Entry, category: Option<&str>) -> bool {
    match category {
        None => true,
        Some("New") => entry.is_new(),
        Some(wanted) => category_for(&entry.extension()) == wanted,
    }
}

fn is_text_type(extension: &str) -> bool {
    matches!(
        extension,
        "txt"
            | "nss"
            | "2da"
            | "txi"
            | "mtr"
            | "lua"
            | "ids"
            | "shd"
            | "jui"
            | "ini"
            | "xml"
            | "gui"
            | "set"
            | "css"
            | "json"
            | "tml"
            | "sql"
    )
}

fn decode_model_preview(bytes: &[u8], total_size: u64) -> Result<ModelPreview, String> {
    if bytes.len() >= 4 && bytes[..4] == [0, 0, 0, 0] {
        if bytes.len() < 12 {
            return Err("The compiled model header is incomplete".into());
        }
        let structured_size = u64::from(u32::from_le_bytes(
            bytes[4..8]
                .try_into()
                .expect("the compiled model header length was checked"),
        ));
        let raw_size = u64::from(u32::from_le_bytes(
            bytes[8..12]
                .try_into()
                .expect("the compiled model header length was checked"),
        ));
        let declared_size = 12_u64
            .checked_add(structured_size)
            .and_then(|size| size.checked_add(raw_size))
            .ok_or_else(|| "The compiled model size fields overflow".to_owned())?;
        if declared_size > total_size {
            return Err("The compiled model data ranges exceed the resource size".into());
        }
        let name = bytes.get(20..84).and_then(|field| {
            let end = field
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(field.len());
            let value = field[..end].to_vec();
            let value = String::from_utf8(value).ok()?;
            let value = value.trim();
            (!value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' '))
            .then(|| value.to_owned())
        });
        return Ok(ModelPreview::Compiled {
            name,
            structured_size,
            raw_size,
            strings: extract_model_strings(bytes),
            truncated: total_size > bytes.len() as u64,
        });
    }

    if looks_like_ascii_model(bytes) {
        return Ok(ModelPreview::Uncompiled {
            text: sanitize_ascii_model_text(bytes),
            truncated: total_size > bytes.len() as u64,
        });
    }

    Err("The resource is neither a compiled Aurora model nor recognizable ASCII MDL source".into())
}

fn looks_like_ascii_model(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(16 * 1024)];
    let non_nul = sample.iter().filter(|byte| **byte != 0).count();
    let printable = sample
        .iter()
        .filter(|byte| **byte != 0 && (byte.is_ascii_graphic() || byte.is_ascii_whitespace()))
        .count();
    if non_nul == 0 || printable.saturating_mul(100) < non_nul.saturating_mul(95) {
        return false;
    }
    let text = sanitize_ascii_model_text(sample).to_ascii_lowercase();
    text.contains("newmodel ")
        || text.contains("beginmodelgeom ")
        || text.contains("#maxmodel ascii")
        || text.contains("# mdl file")
        || (text.contains("filedependancy ") && text.contains("classification "))
        || (text.starts_with("<snoopstart ") && text.contains("checking node "))
}

fn sanitize_ascii_model_text(bytes: &[u8]) -> String {
    bytes
        .iter()
        .filter_map(|byte| match *byte {
            0 => None,
            b'\t' | b'\n' | b'\r' => Some(*byte as char),
            0x20..=0x7e => Some(*byte as char),
            _ => Some('\u{fffd}'),
        })
        .collect()
}

fn extract_model_strings(bytes: &[u8]) -> Vec<String> {
    let allowed = |byte: u8| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'\\' | b':' | b' ')
    };
    let mut strings = Vec::new();
    let mut seen = BTreeSet::new();
    let mut offset = 0;
    while offset < bytes.len() && strings.len() < MAX_EXTRACTED_MODEL_STRINGS {
        while offset < bytes.len() && !allowed(bytes[offset]) {
            offset += 1;
        }
        let start = offset;
        while offset < bytes.len() && allowed(bytes[offset]) {
            offset += 1;
        }
        let length = offset - start;
        if !(4..=160).contains(&length) {
            continue;
        }
        let value = String::from_utf8_lossy(&bytes[start..offset]);
        let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
        if value.len() < 4
            || value
                .bytes()
                .filter(|byte| byte.is_ascii_alphabetic())
                .count()
                < 2
            || !seen.insert(value.clone())
        {
            continue;
        }
        strings.push(value);
    }
    strings
}

struct Mp3PreviewInfo {
    version: &'static str,
    bitrate_kbps: u32,
    sample_rate_hz: u32,
    channels: &'static str,
    has_bmu_header: bool,
}

struct WavPreviewInfo {
    encoding: &'static str,
    channels: u16,
    sample_rate_hz: u32,
    byte_rate: u32,
    bits_per_sample: u16,
    data_size: Option<u64>,
}

fn show_wav_resource_preview(ui: &mut egui::Ui, bytes: &[u8], total_size: u64) {
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        show_wav_preview(ui, bytes);
    } else if parse_bmu_mp3_info(bytes).is_ok() {
        // Aurora archives commonly store BMU-wrapped or plain MP3 payloads
        // under the WAV resource type. Detect the payload instead of trusting
        // the archive extension so those resources receive useful details too.
        show_bmu_preview(ui, bytes, total_size);
    } else {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading("Audio resource");
            ui.colored_label(
                Color32::LIGHT_RED,
                "The WAV resource is neither RIFF/WAVE nor recognizable MP3 audio",
            );
        });
    }
}

fn show_wav_preview(ui: &mut egui::Ui, bytes: &[u8]) {
    ui.vertical_centered(|ui| match parse_wav_info(bytes) {
        Ok(info) => {
            ui.add_space(55.0);
            ui.heading("Waveform audio resource");
            ui.label("RIFF/WAVE audio");
            ui.add_space(8.0);
            egui::Grid::new("wav_audio_info")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Encoding:");
                    ui.label(info.encoding);
                    ui.end_row();
                    ui.label("Sample rate:");
                    ui.label(format!("{} Hz", info.sample_rate_hz));
                    ui.end_row();
                    ui.label("Channels:");
                    ui.label(match info.channels {
                        1 => "Mono".to_owned(),
                        2 => "Stereo".to_owned(),
                        channels => format!("{channels} channels"),
                    });
                    ui.end_row();
                    if info.bits_per_sample > 0 {
                        ui.label("Bit depth:");
                        ui.label(format!("{}-bit", info.bits_per_sample));
                        ui.end_row();
                    }
                    if info.byte_rate > 0 {
                        ui.label("Bitrate:");
                        ui.label(format!("{} kbit/s", u64::from(info.byte_rate) * 8 / 1000));
                        ui.end_row();
                    }
                    if let (Some(data_size), true) = (info.data_size, info.byte_rate > 0) {
                        let milliseconds =
                            data_size.saturating_mul(1000) / u64::from(info.byte_rate);
                        ui.label("Approx. duration:");
                        ui.label(format!(
                            "{}:{:02}.{:03}",
                            milliseconds / 60_000,
                            milliseconds / 1000 % 60,
                            milliseconds % 1000
                        ));
                        ui.end_row();
                    }
                });
        }
        Err(error) => {
            ui.add_space(80.0);
            ui.heading("Waveform audio resource");
            ui.colored_label(Color32::LIGHT_RED, error);
        }
    });
}

fn parse_wav_info(bytes: &[u8]) -> Result<WavPreviewInfo, String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("The resource does not contain a valid RIFF/WAVE header".into());
    }

    let mut format = None;
    let mut data_size = None;
    let mut offset = 12_usize;
    while offset.checked_add(8).is_some_and(|end| end <= bytes.len()) {
        let chunk_id = &bytes[offset..offset + 4];
        let chunk_size = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("the chunk header length was checked"),
        );
        let content = offset + 8;

        if chunk_id == b"fmt " {
            if chunk_size < 16 || content + 16 > bytes.len() {
                return Err("The WAV format chunk is incomplete".into());
            }
            let read_u16 = |start: usize| {
                u16::from_le_bytes(
                    bytes[start..start + 2]
                        .try_into()
                        .expect("the WAV format chunk length was checked"),
                )
            };
            let read_u32 = |start: usize| {
                u32::from_le_bytes(
                    bytes[start..start + 4]
                        .try_into()
                        .expect("the WAV format chunk length was checked"),
                )
            };
            let format_tag = read_u16(content);
            format = Some(WavPreviewInfo {
                encoding: match format_tag {
                    0x0001 => "PCM",
                    0x0002 => "Microsoft ADPCM",
                    0x0003 => "IEEE floating point",
                    0x0006 => "A-law",
                    0x0007 => "μ-law",
                    0x0011 => "IMA ADPCM",
                    0x0055 => "MPEG Layer III",
                    0xfffe => "WAVE_FORMAT_EXTENSIBLE",
                    _ => "Unknown WAV encoding",
                },
                channels: read_u16(content + 2),
                sample_rate_hz: read_u32(content + 4),
                byte_rate: read_u32(content + 8),
                bits_per_sample: read_u16(content + 14),
                data_size: None,
            });
        } else if chunk_id == b"data" {
            data_size = Some(u64::from(chunk_size));
        }

        let padded_size = u64::from(chunk_size) + u64::from(chunk_size % 2);
        let Some(next) = (content as u64)
            .checked_add(padded_size)
            .and_then(|next| usize::try_from(next).ok())
        else {
            return Err("The WAV chunk table is too large".into());
        };
        if next <= offset {
            return Err("The WAV chunk table is invalid".into());
        }
        offset = next;
    }

    let mut info = format.ok_or_else(|| "No WAV format chunk was found".to_owned())?;
    if info.channels == 0 || info.sample_rate_hz == 0 {
        return Err("The WAV format contains invalid channel or sample-rate values".into());
    }
    info.data_size = data_size;
    Ok(info)
}

fn show_bmu_preview(ui: &mut egui::Ui, bytes: &[u8], total_size: u64) {
    ui.vertical_centered(|ui| match parse_bmu_mp3_info(bytes) {
        Ok(info) => {
            ui.add_space(55.0);
            ui.heading("BioWare music resource");
            ui.label(if info.has_bmu_header {
                "BMU V1.0-wrapped MP3 audio"
            } else {
                "Plain MP3 audio used as BMU"
            });
            ui.add_space(8.0);
            egui::Grid::new("bmu_audio_info")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Encoding:");
                    ui.label(info.version);
                    ui.end_row();
                    ui.label("Bitrate:");
                    ui.label(format!("{} kbit/s", info.bitrate_kbps));
                    ui.end_row();
                    ui.label("Sample rate:");
                    ui.label(format!("{} Hz", info.sample_rate_hz));
                    ui.end_row();
                    ui.label("Channels:");
                    ui.label(info.channels);
                    ui.end_row();
                    if info.bitrate_kbps > 0 {
                        let seconds =
                            total_size.saturating_mul(8) / (u64::from(info.bitrate_kbps) * 1000);
                        ui.label("Approx. duration:");
                        ui.label(format!("{}:{:02}", seconds / 60, seconds % 60));
                        ui.end_row();
                    }
                });
        }
        Err(error) => {
            ui.add_space(80.0);
            ui.heading("BioWare music resource");
            ui.colored_label(Color32::LIGHT_RED, error);
        }
    });
}

fn parse_bmu_mp3_info(bytes: &[u8]) -> Result<Mp3PreviewInfo, String> {
    let has_bmu_header = bytes.starts_with(b"BMU V1.0");
    let start = if has_bmu_header { 8 } else { 0 };
    let frame = bytes[start..]
        .windows(4)
        .find(|header| {
            header[0] == 0xff
                && header[1] & 0xe0 == 0xe0
                && (header[1] >> 3) & 0x3 != 1
                && (header[1] >> 1) & 0x3 != 0
                && (header[2] >> 4) & 0xf != 0
                && (header[2] >> 4) & 0xf != 0xf
                && (header[2] >> 2) & 0x3 != 0x3
        })
        .ok_or_else(|| "No valid MP3 audio frame was found".to_owned())?;

    let version_bits = (frame[1] >> 3) & 0x3;
    let layer_bits = (frame[1] >> 1) & 0x3;
    if layer_bits != 1 {
        return Err("BMU audio is not MPEG Layer III".into());
    }
    let (version, bitrate_table, sample_rates): (&str, [u32; 15], [u32; 3]) = match version_bits {
        3 => (
            "MPEG-1 Layer III",
            [
                0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
            ],
            [44_100, 48_000, 32_000],
        ),
        2 => (
            "MPEG-2 Layer III",
            [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
            [22_050, 24_000, 16_000],
        ),
        0 => (
            "MPEG-2.5 Layer III",
            [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
            [11_025, 12_000, 8_000],
        ),
        _ => return Err("Unsupported MPEG audio version".into()),
    };
    let bitrate_kbps = bitrate_table[(frame[2] >> 4) as usize];
    let sample_rate_hz = sample_rates[((frame[2] >> 2) & 0x3) as usize];
    let channels = if frame[3] >> 6 == 3 { "Mono" } else { "Stereo" };
    Ok(Mp3PreviewInfo {
        version,
        bitrate_kbps,
        sample_rate_hz,
        channels,
        has_bmu_header,
    })
}

fn image_format_for(extension: &str) -> Option<image::ImageFormat> {
    use image::ImageFormat;

    Some(match extension.to_ascii_lowercase().as_str() {
        "bmp" => ImageFormat::Bmp,
        "dds" => ImageFormat::Dds,
        "gif" => ImageFormat::Gif,
        "ico" => ImageFormat::Ico,
        "jpg" | "jpeg" | "jpe" => ImageFormat::Jpeg,
        "png" => ImageFormat::Png,
        "pbm" | "pgm" | "ppm" | "pnm" => ImageFormat::Pnm,
        "tga" => ImageFormat::Tga,
        "tif" | "tiff" => ImageFormat::Tiff,
        "webp" => ImageFormat::WebP,
        _ => return None,
    })
}

fn is_previewable_image(extension: &str) -> bool {
    extension.eq_ignore_ascii_case("plt") || image_format_for(extension).is_some()
}

fn decode_preview_image(bytes: &[u8], extension: &str) -> Result<image::DynamicImage, String> {
    if extension.eq_ignore_ascii_case("plt") {
        return decode_plt_preview(bytes);
    }

    let format = image_format_for(extension)
        .ok_or_else(|| format!("Unsupported image format: {extension}"))?;

    if format == image::ImageFormat::Dds && !bytes.starts_with(b"DDS ") {
        let standard_dds = nwn_dds_to_standard(bytes)?;
        return decode_image_with_limits(&standard_dds, Some(image::ImageFormat::Dds))
            .map_err(|error| format!("Could not decode NWN DDS image: {error}"));
    }
    if format == image::ImageFormat::Dds && is_bc5_dds(bytes) {
        return decode_bc5_dds(bytes);
    }

    let format_error = match decode_image_with_limits(bytes, Some(format)) {
        Ok(image) => return Ok(image),
        Err(error) => error,
    };
    // Some archives contain a resource whose type does not match its actual image
    // encoding. Signature detection gives those resources a useful second chance.
    decode_image_with_limits(bytes, None)
        .map_err(|_| format!("Could not decode image: {format_error}"))
}

fn decode_image_with_limits(
    bytes: &[u8],
    format: Option<image::ImageFormat>,
) -> Result<image::DynamicImage, String> {
    let cursor = std::io::Cursor::new(bytes);
    let mut reader = if let Some(format) = format {
        image::ImageReader::with_format(cursor, format)
    } else {
        image::ImageReader::new(cursor)
            .with_guessed_format()
            .map_err(|error| error.to_string())?
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
    reader.limits(limits);
    reader.decode().map_err(|error| error.to_string())
}

fn preview_pixel_count(width: u32, height: u32, format: &str) -> Result<usize, String> {
    if width == 0 || height == 0 {
        return Err(format!("{format} has invalid dimensions"));
    }
    if width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return Err(format!(
            "{format} dimensions exceed the {MAX_IMAGE_SIDE} × {MAX_IMAGE_SIDE} preview limit"
        ));
    }
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| format!("{format} dimensions are too large"))?;
    let decoded_bytes = pixel_count
        .checked_mul(4)
        .ok_or_else(|| format!("{format} dimensions are too large"))?;
    if decoded_bytes as u64 > MAX_IMAGE_DECODE_BYTES {
        return Err(format!("{format} decoded image is too large to preview"));
    }
    Ok(pixel_count)
}

fn is_bc5_dds(bytes: &[u8]) -> bool {
    bytes.len() >= 128 && bytes.starts_with(b"DDS ") && matches!(&bytes[84..88], b"ATI2" | b"BC5U")
}

fn decode_bc5_dds(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    if !is_bc5_dds(bytes) {
        return Err("DDS is not an ATI2/BC5 texture".into());
    }
    let read_u32 = |offset: usize| {
        u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("four-byte field"),
        )
    };
    let height = read_u32(12);
    let width = read_u32(16);
    let pixel_count = preview_pixel_count(width, height, "BC5 DDS")?;
    let blocks_wide = width.div_ceil(4) as usize;
    let blocks_high = height.div_ceil(4) as usize;
    let top_level_size = blocks_wide
        .checked_mul(blocks_high)
        .and_then(|blocks| blocks.checked_mul(16))
        .ok_or_else(|| "BC5 DDS dimensions are too large".to_owned())?;
    if bytes.len() < 128 + top_level_size {
        return Err("BC5 DDS pixel data is truncated".into());
    }

    let mut rgba = vec![0_u8; pixel_count * 4];
    for block_y in 0..blocks_high {
        for block_x in 0..blocks_wide {
            let block_offset = 128 + (block_y * blocks_wide + block_x) * 16;
            let red = decode_bc4_block(&bytes[block_offset..block_offset + 8]);
            let green = decode_bc4_block(&bytes[block_offset + 8..block_offset + 16]);
            for local_y in 0..4 {
                for local_x in 0..4 {
                    let x = block_x * 4 + local_x;
                    let y = block_y * 4 + local_y;
                    if x >= width as usize || y >= height as usize {
                        continue;
                    }
                    let block_pixel = local_y * 4 + local_x;
                    let r = red[block_pixel];
                    let g = green[block_pixel];
                    let normal_x = f32::from(r) / 127.5 - 1.0;
                    let normal_y = f32::from(g) / 127.5 - 1.0;
                    let normal_z = (1.0 - normal_x * normal_x - normal_y * normal_y)
                        .max(0.0)
                        .sqrt();
                    let b = ((normal_z * 0.5 + 0.5) * 255.0).round() as u8;
                    let output = (y * width as usize + x) * 4;
                    rgba[output..output + 4].copy_from_slice(&[r, g, b, 255]);
                }
            }
        }
    }

    let buffer = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| "Could not construct BC5 preview image".to_owned())?;
    Ok(image::DynamicImage::ImageRgba8(buffer))
}

fn decode_bc4_block(block: &[u8]) -> [u8; 16] {
    debug_assert_eq!(block.len(), 8);
    let endpoint_0 = block[0];
    let endpoint_1 = block[1];
    let mut palette = [0_u8; 8];
    palette[0] = endpoint_0;
    palette[1] = endpoint_1;
    if endpoint_0 > endpoint_1 {
        for index in 1..=6 {
            palette[index + 1] = (((7 - index) as u16 * u16::from(endpoint_0)
                + index as u16 * u16::from(endpoint_1)
                + 3)
                / 7) as u8;
        }
    } else {
        for index in 1..=4 {
            palette[index + 1] = (((5 - index) as u16 * u16::from(endpoint_0)
                + index as u16 * u16::from(endpoint_1)
                + 2)
                / 5) as u8;
        }
        palette[6] = 0;
        palette[7] = 255;
    }

    let mut packed_indices = 0_u64;
    for (shift, byte) in block[2..8].iter().enumerate() {
        packed_indices |= u64::from(*byte) << (shift * 8);
    }
    let mut values = [0_u8; 16];
    for (pixel, value) in values.iter_mut().enumerate() {
        let palette_index = ((packed_indices >> (pixel * 3)) & 0x7) as usize;
        *value = palette[palette_index];
    }
    values
}

fn decode_plt_preview(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    const HEADER_SIZE: usize = 24;
    const LAYER_COLORS: [[u8; 4]; 10] = [
        [224, 191, 160, 255], // skin
        [74, 52, 26, 255],    // hair
        [176, 184, 192, 255], // metal 1
        [226, 168, 60, 255],  // metal 2
        [171, 47, 39, 255],   // cloth 1
        [42, 86, 173, 255],   // cloth 2
        [92, 62, 41, 255],    // leather 1
        [128, 92, 58, 255],   // leather 2
        [37, 152, 117, 255],  // tattoo 1
        [108, 64, 160, 255],  // tattoo 2
    ];

    if bytes.len() < HEADER_SIZE {
        return Err("PLT header is truncated".into());
    }
    if &bytes[..8] != b"PLT V1  " {
        return Err("Unsupported PLT signature or version".into());
    }
    let read_u32 = |offset: usize| {
        u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("four-byte field"),
        )
    };
    let width = read_u32(16);
    let height = read_u32(20);
    let pixel_count = preview_pixel_count(width, height, "PLT")?;
    let payload_size = pixel_count
        .checked_mul(2)
        .ok_or_else(|| "PLT payload is too large".to_owned())?;
    if bytes.len() < HEADER_SIZE + payload_size {
        return Err("PLT pixel data is truncated".into());
    }

    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for pixel in bytes[HEADER_SIZE..HEADER_SIZE + payload_size].chunks_exact(2) {
        let value = u16::from(pixel[0]);
        let base = LAYER_COLORS
            .get(pixel[1] as usize)
            .copied()
            .unwrap_or([255, 0, 255, 255]);
        for channel in base {
            rgba.push(((u16::from(channel) * value + 127) / 255) as u8);
        }
    }

    let buffer = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| "Could not construct PLT preview image".to_owned())?;
    Ok(image::DynamicImage::ImageRgba8(buffer))
}

fn nwn_dds_to_standard(bytes: &[u8]) -> Result<Vec<u8>, String> {
    const NWN_HEADER_SIZE: usize = 20;
    if bytes.len() < NWN_HEADER_SIZE {
        return Err("NWN DDS header is truncated".into());
    }

    let read_u32 = |offset: usize| {
        u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("four-byte field"),
        )
    };
    let width = read_u32(0);
    let height = read_u32(4);
    let channels = read_u32(8);
    let linear_size = read_u32(12);
    preview_pixel_count(width, height, "NWN DDS")?;
    let four_cc = match channels {
        3 => *b"DXT1",
        4 => *b"DXT5",
        other => return Err(format!("Unsupported NWN DDS channel count: {other}")),
    };
    if bytes.len() - NWN_HEADER_SIZE < linear_size as usize {
        return Err("NWN DDS pixel data is truncated".into());
    }

    let mut output = Vec::with_capacity(128 + bytes.len() - NWN_HEADER_SIZE);
    output.extend_from_slice(b"DDS ");
    let push_u32 = |output: &mut Vec<u8>, value: u32| {
        output.extend_from_slice(&value.to_le_bytes());
    };
    push_u32(&mut output, 124); // DDS_HEADER size
    push_u32(&mut output, 0x0008_1007); // caps, height, width, pixel format, linear size
    push_u32(&mut output, height);
    push_u32(&mut output, width);
    push_u32(&mut output, linear_size);
    push_u32(&mut output, 0); // depth
    push_u32(&mut output, 0); // mip count; the preview only needs the top level
    for _ in 0..11 {
        push_u32(&mut output, 0);
    }
    push_u32(&mut output, 32); // DDS_PIXELFORMAT size
    push_u32(&mut output, 0x4); // DDPF_FOURCC
    output.extend_from_slice(&four_cc);
    for _ in 0..5 {
        push_u32(&mut output, 0);
    }
    push_u32(&mut output, 0x1000); // DDSCAPS_TEXTURE
    for _ in 0..4 {
        push_u32(&mut output, 0);
    }
    debug_assert_eq!(output.len(), 128);
    output.extend_from_slice(&bytes[NWN_HEADER_SIZE..]);
    Ok(output)
}

fn show_2da_preview(ui: &mut egui::Ui, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let Some(signature) = lines.next() else {
        ui.label("Empty 2DA file");
        return;
    };
    ui.label(
        RichText::new(signature)
            .monospace()
            .color(Color32::from_rgb(120, 190, 240)),
    );
    let remaining: Vec<_> = lines
        .filter(|line| !line.trim_start().starts_with("DEFAULT:"))
        .take(101)
        .collect();
    if remaining.is_empty() {
        return;
    }
    let headers = split_2da_line(remaining[0]);
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        egui::Grid::new("2da_preview").striped(true).show(ui, |ui| {
            for header in &headers {
                ui.strong(header);
            }
            ui.end_row();
            for line in remaining.iter().skip(1) {
                for cell in split_2da_line(line) {
                    ui.label(RichText::new(cell).monospace());
                }
                ui.end_row();
            }
        });
    });
}

fn split_2da_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    for character in line.trim().chars() {
        match character {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !field.is_empty() {
                    fields.push(std::mem::take(&mut field));
                }
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() {
        fields.push(field);
    }
    fields
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    #[test]
    fn removes_uri_list_crlf_residue() {
        let paths = sanitize_clipboard_paths(vec![PathBuf::from("/tmp/example.mdl\r")]);
        assert_eq!(paths, vec![PathBuf::from("/tmp/example.mdl")]);
    }

    #[test]
    fn recognizes_previewable_image_extensions() {
        assert_eq!(image_format_for("tga"), Some(image::ImageFormat::Tga));
        assert_eq!(image_format_for("DDS"), Some(image::ImageFormat::Dds));
        assert_eq!(image_format_for("png"), Some(image::ImageFormat::Png));
        assert_eq!(image_format_for("jpg"), Some(image::ImageFormat::Jpeg));
        assert_eq!(image_format_for("jpeg"), Some(image::ImageFormat::Jpeg));
        assert_eq!(image_format_for("mdl"), None);
        assert!(is_previewable_image("plt"));
        assert_eq!(category_for("plt"), "Textures");
        assert!(is_text_type("mtr"));
        assert!(is_text_type("lua"));
        assert!(is_text_type("ids"));
        assert!(is_text_type("shd"));
        assert!(is_text_type("jui"));
    }

    #[test]
    fn decodes_a_preview_image() {
        let decoded = decode_preview_image(include_bytes!("../assets/aheicon-256.png"), "png")
            .expect("the bundled PNG should decode through the preview path");
        assert_eq!((decoded.width(), decoded.height()), (256, 256));
    }

    #[test]
    fn decodes_legacy_nwn_dds() {
        let mut dds = Vec::new();
        dds.extend_from_slice(&4_u32.to_le_bytes());
        dds.extend_from_slice(&4_u32.to_le_bytes());
        dds.extend_from_slice(&3_u32.to_le_bytes());
        dds.extend_from_slice(&8_u32.to_le_bytes());
        dds.extend_from_slice(&1.0_f32.to_le_bytes());
        // One solid-red DXT1 block.
        dds.extend_from_slice(&[0x00, 0xf8, 0x00, 0x00, 0, 0, 0, 0]);

        let decoded = decode_preview_image(&dds, "dds").expect("NWN DDS should decode");
        assert_eq!((decoded.width(), decoded.height()), (4, 4));
        let pixel = decoded.into_rgba8().get_pixel(0, 0).0;
        assert!(pixel[0] > 200 && pixel[1] < 20 && pixel[2] < 20);
    }

    #[test]
    fn decodes_ati2_bc5_normal_map() {
        let mut dds = vec![0_u8; 128];
        dds[..4].copy_from_slice(b"DDS ");
        dds[4..8].copy_from_slice(&124_u32.to_le_bytes());
        dds[12..16].copy_from_slice(&4_u32.to_le_bytes());
        dds[16..20].copy_from_slice(&4_u32.to_le_bytes());
        dds[76..80].copy_from_slice(&32_u32.to_le_bytes());
        dds[80..84].copy_from_slice(&4_u32.to_le_bytes());
        dds[84..88].copy_from_slice(b"ATI2");
        dds.extend_from_slice(&[128, 128, 0, 0, 0, 0, 0, 0]);
        dds.extend_from_slice(&[128, 128, 0, 0, 0, 0, 0, 0]);

        let decoded = decode_preview_image(&dds, "dds").expect("ATI2 DDS should decode");
        assert_eq!((decoded.width(), decoded.height()), (4, 4));
        assert_eq!(decoded.into_rgba8().get_pixel(0, 0).0, [128, 128, 255, 255]);
    }

    #[test]
    fn decodes_plt_with_representative_layer_colors() {
        let mut plt = b"PLT V1  \x0a\0\0\0\0\0\0\0".to_vec();
        plt.extend_from_slice(&2_u32.to_le_bytes());
        plt.extend_from_slice(&1_u32.to_le_bytes());
        plt.extend_from_slice(&[255, 4, 128, 5]);

        let decoded = decode_preview_image(&plt, "plt").expect("PLT should decode");
        assert_eq!((decoded.width(), decoded.height()), (2, 1));
        let pixels = decoded.into_rgba8();
        assert_eq!(pixels.get_pixel(0, 0).0, [171, 47, 39, 255]);
        assert_eq!(pixels.get_pixel(1, 0).0, [21, 43, 87, 128]);
    }

    #[test]
    fn rejects_oversized_custom_image_dimensions_before_allocating() {
        let mut dds = vec![0_u8; 128];
        dds[..4].copy_from_slice(b"DDS ");
        dds[12..16].copy_from_slice(&20_000_u32.to_le_bytes());
        dds[16..20].copy_from_slice(&20_000_u32.to_le_bytes());
        dds[84..88].copy_from_slice(b"ATI2");
        assert!(
            decode_bc5_dds(&dds)
                .unwrap_err()
                .contains("dimensions exceed")
        );

        let mut plt = b"PLT V1  \x0a\0\0\0\0\0\0\0".to_vec();
        plt.extend_from_slice(&20_000_u32.to_le_bytes());
        plt.extend_from_slice(&20_000_u32.to_le_bytes());
        assert!(
            decode_plt_preview(&plt)
                .unwrap_err()
                .contains("dimensions exceed")
        );
    }

    #[test]
    fn recognizes_bmu_wrapped_mp3_audio() {
        let mut bmu = b"BMU V1.0".to_vec();
        bmu.extend_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
        let info = parse_bmu_mp3_info(&bmu).expect("BMU MP3 frame should be recognized");
        assert!(info.has_bmu_header);
        assert_eq!(info.version, "MPEG-1 Layer III");
        assert_eq!(info.bitrate_kbps, 128);
        assert_eq!(info.sample_rate_hz, 44_100);
        assert_eq!(info.channels, "Stereo");

        let plain = parse_bmu_mp3_info(&[0xff, 0xfb, 0x90, 0x00])
            .expect("plain MP3 stored as WAV should be recognized");
        assert!(!plain.has_bmu_header);
        assert_eq!(plain.bitrate_kbps, 128);
    }

    #[test]
    fn recognizes_pcm_wav_audio() {
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&176_436_u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&44_100_u32.to_le_bytes());
        wav.extend_from_slice(&176_400_u32.to_le_bytes());
        wav.extend_from_slice(&4_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&176_400_u32.to_le_bytes());

        let info = parse_wav_info(&wav).expect("PCM WAV header should be recognized");
        assert_eq!(info.encoding, "PCM");
        assert_eq!(info.channels, 2);
        assert_eq!(info.sample_rate_hz, 44_100);
        assert_eq!(info.byte_rate, 176_400);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.data_size, Some(176_400));
    }

    #[test]
    fn distinguishes_ascii_and_compiled_models() {
        let ascii =
            b"# mdl file\nnewmodel example\nclassification Character\nbeginmodelgeom example\n";
        match decode_model_preview(ascii, ascii.len() as u64).expect("ASCII MDL should parse") {
            ModelPreview::Uncompiled { text, truncated } => {
                assert!(text.contains("newmodel example"));
                assert!(!truncated);
            }
            ModelPreview::Compiled { .. } => panic!("ASCII MDL was classified as compiled"),
        }

        let mut compiled = vec![0_u8; 128];
        compiled[4..8].copy_from_slice(&100_u32.to_le_bytes());
        compiled[8..12].copy_from_slice(&16_u32.to_le_bytes());
        compiled[20..33].copy_from_slice(b"example_model");
        compiled[88..100].copy_from_slice(b"texture_name");
        match decode_model_preview(&compiled, compiled.len() as u64)
            .expect("compiled MDL should parse")
        {
            ModelPreview::Compiled {
                name,
                structured_size,
                raw_size,
                strings,
                truncated,
            } => {
                assert_eq!(name.as_deref(), Some("example_model"));
                assert_eq!(structured_size, 100);
                assert_eq!(raw_size, 16);
                assert!(strings.iter().any(|value| value == "texture_name"));
                assert!(!truncated);
            }
            ModelPreview::Uncompiled { .. } => panic!("compiled MDL was classified as ASCII"),
        }

        let ascii_with_nul_padding =
            b"#MAXMODEL ASCII\nnewmodel padded\nclassification Character\n\0\0";
        match decode_model_preview(ascii_with_nul_padding, ascii_with_nul_padding.len() as u64)
            .expect("NUL-padded ASCII MDL should parse")
        {
            ModelPreview::Uncompiled { text, truncated } => {
                assert!(text.contains("newmodel padded"));
                assert!(!text.contains('\0'));
                assert!(!truncated);
            }
            ModelPreview::Compiled { .. } => {
                panic!("NUL-padded ASCII MDL was classified as compiled")
            }
        }
    }
}
