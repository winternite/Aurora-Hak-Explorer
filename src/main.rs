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
    path::PathBuf,
    time::{Duration, Instant},
};

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
    image_preview: Option<ImagePreviewCache>,
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum SortColumn {
    Name,
    Type,
    Size,
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
    queue: VecDeque<PathBuf>,
    conflict: Option<AddConflict>,
    policy: ConflictPolicy,
    added: usize,
    replaced: usize,
    skipped: usize,
    failures: Vec<String>,
}

impl AddBatch {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self {
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
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let compact_mode = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, "compact_mode"))
            .unwrap_or(false);
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
            image_preview: None,
        }
    }

    fn show_image_preview(&mut self, ui: &mut egui::Ui, entry: &archive::Entry) {
        const MAX_IMAGE_FILE_SIZE: u64 = 128 * 1024 * 1024;
        const MAX_TEXTURE_SIDE: u32 = 4096;

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
        if let (Some(index), Some(state)) = (self.active_tab, self.current_tab_state()) {
            if let Some(tab) = self.tabs.get_mut(index) {
                *tab = state;
            }
        }
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
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.archive.path.as_deref() == Some(path.as_path()))
        {
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
            }
            Err(e) => self.fail("Could not open archive", e),
        }
    }
    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("NWN archives", &["hak", "erf", "mod", "sav"])
            .pick_file()
        {
            self.open_path(path);
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
            batch.queue.extend(paths);
            return;
        }
        self.pending_add = Some(AddBatch::new(paths));
        self.process_add_batch(ConflictAction::Continue);
    }
    fn process_add_batch(&mut self, action: ConflictAction) {
        let Some(mut batch) = self.pending_add.take() else {
            return;
        };
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

        self.dirty |= batch.added + batch.replaced > 0;
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
        let Some(archive) = self.archive.as_mut() else {
            return;
        };
        match archive.add_directory(&path) {
            Ok((added, skipped)) => {
                self.dirty |= added > 0;
                self.status = format!(
                    "Added {added} resources from {} ({skipped} skipped)",
                    path.display()
                );
            }
            Err(e) => self.fail("Could not add directory", e),
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
                let Some(current) = self.archive.as_mut() else {
                    return;
                };
                let (added, replaced) = current.merge(&other);
                self.dirty |= added + replaced > 0;
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
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name(entry.filename())
                .save_file()
            {
                match archive.export_entry(index, &path) {
                    Ok(()) => self.status = format!("Exported {}", path.display()),
                    Err(e) => self.fail("Could not export resource", e),
                }
            }
        } else if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            let mut count = 0;
            for index in self.selected.clone() {
                let entry = &archive.entries[index];
                if let Err(e) = archive.export_entry(index, dir.join(entry.filename())) {
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
            let path = directory.path().join(entry.filename());
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
            let path = directory.path().join(entry.filename());
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
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let resource_context = !ctx.egui_wants_keyboard_input()
            && self.confirm_close_tab.is_none()
            && self.pending_add.is_none()
            && !self.show_new
            && !self.show_description
            && self.error.is_none();
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
        let cut_down = raw_input.modifiers.command && x11_keysym_down(b'x' as u64);
        let paste_down = raw_input.modifiers.command && x11_keysym_down(b'v' as u64);
        if raw_input.modifiers.command {
            // Keep sampling briefly while Ctrl is held so a file-list paste is
            // caught even though egui-winit does not forward it as an event.
            ctx.request_repaint_after(Duration::from_millis(8));
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
        self.capture_typeahead(&ctx);
        let dropped = std::mem::take(&mut self.pending_drop_files);
        if !dropped.is_empty() {
            let (archives, resources): (Vec<_>, Vec<_>) =
                dropped.into_iter().partition(|path| is_archive_path(path));
            for path in archives {
                self.open_path(path);
            }
            if !resources.is_empty() {
                self.add_paths(resources);
            }
        }
        if !self.hovered_drop_files.is_empty() {
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
        egui::Panel::top("document_tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("+").on_hover_text("New archive").clicked() {
                    self.show_new = true;
                }
                egui::ScrollArea::horizontal()
                    .id_salt("open_archive_tabs")
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (index, tab) in self.tabs.iter().enumerate() {
                                let active = self.active_tab == Some(index);
                                let dirty = if active { self.dirty } else { tab.dirty };
                                let label =
                                    format!("{}{}", if dirty { "*" } else { "" }, tab.label());
                                ui.push_id(index, |ui| {
                                    if ui.selectable_label(active, label).clicked() {
                                        requested_switch = Some(index);
                                    }
                                    if ui.small_button("x").on_hover_text("Close tab").clicked() {
                                        requested_close = Some(index);
                                    }
                                });
                                ui.separator();
                            }
                        });
                    });
            });
        });
        if let Some(index) = requested_close {
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
        } else if let Some(index) = requested_switch {
            self.switch_tab(index);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::O)) {
            self.open_dialog();
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::S)) {
            self.save(false);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::N)) {
            self.show_new = true;
        }
        if !ctx.egui_wants_keyboard_input()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A))
        {
            request_select_all = true;
        }
        if let Some(command) = self.pending_clipboard_command.take() {
            match command {
                ClipboardCommand::Copy => request_copy = true,
                ClipboardCommand::Cut => request_cut = true,
                ClipboardCommand::Paste => request_paste = true,
            }
        }
        let clipboard_shortcuts_enabled = !ctx.egui_wants_keyboard_input()
            && self.confirm_close_tab.is_none()
            && self.pending_add.is_none()
            && !self.show_new
            && !self.show_description
            && self.error.is_none();
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
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::W)) {
            if let Some(index) = self.active_tab {
                if self.dirty {
                    self.confirm_close_tab = Some(index);
                } else {
                    self.close_tab(index);
                }
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Delete)) {
            self.delete_selected();
        }
        let keyboard_navigation = if !ctx.egui_wants_keyboard_input()
            && self.confirm_close_tab.is_none()
            && !self.show_new
            && !self.show_description
            && self.error.is_none()
            && self.pending_add.is_none()
        {
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
                    ui.heading("Resource Tree");
                    ui.separator();
                    if ui
                        .selectable_label(self.category.is_none(), format!("📦 {name}     {count}"))
                        .clicked()
                    {
                        self.category = None;
                    }
                    ui.indent("categories", |ui| {
                        let active = self.category.as_deref() == Some("New");
                        if ui
                            .selectable_label(active, format!("+ New     {new_count}"))
                            .clicked()
                        {
                            self.category = Some("New".to_owned());
                            self.selected.clear();
                            self.selection_anchor = None;
                            self.selection_cursor = None;
                        }
                        for (category, amount) in &categories {
                            let active = self.category.as_deref() == Some(category);
                            if ui
                                .selectable_label(active, format!("📁 {category}     {amount}"))
                                .clicked()
                            {
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
                        ui.heading(entry.filename());
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("Type: {}", entry.extension().to_ascii_uppercase()));
                            ui.separator();
                            ui.label(format!(
                                "Size: {}",
                                entry.size().map(human_size).unwrap_or_else(|_| "?".into())
                            ));
                            ui.separator();
                            ui.label(format!("Type ID: 0x{:04x}", entry.type_id));
                        });
                        ui.add_space(8.0);
                        ui.group(|ui| {
                            ui.set_min_height(300.0);
                            let extension = entry.extension();
                            if image_format_for(&extension).is_some() {
                                self.show_image_preview(ui, entry);
                            } else {
                                match entry.read_prefix(256 * 1024) {
                                    Ok(bytes) if entry.extension() == "2da" => {
                                        show_2da_preview(ui, &bytes)
                                    }
                                    Ok(bytes) if is_text_type(&entry.extension()) => {
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
                if ui.button("Open HAK…").clicked() {
                    self.open_dialog();
                }
                if let Some((name, _, _, _, _)) = &archive_info {
                    ui.separator();
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
                egui::ScrollArea::vertical()
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
                                    if response.drag_started() {
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
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.heading("Aurora Hak Explorer");
                    ui.label("AHE");
                    ui.label("A native editor for Neverwinter Nights HAK and ERF archives");
                    ui.add_space(16.0);
                    if ui.button("Open an archive").clicked() {
                        self.open_dialog();
                    }
                    if ui.button("Create a new archive").clicked() {
                        self.show_new = true;
                    }
                    ui.add_space(10.0);
                    ui.label("You can also drop a .hak, .erf, .mod, or .sav file here.");
                });
            }
        });
        if self.show_new {
            let mut open = true;
            egui::Window::new("New archive")
                .open(&mut open)
                .collapsible(false)
                .show(&ctx, |ui| {
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
                    if ui.button("Create").clicked() {
                        self.new_archive();
                    }
                });
            self.show_new &= open;
        }
        if self.show_description {
            let mut open = true;
            egui::Window::new("Archive description")
                .open(&mut open)
                .show(&ctx, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.description_buffer)
                            .desired_rows(8)
                            .desired_width(480.0),
                    );
                    if ui.button("Apply").clicked() {
                        if let Some(a) = self.archive.as_mut() {
                            a.set_description(self.description_buffer.clone());
                            self.dirty = true;
                        }
                        self.show_description = false;
                    }
                });
            self.show_description &= open;
        }
        if self.show_about {
            egui::Window::new("About")
                .open(&mut self.show_about)
                .show(&ctx, |ui| {
                    ui.heading("Aurora Hak Explorer (AHE) 0.2.1");
                    ui.label("Native HAK/ERF archive management for Linux.");
                    ui.label("Copyright © 2026 Winternite");
                    ui.hyperlink_to(
                        "GNU GPL v3 or later",
                        "https://www.gnu.org/licenses/gpl-3.0.html",
                    );
                });
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
            let modal = egui::Modal::new(egui::Id::new("resource_conflict_modal"))
                .frame(egui::Frame::popup(&ctx.style_of(egui::Theme::Dark)).inner_margin(22.0))
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
            let modal = egui::Modal::new(egui::Id::new("unsaved_changes_modal"))
                .frame(egui::Frame::popup(&ctx.style_of(egui::Theme::Dark)).inner_margin(22.0))
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
                    ui.horizontal(|ui| {
                        if ui
                            .add_sized(
                                [120.0, 38.0],
                                egui::Button::new(RichText::new("Save").size(16.0)),
                            )
                            .clicked()
                        {
                            save = true;
                        }
                        if ui
                            .add_sized(
                                [120.0, 38.0],
                                egui::Button::new(RichText::new("Discard").size(16.0)),
                            )
                            .clicked()
                        {
                            discard = true;
                        }
                        if ui
                            .add_sized(
                                [120.0, 38.0],
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
            let mut open = true;
            egui::Window::new("Error")
                .open(&mut open)
                .collapsible(false)
                .show(&ctx, |ui| {
                    ui.label(RichText::new(message).color(Color32::from_rgb(255, 130, 130)));
                    if ui.button("Close").clicked() {
                        self.error = None;
                    }
                });
            if !open {
                self.error = None;
            }
        }
        self.sync_current_tab();
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
fn x11_keysym_down(keysym: u64) -> bool {
    let Ok(xlib) = x11_dl::xlib::Xlib::open() else {
        return false;
    };
    unsafe {
        let display = (xlib.XOpenDisplay)(std::ptr::null());
        if display.is_null() {
            return false;
        }
        let keycode = (xlib.XKeysymToKeycode)(display, keysym);
        let mut keys = [0_i8; 32];
        (xlib.XQueryKeymap)(display, keys.as_mut_ptr());
        (xlib.XCloseDisplay)(display);
        keycode != 0 && (keys[(keycode / 8) as usize] as u8 & (1 << (keycode % 8))) != 0
    }
}

#[cfg(target_os = "windows")]
fn x11_keysym_down(keysym: u64) -> bool {
    // The caller passes lower-case ASCII keysyms. Win32 virtual-key codes for
    // alphabetic keys are the corresponding upper-case ASCII values.
    let virtual_key = (keysym as u8).to_ascii_uppercase() as i32;
    unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(virtual_key) < 0 }
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
        "mdl" | "mtr" | "plt" | "wok" | "pwk" | "dwk" => "Models",
        "tga" | "dds" | "txi" | "bmp" | "jpg" | "png" | "ktx" => "Textures",
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

fn decode_preview_image(bytes: &[u8], extension: &str) -> Result<image::DynamicImage, String> {
    let format = image_format_for(extension)
        .ok_or_else(|| format!("Unsupported image format: {extension}"))?;

    if format == image::ImageFormat::Dds && !bytes.starts_with(b"DDS ") {
        let standard_dds = nwn_dds_to_standard(bytes)?;
        return image::load_from_memory_with_format(&standard_dds, image::ImageFormat::Dds)
            .map_err(|error| format!("Could not decode NWN DDS image: {error}"));
    }

    image::load_from_memory_with_format(bytes, format)
        .or_else(|format_error| {
            // Some archives contain a resource whose type does not match its actual image
            // encoding. Signature detection gives those resources a useful second chance.
            image::load_from_memory(bytes).map_err(|_| format_error)
        })
        .map_err(|error| format!("Could not decode image: {error}"))
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
    if width == 0 || height == 0 {
        return Err("NWN DDS has invalid dimensions".into());
    }
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
    }

    #[test]
    fn decodes_a_preview_image() {
        let decoded = image::load_from_memory_with_format(
            include_bytes!("../assets/aheicon-256.png"),
            image::ImageFormat::Png,
        )
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
}
