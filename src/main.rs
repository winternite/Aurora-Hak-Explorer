#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod archive;
#[cfg(target_os = "linux")]
mod dnd_extract;
mod drag_cleanup;
#[cfg(target_os = "linux")]
mod drag_out;
#[cfg(target_os = "windows")]
#[path = "drag_out_windows.rs"]
mod drag_out;
mod game_resources;
mod mdl;
mod resource_types;
mod save_cleanup;
mod single_instance;

use archive::{Archive, ArchiveKind, ArchiveVersion, Entry, EntryData};
use eframe::egui::{self, Color32, RichText};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_IMAGE_FILE_SIZE: u64 = 128 * 1024 * 1024;
const MAX_IMAGE_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_IMAGE_SIDE: u32 = 16_384;
const MAX_TEXTURE_SIDE: u32 = 4096;
const MAX_RECENT_ARCHIVES: usize = 8;
const MAX_INTERNAL_DRAG_ORIGINS: usize = 16;
const MAX_MODEL_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MODEL_RENDER_BYTES: u64 = 128 * 1024 * 1024;
const MAX_EXTRACTED_MODEL_STRINGS: usize = 2_000;
const IMPORT_MIN_FILES_PER_FRAME: usize = 256;
const IMPORT_MAX_FILES_PER_FRAME: usize = 4096;
const IMPORT_FRAME_BUDGET: Duration = Duration::from_millis(8);
const MAX_DROPPED_DIRECTORY_FILES: usize = 1_000_000;
const MAX_UNDO_STEPS: usize = 32;
const MAX_UNDO_BYTES: usize = 64 * 1024 * 1024;
const DISPLAY_VERSION: &str = env!("CARGO_PKG_VERSION");
type TextureDependencies = BTreeMap<String, Vec<archive::Entry>>;
type TextureDependencyResult = (PathBuf, TextureDependencies);

fn main() -> eframe::Result {
    let arguments: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if arguments
        .first()
        .is_some_and(|argument| argument == drag_cleanup::HELPER_ARGUMENT)
    {
        if let Err(error) = drag_cleanup::run_helper() {
            eprintln!("Drag cleanup helper failed: {error}");
        }
        return Ok(());
    }
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
                    archive.version_label()
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
    let incoming_paths = match single_instance::route(&arguments) {
        single_instance::Launch::Forwarded => return Ok(()),
        single_instance::Launch::Primary(receiver) => {
            drag_cleanup::recover_abandoned();
            save_cleanup::recover_abandoned();
            receiver
        }
    };
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
            editor.incoming_paths = Some(incoming_paths);
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
    warning: Option<String>,
    dirty: bool,
    edit_history: EditHistory,
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
    hovered_drop_files: Vec<HoveredDropFile>,
    hovered_drop_file_count: usize,
    hovered_drop_unsupported_count: usize,
    hovered_drop_unsupported_extensions: BTreeSet<String>,
    pending_drop_files: Vec<PathBuf>,
    incoming_paths: Option<mpsc::Receiver<Vec<PathBuf>>>,
    internal_drag_origins: BTreeMap<PathBuf, InternalDragOrigin>,
    tab_drop_rects: Vec<(usize, egui::Rect)>,
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
    nwn_installation: Option<PathBuf>,
    resource_middle_scroll_anchor: Option<egui::Pos2>,
    resource_scroll_reset_pending: bool,
    image_preview: Option<ImagePreviewCache>,
    model_preview: Option<ModelPreviewCache>,
    model_view: ModelView,
    model_yaw: f32,
    model_pitch: f32,
    model_zoom: f32,
    texture_dependency_directory: Option<PathBuf>,
    texture_dependencies: TextureDependencies,
    texture_dependency_receiver: Option<mpsc::Receiver<TextureDependencyResult>>,
    resource_view_cache: Option<ResourceViewCache>,
    model_compiler: Option<Result<ModelCompiler, String>>,
    model_compile_job: Option<ModelCompileJob>,
    quit_after_model_compile: bool,
    #[cfg(target_os = "linux")]
    dnd_extract: Option<dnd_extract::Bridge>,
}

struct HoveredDropFile {
    name: String,
    unsupported: bool,
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
    scene: Result<mdl::Scene, String>,
    textures: BTreeMap<String, ModelTexture>,
}

#[derive(Clone)]
struct ModelTexture {
    handle: egui::TextureHandle,
    flip_vertical: bool,
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
    Model,
    Source,
    Strings,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ModelExportAction {
    Compile,
    Decompile,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SortColumn {
    Name,
    Type,
    Size,
}

#[derive(Clone)]
struct ResourceSummary {
    categories: BTreeMap<String, usize>,
    compiled_models: usize,
    uncompiled_models: usize,
    new_count: usize,
    name: String,
    count: usize,
    bytes: u64,
    version: &'static str,
    kind: String,
}

struct ResourceViewCache {
    archive_key: (u64, u64),
    filter: String,
    category: Option<String>,
    compact_mode: bool,
    sort_column: SortColumn,
    sort_ascending: bool,
    summary: ResourceSummary,
    visible_indices: Arc<[usize]>,
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
    entry: Entry,
    existing_filename: String,
    replacement_index: usize,
}

#[derive(Clone)]
struct InternalDragOrigin {
    source_tab: usize,
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
    unsupported_files: usize,
    unsupported_extensions: BTreeSet<String>,
    failures: Vec<String>,
    entry_lookup: HashMap<(String, u16), usize>,
    changes: HashMap<(String, u16), ResourceChange>,
    dirty_before: bool,
    category_before: Option<String>,
}

#[derive(Clone)]
struct ResourceChange {
    key: (String, u16),
    before: Option<Entry>,
    after: Option<Entry>,
}

#[derive(Clone)]
enum ArchiveEdit {
    Resources(Vec<ResourceChange>),
    Description { before: String, after: String },
}

#[derive(Clone)]
struct EditTransaction {
    label: String,
    edit: ArchiveEdit,
    selected_before: BTreeSet<(String, u16)>,
    selected_after: BTreeSet<(String, u16)>,
    category_before: Option<String>,
    category_after: Option<String>,
    dirty_before: bool,
    dirty_after: bool,
    estimated_bytes: usize,
}

#[derive(Clone, Default)]
struct EditHistory {
    undo: VecDeque<EditTransaction>,
    redo: VecDeque<EditTransaction>,
    estimated_bytes: usize,
}

impl EditHistory {
    fn record(&mut self, transaction: EditTransaction) -> bool {
        self.clear_redo();
        if transaction.estimated_bytes > MAX_UNDO_BYTES {
            self.clear();
            return false;
        }
        self.estimated_bytes = self
            .estimated_bytes
            .saturating_add(transaction.estimated_bytes);
        self.undo.push_back(transaction);
        while self.undo.len() > MAX_UNDO_STEPS || self.estimated_bytes > MAX_UNDO_BYTES {
            let Some(expired) = self.undo.pop_front() else {
                break;
            };
            self.estimated_bytes = self.estimated_bytes.saturating_sub(expired.estimated_bytes);
        }
        true
    }

    fn clear_redo(&mut self) {
        while let Some(transaction) = self.redo.pop_front() {
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_sub(transaction.estimated_bytes);
        }
    }

    fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.estimated_bytes = 0;
    }

    fn undo_label(&self) -> Option<&str> {
        self.undo
            .back()
            .map(|transaction| transaction.label.as_str())
    }

    fn redo_label(&self) -> Option<&str> {
        self.redo
            .back()
            .map(|transaction| transaction.label.as_str())
    }
}

struct ModelCompileJob {
    receiver: mpsc::Receiver<ModelCompileEvent>,
    cancel: Arc<AtomicBool>,
    started_at: Instant,
    completed: usize,
    total: usize,
    phase: String,
    current: String,
}

enum ModelCompileEvent {
    Progress {
        completed: usize,
        phase: String,
        current: String,
    },
    Finished(ModelCompileOutcome),
}

struct ModelCompileOutcome {
    exported: usize,
    skipped: usize,
    canceled: bool,
    single_path: Option<PathBuf>,
    directory: Option<PathBuf>,
    report_path: Option<PathBuf>,
    fatal_error: Option<String>,
}

impl AddBatch {
    fn new(
        paths: Vec<PathBuf>,
        target_tab: usize,
        selected_keys: BTreeSet<(String, u16)>,
        archive: &Archive,
        dirty_before: bool,
        category_before: Option<String>,
    ) -> Self {
        let entry_lookup = archive
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| ((entry.name.to_ascii_lowercase(), entry.type_id), index))
            .collect();
        Self {
            target_tab,
            selected_keys,
            queue: paths.into(),
            conflict: None,
            policy: ConflictPolicy::Ask,
            added: 0,
            replaced: 0,
            skipped: 0,
            unsupported_files: 0,
            unsupported_extensions: BTreeSet::new(),
            failures: Vec::new(),
            entry_lookup,
            changes: HashMap::new(),
            dirty_before,
            category_before,
        }
    }
}

#[derive(Clone)]
struct TabState {
    archive: Archive,
    selected: BTreeSet<usize>,
    filter: String,
    dirty: bool,
    edit_history: EditHistory,
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
            edit_history: EditHistory::default(),
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
        let stored_nwn_installation = cc
            .storage
            .and_then(|storage| eframe::get_value::<String>(storage, "nwn_installation"))
            .map(PathBuf::from)
            .and_then(|path| normalize_nwn_installation(&path));
        let nwn_installation =
            stored_nwn_installation.or_else(|| discover_nwn_installations(None).into_iter().next());
        cc.egui_ctx.set_theme(appearance.preference());
        Self {
            archive: None,
            selected: BTreeSet::new(),
            filter: String::new(),
            status: "Ready — open an archive or create a new one".into(),
            error: None,
            warning: None,
            dirty: false,
            edit_history: EditHistory::default(),
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
            hovered_drop_file_count: 0,
            hovered_drop_unsupported_count: 0,
            hovered_drop_unsupported_extensions: BTreeSet::new(),
            pending_drop_files: Vec::new(),
            incoming_paths: None,
            internal_drag_origins: BTreeMap::new(),
            tab_drop_rects: Vec::new(),
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
            nwn_installation,
            resource_middle_scroll_anchor: None,
            resource_scroll_reset_pending: false,
            image_preview: None,
            model_preview: None,
            model_view: ModelView::Model,
            model_yaw: -0.65,
            model_pitch: 0.35,
            model_zoom: 1.0,
            texture_dependency_directory: None,
            texture_dependencies: BTreeMap::new(),
            texture_dependency_receiver: None,
            resource_view_cache: None,
            model_compiler: None,
            model_compile_job: None,
            quit_after_model_compile: false,
            #[cfg(target_os = "linux")]
            dnd_extract: dnd_extract::Bridge::new()
                .map_err(|error| {
                    eprintln!("Could not enable KDE archive drag support: {error}");
                    error
                })
                .ok(),
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

    fn load_model_textures(
        &mut self,
        context: &egui::Context,
        scene: &mdl::Scene,
        cache_key: &str,
    ) -> BTreeMap<String, ModelTexture> {
        if self.archive.is_none() {
            return BTreeMap::new();
        }
        self.ensure_texture_dependencies(context);
        // Character-part models commonly name a generic engine texture such
        // as M_Helmet while the matching PLT uses the model resref. This is
        // NWN Explorer's "model name texture" behavior and is required when
        // only the self-named PLT is present in the open archive.
        let mut names = BTreeMap::from([(scene.name.to_ascii_lowercase(), None)]);
        for mesh in &scene.meshes {
            if let Some(name) = &mesh.texture_name {
                let (name, requested_extension) = name.rsplit_once('.').map_or_else(
                    || (name.as_str(), None),
                    |(stem, extension)| (stem, Some(extension.to_ascii_lowercase())),
                );
                let name = name.to_ascii_lowercase();
                if !name.is_empty() && name != "null" {
                    names.insert(name, requested_extension);
                }
            }
        }
        // Enhanced Edition models frequently reference an MTR rather than an
        // image directly. Resolve its diffuse texture0 before looking for the
        // actual TGA/DDS/PLT, including MTRs found in sibling HAKs.
        let material_names: Vec<String> = names.keys().cloned().collect();
        let mut material_aliases = BTreeMap::new();
        for material_name in material_names {
            let active = self.archive.iter().flat_map(|archive| &archive.entries);
            let open_tabs = self.tabs.iter().flat_map(|tab| &tab.archive.entries);
            let dependencies = self
                .texture_dependencies
                .get(&material_name)
                .into_iter()
                .flatten();
            let material = active.chain(open_tabs).chain(dependencies).find(|entry| {
                entry.name.eq_ignore_ascii_case(&material_name)
                    && entry.extension().eq_ignore_ascii_case("mtr")
            });
            let Some(texture_name) = material
                .and_then(|entry| {
                    entry
                        .size()
                        .ok()
                        .filter(|size| *size <= 1024 * 1024)
                        .map(|size| (entry, size))
                })
                .and_then(|(entry, size)| entry.read_prefix(size).ok())
                .and_then(|bytes| mtr_diffuse_texture(&bytes))
            else {
                continue;
            };
            let texture_name = texture_name.to_ascii_lowercase();
            names.entry(texture_name.clone()).or_insert(None);
            material_aliases.insert(material_name, texture_name);
        }
        let texture_rank = |extension: &str| match extension {
            "plt" => 0,
            "dds" => 1,
            "tga" => 2,
            "png" => 3,
            "jpg" | "jpeg" => 4,
            _ => usize::MAX,
        };
        let mut textures = BTreeMap::new();
        for (name, requested_extension) in names {
            let active = self.archive.iter().flat_map(|archive| &archive.entries);
            let open_tabs = self.tabs.iter().flat_map(|tab| &tab.archive.entries);
            let dependencies = self.texture_dependencies.get(&name).into_iter().flatten();
            let Some(entry) = active
                .chain(open_tabs)
                .chain(dependencies)
                .filter(|entry| entry.name.eq_ignore_ascii_case(&name))
                .filter(|entry| texture_rank(&entry.extension()) != usize::MAX)
                .min_by_key(|entry| {
                    let extension = entry.extension();
                    (
                        requested_extension
                            .as_deref()
                            .is_none_or(|requested| requested != extension),
                        texture_rank(&extension),
                    )
                })
            else {
                continue;
            };
            let size = match entry.size() {
                Ok(size) if size <= MAX_IMAGE_FILE_SIZE => size,
                _ => continue,
            };
            let extension = entry.extension();
            let decoded = entry
                .read_prefix(size)
                .map_err(|error| error.to_string())
                .and_then(|bytes| decode_preview_image(&bytes, &extension));
            let Ok(decoded) = decoded else {
                continue;
            };
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
            let texture = context.load_texture(
                format!("{cache_key}:texture:{name}"),
                color_image,
                egui::TextureOptions::LINEAR_REPEAT,
            );
            textures.insert(
                name,
                ModelTexture {
                    handle: texture,
                    // NWN Explorer uploads PLT and DDS scanlines directly.
                    // TGA and conventional web images are normalized to a
                    // top-left origin by the image decoder and need flipping
                    // to retain OpenGL-style model texture coordinates.
                    flip_vertical: matches!(extension.as_str(), "tga" | "png" | "jpg" | "jpeg"),
                },
            );
        }
        for (material_name, texture_name) in material_aliases {
            if let Some(texture) = textures.get(&texture_name).cloned() {
                textures.insert(material_name, texture);
            }
        }
        textures
    }

    fn ensure_texture_dependencies(&mut self, context: &egui::Context) {
        let directory = self
            .archive
            .as_ref()
            .and_then(|archive| archive.path.as_ref())
            .and_then(|path| path.parent())
            .map(PathBuf::from);
        if self.texture_dependency_directory == directory {
            return;
        }
        self.texture_dependency_directory = directory.clone();
        self.texture_dependencies.clear();
        self.texture_dependency_receiver = None;
        let Some(directory) = directory else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        self.texture_dependency_receiver = Some(receiver);
        let repaint = context.clone();
        thread::spawn(move || {
            let mut dependencies = TextureDependencies::new();
            if let Ok(files) = fs::read_dir(&directory) {
                for path in files.filter_map(Result::ok).map(|entry| entry.path()) {
                    if path.extension().is_none_or(|extension| {
                        !extension.to_string_lossy().eq_ignore_ascii_case("hak")
                    }) {
                        continue;
                    }
                    let Ok(archive) = Archive::open(&path) else {
                        continue;
                    };
                    for entry in archive.entries {
                        let extension = entry.extension();
                        if !matches!(
                            extension.as_str(),
                            "plt" | "dds" | "tga" | "png" | "jpg" | "jpeg" | "mtr"
                        ) {
                            continue;
                        }
                        let matches = dependencies
                            .entry(entry.name.to_ascii_lowercase())
                            .or_default();
                        // Only the first resource of a given name and format can win the
                        // existing texture preference order. Avoid retaining duplicates
                        // from every sibling HAK.
                        if !matches
                            .iter()
                            .any(|existing| existing.extension() == extension)
                        {
                            matches.push(entry);
                        }
                    }
                }
            }
            let _ = sender.send((directory, dependencies));
            repaint.request_repaint();
        });
    }

    fn poll_texture_dependencies(&mut self) {
        let result = self
            .texture_dependency_receiver
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        if let Some((directory, dependencies)) = result {
            self.texture_dependency_receiver = None;
            if self.texture_dependency_directory.as_ref() == Some(&directory) {
                self.texture_dependencies = dependencies;
                self.model_preview = None;
            }
        }
    }

    fn resource_view(&mut self) -> Option<(ResourceSummary, Arc<[usize]>)> {
        let archive = self.archive.as_ref()?;
        let archive_key = archive.view_key();
        let category = (!self.compact_mode)
            .then(|| self.category.clone())
            .flatten();
        let stale = self.resource_view_cache.as_ref().is_none_or(|cache| {
            cache.archive_key != archive_key
                || cache.filter != self.filter
                || cache.category != category
                || cache.compact_mode != self.compact_mode
                || cache.sort_column != self.sort_column
                || cache.sort_ascending != self.sort_ascending
        });
        if stale {
            let mut categories = BTreeMap::<String, usize>::new();
            let mut compiled_models = 0;
            let mut uncompiled_models = 0;
            let mut new_count = 0;
            let mut bytes = 0_u64;
            for entry in &archive.entries {
                *categories
                    .entry(category_for(&entry.extension()).to_owned())
                    .or_default() += 1;
                match entry.model_compiled() {
                    Some(true) => compiled_models += 1,
                    Some(false) => uncompiled_models += 1,
                    None => {}
                }
                new_count += usize::from(entry.is_new());
                bytes = bytes.saturating_add(entry.size().unwrap_or(0));
            }
            let summary = ResourceSummary {
                categories,
                compiled_models,
                uncompiled_models,
                new_count,
                name: archive
                    .path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("Untitled")
                    .to_owned(),
                count: archive.entries.len(),
                bytes,
                version: archive.version_label(),
                kind: String::from_utf8_lossy(archive.kind.signature())
                    .trim()
                    .to_owned(),
            };
            let filter = self.filter.to_ascii_lowercase();
            let mut visible_indices: Vec<usize> = archive
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    let extension = entry.extension();
                    resource_matches_filter(&entry.name, &extension, &filter)
                        && entry_matches_category(entry, category.as_deref())
                })
                .map(|(index, _)| index)
                .collect();
            match self.sort_column {
                SortColumn::Name => visible_indices
                    .sort_by_cached_key(|index| archive.entries[*index].name.to_ascii_lowercase()),
                SortColumn::Type => visible_indices.sort_by_cached_key(|index| {
                    let entry = &archive.entries[*index];
                    (entry.extension(), entry.name.to_ascii_lowercase())
                }),
                SortColumn::Size => visible_indices.sort_by_cached_key(|index| {
                    let entry = &archive.entries[*index];
                    (entry.size().unwrap_or(0), entry.name.to_ascii_lowercase())
                }),
            }
            if !self.sort_ascending {
                visible_indices.reverse();
            }
            self.resource_view_cache = Some(ResourceViewCache {
                archive_key,
                filter: self.filter.clone(),
                category,
                compact_mode: self.compact_mode,
                sort_column: self.sort_column,
                sort_ascending: self.sort_ascending,
                summary,
                visible_indices: visible_indices.into(),
            });
        }
        let cache = self.resource_view_cache.as_ref().unwrap();
        Some((cache.summary.clone(), Arc::clone(&cache.visible_indices)))
    }

    fn show_model_preview(
        &mut self,
        ui: &mut egui::Ui,
        entry: &archive::Entry,
    ) -> Option<ModelExportAction> {
        let mut request_export = None;
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
            let bytes = entry
                .read_prefix(size.min(MAX_MODEL_RENDER_BYTES))
                .map_err(|error| format!("Could not read model: {error}"));
            let result = bytes.as_ref().map_err(Clone::clone).and_then(|bytes| {
                decode_model_preview(
                    &bytes[..bytes.len().min(MAX_MODEL_PREVIEW_BYTES as usize)],
                    size,
                )
            });
            let scene = if size > MAX_MODEL_RENDER_BYTES {
                Err(format!(
                    "Model rendering is limited to {} resources",
                    human_size(MAX_MODEL_RENDER_BYTES)
                ))
            } else {
                bytes
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|bytes| mdl::parse_scene(bytes))
            };
            let textures = scene
                .as_ref()
                .map(|scene| self.load_model_textures(ui.ctx(), scene, &key))
                .unwrap_or_default();
            self.model_preview = Some(ModelPreviewCache {
                key,
                result,
                scene,
                textures,
            });
        }

        let cache = self.model_preview.as_ref().unwrap();
        match &cache.result {
            Ok(ModelPreview::Uncompiled { text, truncated }) => {
                ui.label(RichText::new("Model Uncompiled").size(16.0).strong());
                ui.label("ASCII model source");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.model_view, ModelView::Model, "View Model");
                    ui.selectable_value(&mut self.model_view, ModelView::Source, "Source");
                });
                if *truncated {
                    ui.small(format!(
                        "Preview limited to the first {}",
                        human_size(MAX_MODEL_PREVIEW_BYTES)
                    ));
                }
                ui.separator();
                if self.model_view == ModelView::Model {
                    if ui.button("Compile and Export…").clicked() {
                        request_export = Some(ModelExportAction::Compile);
                    }
                    ui.add_space(4.0);
                    show_model_scene(
                        ui,
                        &cache.scene,
                        &cache.textures,
                        &mut self.model_yaw,
                        &mut self.model_pitch,
                        &mut self.model_zoom,
                    );
                } else {
                    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true));
                    });
                }
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
                    ui.selectable_value(&mut self.model_view, ModelView::Model, "View Model");
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
                    ModelView::Model => {
                        if ui.button("Decompile and Export…").clicked() {
                            request_export = Some(ModelExportAction::Decompile);
                        }
                        ui.add_space(4.0);
                        show_model_scene(
                            ui,
                            &cache.scene,
                            &cache.textures,
                            &mut self.model_yaw,
                            &mut self.model_pitch,
                            &mut self.model_zoom,
                        );
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
                    ModelView::Source => {
                        self.model_view = ModelView::Model;
                    }
                }
            }
            Err(error) => {
                ui.label(RichText::new("Model format unknown").size(16.0).strong());
                ui.colored_label(Color32::LIGHT_RED, error);
            }
        }
        request_export
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
            edit_history: self.edit_history.clone(),
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
            || self.model_compile_job.is_some()
            || self.show_new
            || self.show_description
            || self.show_about
            || self.error.is_some()
            || self.warning.is_some()
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

    fn record_edit(&mut self, mut transaction: EditTransaction) -> bool {
        transaction.estimated_bytes = estimate_transaction_bytes(&transaction);
        self.edit_history.record(transaction)
    }

    fn record_add_batch(&mut self, batch: &mut AddBatch) -> bool {
        let changed = batch.added + batch.replaced;
        let transaction = resource_transaction(
            format!("import of {changed} resource(s)"),
            std::mem::take(&mut batch.changes).into_values().collect(),
            batch.selected_keys.clone(),
            self.selected_resource_keys(),
            batch.category_before.clone(),
            self.category.clone(),
            batch.dirty_before,
        );
        self.record_edit(transaction)
    }

    fn undo_edit(&mut self) {
        let Some(transaction) = self.edit_history.undo.pop_back() else {
            return;
        };
        match self.apply_edit_transaction(&transaction, false) {
            Ok(()) => {
                self.status = format!("Undid {}", transaction.label);
                self.edit_history.redo.push_back(transaction);
            }
            Err(error) => {
                self.edit_history.undo.push_back(transaction);
                self.error = Some(format!("Could not undo the edit: {error}"));
            }
        }
    }

    fn redo_edit(&mut self) {
        let Some(transaction) = self.edit_history.redo.pop_back() else {
            return;
        };
        match self.apply_edit_transaction(&transaction, true) {
            Ok(()) => {
                self.status = format!("Redid {}", transaction.label);
                self.edit_history.undo.push_back(transaction);
            }
            Err(error) => {
                self.edit_history.redo.push_back(transaction);
                self.error = Some(format!("Could not redo the edit: {error}"));
            }
        }
    }

    fn apply_edit_transaction(
        &mut self,
        transaction: &EditTransaction,
        redo: bool,
    ) -> Result<(), String> {
        let Some(archive) = self.archive.as_mut() else {
            return Err("no archive is open".into());
        };
        apply_archive_edit(archive, &transaction.edit, redo)?;
        self.dirty = if redo {
            transaction.dirty_after
        } else {
            transaction.dirty_before
        };
        self.category = if redo {
            transaction.category_after.clone()
        } else {
            transaction.category_before.clone()
        };
        let selected = if redo {
            &transaction.selected_after
        } else {
            &transaction.selected_before
        };
        self.restore_selection_by_keys(selected);
        self.resource_view_cache = None;
        self.image_preview = None;
        self.model_preview = None;
        Ok(())
    }
    fn load_tab(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index).cloned() else {
            return;
        };
        self.archive = Some(tab.archive);
        self.selected = tab.selected;
        self.filter = tab.filter;
        self.dirty = tab.dirty;
        self.edit_history = tab.edit_history;
        self.category = tab.category;
        self.sort_column = tab.sort_column;
        self.sort_ascending = tab.sort_ascending;
        self.selection_anchor = tab.selection_anchor;
        self.selection_cursor = tab.selection_cursor;
        self.active_tab = Some(index);
        self.typeahead.clear();
        self.typeahead_pending = false;
        self.resource_middle_scroll_anchor = None;
        self.image_preview = None;
    }

    fn set_category(&mut self, category: Option<String>) {
        if self.category != category {
            self.category = category;
            self.resource_middle_scroll_anchor = None;
            self.resource_scroll_reset_pending = true;
        }
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
            self.edit_history.clear();
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
        if let Some(mut batch) = self.pending_add.take() {
            if batch.added + batch.replaced > 0 {
                if let Some(archive) = self.archive.as_mut() {
                    archive.finish_bulk_add();
                }
                self.dirty = true;
                let selected_keys = batch.selected_keys.clone();
                self.restore_selection_by_keys(&selected_keys);
                let _ = self.record_add_batch(&mut batch);
            }
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
                let unsupported = unsupported_archive_resource_summary(&archive);
                self.sync_current_tab();
                self.tabs.push(TabState::new(archive, false));
                self.load_tab(self.tabs.len() - 1);
                self.resource_scroll_reset_pending = true;
                self.status = format!("Opened {} — {count} resources", path.display());
                self.remember_recent_archive(&path);
                if let Some(unsupported) = unsupported {
                    self.warning = Some(format!(
                        "This archive contains {unsupported} outside the NWN/NWN:EE import allowlist. They remain available to inspect and extract, but cannot be added to or merged into a new archive."
                    ));
                }
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
            .add_filter("NWN archives", &["hak", "erf", "mod", "sav", "bif"])
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
        if !archive.kind.is_editable() {
            self.status = "BIF archives are read-only; resources can still be extracted".into();
            return;
        }
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
                self.edit_history.clear();
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
    fn filter_same_archive_drag_paths(&mut self, paths: Vec<PathBuf>) -> (Vec<PathBuf>, usize) {
        let target_tab = self.active_tab;
        let mut kept = Vec::with_capacity(paths.len());
        let mut skipped = 0;
        for path in paths {
            let same_archive = path
                .parent()
                .and_then(|directory| self.internal_drag_origins.get(directory))
                .is_some_and(|origin| internal_drag_returns_to_source(origin, target_tab));
            if same_archive {
                skipped += 1;
            } else {
                kept.push(path);
            }
        }
        self.internal_drag_origins
            .retain(|directory, _| directory.is_dir());
        (kept, skipped)
    }
    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() || self.archive.is_none() {
            return;
        }
        if self
            .archive
            .as_ref()
            .is_some_and(|archive| !archive.kind.is_editable())
        {
            self.status = "BIF archives are read-only; resources cannot be added".into();
            return;
        }
        let (mut paths, same_archive_skipped) = self.filter_same_archive_drag_paths(paths);
        let mut unsupported_extensions = BTreeSet::new();
        let original_path_count = paths.len();
        paths.retain(|path| {
            let Some(extension) = unsupported_import_extension(path) else {
                return true;
            };
            unsupported_extensions.insert(extension);
            false
        });
        let unsupported_files = original_path_count - paths.len();
        if unsupported_files > 0 {
            self.error = Some(format!(
                "Skipped {unsupported_files} file(s) that are not valid NWN/NWN:EE resources: {}",
                unsupported_extensions
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if paths.is_empty() {
            self.status = if unsupported_extensions.is_empty() {
                format!(
                    "Skipped {same_archive_skipped} resource(s) already present in this archive"
                )
            } else {
                format!(
                    "Skipped {unsupported_files} unsupported resource(s) ({})",
                    unsupported_extensions
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            return;
        }
        if let Some(batch) = self.pending_add.as_mut() {
            if self.active_tab == Some(batch.target_tab) {
                batch.queue.extend(paths);
                batch.skipped += same_archive_skipped + unsupported_files;
                batch.unsupported_files += unsupported_files;
                batch.unsupported_extensions.extend(unsupported_extensions);
            } else {
                self.pending_drop_files.extend(paths);
            }
            return;
        }
        let Some(target_tab) = self.active_tab else {
            return;
        };
        let selected_keys = self.selected_resource_keys();
        let Some(archive) = self.archive.as_ref() else {
            return;
        };
        let mut batch = AddBatch::new(
            paths,
            target_tab,
            selected_keys,
            archive,
            self.dirty,
            self.category.clone(),
        );
        batch.skipped = same_archive_skipped + unsupported_files;
        batch.unsupported_files = unsupported_files;
        batch.unsupported_extensions = unsupported_extensions;
        self.pending_add = Some(batch);
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
                ConflictAction::Replace => {
                    add_incoming(
                        archive,
                        &mut batch,
                        conflict.path,
                        conflict.entry,
                        Some(conflict.replacement_index),
                    );
                }
                ConflictAction::ReplaceAll => {
                    batch.policy = ConflictPolicy::ReplaceAll;
                    add_incoming(
                        archive,
                        &mut batch,
                        conflict.path,
                        conflict.entry,
                        Some(conflict.replacement_index),
                    );
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

        let started = Instant::now();
        let mut processed = 0;
        while !canceled
            && batch.conflict.is_none()
            && processed < IMPORT_MAX_FILES_PER_FRAME
            && (processed < IMPORT_MIN_FILES_PER_FRAME || started.elapsed() < IMPORT_FRAME_BUDGET)
        {
            let Some(path) = batch.queue.pop_front() else {
                break;
            };
            processed += 1;
            match archive.prepare_incoming_file(&path) {
                Ok(entry) => {
                    let key = Archive::incoming_entry_identity(&entry);
                    match batch.entry_lookup.get(&key).copied() {
                        Some(replacement_index) => match batch.policy {
                            ConflictPolicy::Ask => {
                                batch.conflict = Some(AddConflict {
                                    path,
                                    entry,
                                    existing_filename: archive.entries[replacement_index]
                                        .filename(),
                                    replacement_index,
                                });
                            }
                            ConflictPolicy::ReplaceAll => {
                                add_incoming(
                                    archive,
                                    &mut batch,
                                    path,
                                    entry,
                                    Some(replacement_index),
                                );
                            }
                            ConflictPolicy::SkipAll => batch.skipped += 1,
                        },
                        None => {
                            let new_index = archive.entries.len();
                            if add_incoming(archive, &mut batch, path, entry, None) {
                                batch.entry_lookup.insert(key, new_index);
                            }
                        }
                    }
                }
                Err(error) => batch.failures.push(format!("{}: {error}", path.display())),
            }
        }

        let changed = batch.added + batch.replaced > 0;
        let finished = canceled || (batch.conflict.is_none() && batch.queue.is_empty());
        if finished && changed {
            archive.finish_bulk_add();
        }
        self.dirty |= changed;
        if changed {
            self.image_preview = None;
        }
        if batch.conflict.is_some() {
            self.status = format!(
                "Added {}, replaced {}, skipped {} — waiting for overwrite choice",
                batch.added, batch.replaced, batch.skipped
            );
            self.pending_add = Some(batch);
        } else if !finished {
            self.status = format!(
                "Importing resources — added {}, replaced {}, skipped {}, {} remaining",
                batch.added,
                batch.replaced,
                batch.skipped,
                batch.queue.len()
            );
            self.pending_add = Some(batch);
        } else {
            if changed {
                let selected_keys = batch.selected_keys.clone();
                self.restore_selection_by_keys(&selected_keys);
            }
            let history_recorded = !changed || self.record_add_batch(&mut batch);
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
            if batch.unsupported_files > 0 {
                self.status.push_str(&format!(
                    " — {} unsupported ({})",
                    batch.unsupported_files,
                    batch
                        .unsupported_extensions
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !batch.failures.is_empty() {
                self.error = Some(summarize_import_failures(&batch.failures));
            }
            if !history_recorded {
                self.status
                    .push_str(" — operation exceeded the bounded undo-history limit");
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
        if self
            .archive
            .as_ref()
            .is_some_and(|archive| !archive.kind.is_editable())
        {
            self.status = "BIF archives are read-only; they cannot be modified".into();
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("NWN archives", &["hak", "erf", "mod", "sav", "bif"])
            .pick_file()
        else {
            return;
        };
        match Archive::open(&path) {
            Ok(other) => {
                let selected_keys = self.selected_resource_keys();
                let dirty_before = self.dirty;
                let category_before = self.category.clone();
                let allowed_entries = other
                    .entries
                    .iter()
                    .filter(|entry| resource_types::is_nwn_ee_type(entry.type_id))
                    .collect::<Vec<_>>();
                let unsupported_entries = other.entries.len() - allowed_entries.len();
                let Some(current) = self.archive.as_mut() else {
                    return;
                };
                let mut changes = BTreeMap::new();
                for incoming in &allowed_entries {
                    let key = (incoming.name.to_ascii_lowercase(), incoming.type_id);
                    let before = current
                        .entries
                        .iter()
                        .find(|entry| {
                            entry.type_id == key.1 && entry.name.eq_ignore_ascii_case(&key.0)
                        })
                        .cloned();
                    changes
                        .entry(key.clone())
                        .and_modify(|change: &mut ResourceChange| {
                            change.after = Some((*incoming).clone());
                        })
                        .or_insert(ResourceChange {
                            key,
                            before,
                            after: Some((*incoming).clone()),
                        });
                }
                let (added, replaced) = current.merge_entries(allowed_entries);
                self.dirty |= added + replaced > 0;
                if added + replaced > 0 {
                    self.image_preview = None;
                    self.restore_selection_by_keys(&selected_keys);
                }
                self.status = format!(
                    "Merged {}: {added} added, {replaced} replaced",
                    path.display()
                );
                if unsupported_entries > 0 {
                    self.status.push_str(&format!(
                        " — skipped {unsupported_entries} non-NWN/EE resource(s)"
                    ));
                }
                if added + replaced > 0 {
                    let transaction = resource_transaction(
                        format!("merge of {} resource(s)", added + replaced),
                        changes.into_values().collect(),
                        selected_keys,
                        self.selected_resource_keys(),
                        category_before,
                        self.category.clone(),
                        dirty_before,
                    );
                    if !self.record_edit(transaction) {
                        self.status
                            .push_str(" — operation exceeded the bounded undo-history limit");
                    }
                }
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

    fn export_selected_models(&mut self, action: ModelExportAction) {
        match action {
            ModelExportAction::Compile => self.compile_selected_models(),
            ModelExportAction::Decompile => self.decompile_selected_models(),
        }
    }

    fn compile_selected_models(&mut self) {
        if self.model_compile_job.is_some() {
            return;
        }
        let Some(archive) = self.archive.as_ref() else {
            return;
        };
        let model_indices: Vec<usize> = self
            .selected
            .iter()
            .copied()
            .filter(|index| {
                archive.entries.get(*index).is_some_and(|entry| {
                    entry.extension() == "mdl"
                        && model_export_action(entry) == Ok(ModelExportAction::Compile)
                })
            })
            .collect();
        if model_indices.is_empty() {
            return;
        }
        let destination = if model_indices.len() == 1 {
            let entry = &archive.entries[model_indices[0]];
            rfd::FileDialog::new()
                .add_filter("Aurora compiled model", &["mdl"])
                .set_file_name(entry.filename())
                .save_file()
                .map(|path| (Some(path), None))
        } else {
            rfd::FileDialog::new()
                .pick_folder()
                .map(|path| (None, Some(path)))
        };
        let Some((single_path, directory)) = destination else {
            return;
        };
        if self.model_compiler.is_none() {
            self.model_compiler = Some(model_compiler());
        }
        let compiler = match self.model_compiler.as_ref().expect("compiler initialized") {
            Ok(compiler) => compiler.path.clone(),
            Err(error) => {
                self.fail("Could not compile model", error.clone());
                return;
            }
        };

        let request = ModelCompileRequest {
            archive: archive.clone(),
            tabs: self.tabs.clone(),
            nwn_installation: self.nwn_installation.clone(),
            model_indices,
            compiler,
            single_path,
            directory,
        };
        let total = request.model_indices.len();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || compile_models_worker(request, worker_cancel, sender));
        self.model_compile_job = Some(ModelCompileJob {
            receiver,
            cancel,
            started_at: Instant::now(),
            completed: 0,
            total,
            phase: "Preparing models".into(),
            current: String::new(),
        });
        self.status = format!("Compiling {total} model(s)…");
    }

    fn poll_model_compile_job(&mut self) {
        let (events, disconnected) = self.model_compile_job.as_ref().map_or_else(
            || (Vec::new(), false),
            |job| {
                let mut events = Vec::new();
                loop {
                    match job.receiver.try_recv() {
                        Ok(event) => events.push(event),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => return (events, true),
                    }
                }
                (events, false)
            },
        );
        for event in events {
            match event {
                ModelCompileEvent::Progress {
                    completed,
                    phase,
                    current,
                } => {
                    if let Some(job) = self.model_compile_job.as_mut() {
                        job.completed = completed;
                        job.phase = phase;
                        job.current = current;
                    }
                }
                ModelCompileEvent::Finished(outcome) => {
                    self.model_compile_job = None;
                    if let Some(error) = outcome.fatal_error {
                        self.fail("Could not compile model", error);
                    } else if outcome.canceled {
                        self.status = format!(
                            "Compilation canceled — {} model(s) completed",
                            outcome.exported
                        );
                    } else if let Some(path) = outcome.single_path {
                        self.status = format!("Compiled model to {}", path.display());
                    } else if let Some(directory) = outcome.directory {
                        if outcome.skipped == 0 {
                            self.status = format!(
                                "Compiled {} models to {}",
                                outcome.exported,
                                directory.display()
                            );
                        } else if let Some(report_path) = outcome.report_path {
                            self.status = format!(
                                "Compiled {} models, skipped {} — report: {}",
                                outcome.exported,
                                outcome.skipped,
                                report_path.display()
                            );
                            self.error = Some(format!(
                                "Compilation completed with skipped models.\n\nCompiled: {}\nSkipped: {}\n\nFailure report:\n{}",
                                outcome.exported,
                                outcome.skipped,
                                report_path.display()
                            ));
                        }
                    }
                }
            }
        }
        if disconnected && self.model_compile_job.is_some() {
            self.model_compile_job = None;
            self.fail(
                "Could not compile model",
                "the background compilation worker stopped unexpectedly",
            );
        }
        if self.model_compile_job.is_none() && self.quit_after_model_compile {
            self.quit_after_model_compile = false;
            self.error = None;
            self.request_quit();
        }
    }

    fn decompile_selected_models(&mut self) {
        let Some(archive) = self.archive.as_ref() else {
            return;
        };
        let model_indices: Vec<usize> = self
            .selected
            .iter()
            .copied()
            .filter(|index| {
                archive.entries.get(*index).is_some_and(|entry| {
                    entry.extension() == "mdl"
                        && model_export_action(entry) == Ok(ModelExportAction::Decompile)
                })
            })
            .collect();
        if model_indices.is_empty() {
            return;
        }
        let destination = if model_indices.len() == 1 {
            let entry = &archive.entries[model_indices[0]];
            rfd::FileDialog::new()
                .add_filter("Aurora ASCII model", &["mdl"])
                .set_file_name(entry.filename())
                .save_file()
                .map(|path| (Some(path), None))
        } else {
            rfd::FileDialog::new()
                .pick_folder()
                .map(|path| (None, Some(path)))
        };
        let Some((single_path, directory)) = destination else {
            return;
        };
        let mut exported = 0usize;
        let mut failure = None;
        for index in model_indices {
            let entry = &archive.entries[index];
            let result = entry
                .size()
                .map_err(|error| error.to_string())
                .and_then(|size| {
                    if size > MAX_MODEL_RENDER_BYTES {
                        Err(format!(
                            "{} exceeds the {} decompilation limit",
                            entry.filename(),
                            human_size(MAX_MODEL_RENDER_BYTES)
                        ))
                    } else {
                        entry.read_prefix(size).map_err(|error| error.to_string())
                    }
                })
                .and_then(|bytes| mdl::decompile(&bytes))
                .and_then(|text| {
                    let path = single_path
                        .clone()
                        .unwrap_or_else(|| directory.as_ref().unwrap().join(entry.filename()));
                    fs::write(&path, text)
                        .map(|()| path)
                        .map_err(|error| error.to_string())
                });
            match result {
                Ok(_) => exported += 1,
                Err(error) => {
                    failure = Some(format!("{}: {error}", entry.filename()));
                    break;
                }
            }
        }
        if let Some(error) = failure {
            self.fail("Could not decompile model", error);
        } else if let Some(path) = single_path {
            self.status = format!("Decompiled model to {}", path.display());
        } else if let Some(directory) = directory {
            self.status = format!("Decompiled {exported} models to {}", directory.display());
        }
    }
    fn drag_selected(&mut self, frame: &eframe::Frame) {
        let Some(archive) = self.archive.as_ref() else {
            return;
        };
        let Some(source_tab) = self.active_tab else {
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
        drag_cleanup::register(directory.path(), drag_cleanup::UNCONFIRMED_DRAG_RETENTION);
        let mut resources = Vec::with_capacity(self.selected.len());
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
            resources.push((entry.clone(), path));
        }
        if resources.is_empty() {
            return;
        }
        let count = resources.len();
        #[cfg(target_os = "windows")]
        if let Err(error) = archive::export_entries_parallel(&resources) {
            self.fail("Could not prepare resources for dragging", error);
            return;
        }
        let paths = resources
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<Vec<_>>();
        self.internal_drag_origins.insert(
            directory.path().to_path_buf(),
            InternalDragOrigin { source_tab },
        );
        prune_internal_drag_origins(&mut self.internal_drag_origins);
        drag_out::release_pointer_grab(frame);
        #[cfg(target_os = "linux")]
        {
            let archive_offer = self.dnd_extract.as_ref().map(|bridge| {
                bridge.set_entries(resources.iter().map(|(entry, _)| entry.clone()).collect());
                drag_out::ArchiveExtractOffer {
                    service: bridge.service().to_owned(),
                    path: bridge.path().to_owned(),
                }
            });
            drag_out::start(frame, paths, resources, directory, archive_offer);
        }
        #[cfg(target_os = "windows")]
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
        if cut && !archive.kind.is_editable() {
            self.status = "BIF archives are read-only; use Copy instead of Cut".into();
            return;
        }
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
        drag_cleanup::register(directory.path(), Duration::ZERO);
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
        let Some(archive) = self.archive.as_ref() else {
            return;
        };
        if !archive.kind.is_editable() {
            self.status = "BIF archives are read-only; resources cannot be removed".into();
            return;
        }
        if self.selected.is_empty() {
            return;
        }
        let selected_before = self.selected_resource_keys();
        let category_before = self.category.clone();
        let dirty_before = self.dirty;
        let changes = self
            .selected
            .iter()
            .filter_map(|index| archive.entries.get(*index))
            .map(|entry| ResourceChange {
                key: (entry.name.to_ascii_lowercase(), entry.type_id),
                before: Some(entry.clone()),
                after: None,
            })
            .collect::<Vec<_>>();
        let count = self.selected.len();
        let archive = self.archive.as_mut().unwrap();
        archive.entries = archive
            .entries
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.selected.contains(i))
            .map(|(_, e)| e.clone())
            .collect();
        archive.mark_resources_changed();
        self.selected.clear();
        self.selection_anchor = None;
        self.selection_cursor = None;
        self.category = None;
        self.dirty = true;
        self.status = format!("Removed {count} resources — save to commit changes");
        let transaction = resource_transaction(
            format!("deletion of {count} resource(s)"),
            changes,
            selected_before,
            BTreeSet::new(),
            category_before,
            None,
            dirty_before,
        );
        if !self.record_edit(transaction) {
            self.status
                .push_str(" — operation exceeded the bounded undo-history limit");
        }
    }
    fn new_archive(&mut self) {
        let archive = Archive::new(self.new_kind, self.new_version);
        self.sync_current_tab();
        self.tabs.push(TabState::new(archive, true));
        self.load_tab(self.tabs.len() - 1);
        self.resource_scroll_reset_pending = true;
        self.show_new = false;
        self.status = "Created a new unsaved archive".into();
    }

    fn choose_nwn_installation(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("Choose Neverwinter Nights installation");
        if let Some(path) = self.nwn_installation.as_ref() {
            dialog = dialog.set_directory(path);
        }
        let Some(path) = dialog.pick_folder() else {
            return;
        };
        if let Some(path) = normalize_nwn_installation(&path) {
            self.status = format!("Using Neverwinter Nights installation: {}", path.display());
            self.nwn_installation = Some(path);
            self.model_preview = None;
        } else {
            self.error = Some(format!(
                "{} does not look like a Neverwinter Nights installation. Choose the folder containing the data directory and NWN .key files.",
                path.display()
            ));
        }
    }

    fn auto_detect_nwn_installation(&mut self) {
        let detected = discover_nwn_installations(None);
        if let Some(path) = detected.into_iter().next() {
            self.status = format!(
                "Detected Neverwinter Nights installation: {}",
                path.display()
            );
            self.nwn_installation = Some(path);
            self.model_preview = None;
        } else {
            self.error = Some(
                "No Neverwinter Nights installation was found in Steam, GOG, Beamdog, or the standard installation folders. You can choose it manually from Tools > NWN installation."
                    .into(),
            );
        }
    }
    fn active_theme(&self, ctx: &egui::Context) -> egui::Theme {
        match self.appearance {
            Appearance::System => ctx.system_theme().unwrap_or(egui::Theme::Dark),
            Appearance::Dark => egui::Theme::Dark,
            Appearance::Light => egui::Theme::Light,
        }
    }
    fn title(&self) -> String {
        let Some(archive) = self.archive.as_ref() else {
            return "Aurora Hak Explorer".to_owned();
        };
        let name = archive
            .path
            .as_ref()
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
        save_nwn_installation(storage, self.nwn_installation.as_deref());
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
        self.poll_texture_dependencies();
        self.poll_model_compile_job();
        if self.model_compile_job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        if self
            .pending_add
            .as_ref()
            .is_some_and(|batch| batch.conflict.is_none())
        {
            self.process_add_batch(ConflictAction::Continue);
            ctx.request_repaint();
        }
        let forwarded_messages = self
            .incoming_paths
            .as_ref()
            .map(|receiver| receiver.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let activation_requested = !forwarded_messages.is_empty();
        let forwarded = forwarded_messages.into_iter().flatten().collect::<Vec<_>>();
        if !forwarded.is_empty() {
            self.pending_drop_files.extend(forwarded);
        }
        let (
            hovered_drop_file_count,
            hovered_drop_files,
            hovered_drop_unsupported_count,
            hovered_drop_unsupported_extensions,
        ) = ctx.input(|input| {
            let files = &input.raw.hovered_files;
            let preview_files = files
                .iter()
                .take(5)
                .map(|file| {
                    let path = file.path.as_deref();
                    HoveredDropFile {
                        name: path
                            .as_ref()
                            .and_then(|path| path.file_name())
                            .and_then(|name| name.to_str())
                            .unwrap_or("file")
                            .to_owned(),
                        unsupported: path
                            .filter(|path| !path.is_dir())
                            .is_some_and(|path| unsupported_import_extension(path).is_some()),
                    }
                })
                .collect();
            let unsupported_extensions = files
                .iter()
                .filter_map(|file| file.path.as_deref())
                .filter(|path| !path.is_dir())
                .filter_map(unsupported_import_extension)
                .collect::<BTreeSet<_>>();
            let unsupported_count = files
                .iter()
                .filter_map(|file| file.path.as_deref())
                .filter(|path| !path.is_dir())
                .filter(|path| unsupported_import_extension(path).is_some())
                .count();
            (
                files.len(),
                preview_files,
                unsupported_count,
                unsupported_extensions,
            )
        });
        self.hovered_drop_file_count = hovered_drop_file_count;
        self.hovered_drop_files = hovered_drop_files;
        self.hovered_drop_unsupported_count = hovered_drop_unsupported_count;
        self.hovered_drop_unsupported_extensions = hovered_drop_unsupported_extensions;
        let mut dropped: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        let mut seen = HashSet::with_capacity(dropped.len());
        dropped.retain(|path| seen.insert(path.clone()));
        if !dropped.is_empty() {
            if let Some(position) = native_drag_local_position(ctx)
                && let Some(index) = self
                    .tab_drop_rects
                    .iter()
                    .find_map(|(index, rect)| rect.contains(position).then_some(*index))
                && self.active_tab != Some(index)
            {
                self.switch_tab(index);
            }
            self.pending_drop_files.extend(dropped);
            ctx.request_repaint();
        }
        if activation_requested || !self.pending_drop_files.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
        // The IPC listener is deliberately independent of the UI thread.
        // Poll it at a low rate so a file-manager launch can wake an otherwise
        // idle window without keeping the application busy.
        ctx.request_repaint_after(Duration::from_millis(200));
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let archive_editable = self
            .archive
            .as_ref()
            .is_some_and(|archive| archive.kind.is_editable());
        if ctx.input(|input| input.viewport().close_requested()) && !self.force_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if let Some(job) = self.model_compile_job.as_ref() {
                job.cancel.store(true, Ordering::Relaxed);
                self.quit_after_model_compile = true;
                self.status = "Canceling model compilation before closing…".into();
            } else {
                self.request_quit();
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(self.title()));
        if !self.blocking_dialog_open() {
            self.capture_typeahead(&ctx);
        }
        if !self.blocking_dialog_open() && !self.pending_drop_files.is_empty() {
            let dropped = std::mem::take(&mut self.pending_drop_files);
            let mut archives = Vec::new();
            let mut resources = Vec::new();
            let mut directory_failures = Vec::new();
            for path in dropped {
                if path.is_dir() {
                    match files_in_dropped_directory(&path) {
                        Ok(mut files) => resources.append(&mut files),
                        Err(error) => {
                            directory_failures.push(format!("{}: {error}", path.display()))
                        }
                    }
                } else if is_archive_path(&path) {
                    archives.push(path);
                } else {
                    resources.push(path);
                }
            }
            for path in archives {
                self.open_path(path);
            }
            if !resources.is_empty() {
                self.add_paths(resources);
            }
            if !directory_failures.is_empty() {
                self.error = Some(directory_failures.join("\n"));
            }
        }
        if !self.blocking_dialog_open() && self.hovered_drop_file_count > 0 {
            egui::Area::new(egui::Id::new("file_drop_overlay"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .inner_margin(32.0)
                        .show(ui, |ui| {
                            if self.hovered_drop_unsupported_count > 0 {
                                let accepted = self
                                    .hovered_drop_file_count
                                    .saturating_sub(self.hovered_drop_unsupported_count);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("×")
                                            .size(42.0)
                                            .color(Color32::from_rgb(255, 130, 130)),
                                    );
                                    ui.vertical(|ui| {
                                        ui.heading("Some files cannot be added");
                                        ui.label(format!(
                                            "{} file(s) will be skipped ({})",
                                            self.hovered_drop_unsupported_count,
                                            self.hovered_drop_unsupported_extensions
                                                .iter()
                                                .cloned()
                                                .collect::<Vec<_>>()
                                                .join(", ")
                                        ));
                                    });
                                });
                                ui.add_space(4.0);
                                ui.separator();
                                ui.add_space(3.0);
                                for file in &self.hovered_drop_files {
                                    let (color, action) = if file.unsupported {
                                        (Color32::from_rgb(255, 130, 130), "Skipped")
                                    } else {
                                        (Color32::from_rgb(120, 190, 240), "Added")
                                    };
                                    ui.horizontal(|ui| {
                                        let (rect, _) = ui.allocate_exact_size(
                                            egui::vec2(12.0, ui.spacing().interact_size.y),
                                            egui::Sense::hover(),
                                        );
                                        if file.unsupported {
                                            ui.painter().text(
                                                rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                "×",
                                                egui::TextStyle::Body.resolve(ui.style()),
                                                color,
                                            );
                                        } else {
                                            ui.painter().circle_filled(rect.center(), 3.0, color);
                                        }
                                        ui.label(&file.name);
                                        ui.label(RichText::new(action).color(color));
                                    });
                                }
                                if self.hovered_drop_file_count > self.hovered_drop_files.len() {
                                    ui.label(format!(
                                        "and {} more file(s)",
                                        self.hovered_drop_file_count
                                            - self.hovered_drop_files.len()
                                    ));
                                }
                                ui.add_space(3.0);
                                if accepted > 0 {
                                    ui.label(
                                        RichText::new(format!(
                                            "{accepted} supported file(s) will be added"
                                        ))
                                        .color(Color32::from_rgb(120, 190, 240)),
                                    );
                                }
                            } else {
                                ui.heading("Drop files or folders to add to this archive");
                                ui.label(format!("{} file(s) ready", self.hovered_drop_file_count));
                                for file in &self.hovered_drop_files {
                                    ui.label(&file.name);
                                }
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
                    ui.set_min_width(185.0);
                    if ui.button("New   Ctrl+N").clicked() {
                        self.show_new = true;
                        ui.close();
                    }
                    if ui.button("Open   Ctrl+O").clicked() {
                        self.open_dialog();
                        ui.close();
                    }
                    ui.separator();
                    let enabled = self.archive.is_some();
                    if ui
                        .add_enabled(archive_editable, egui::Button::new("Save   Ctrl+S"))
                        .clicked()
                    {
                        self.save(false);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            archive_editable,
                            egui::Button::new("Save As   Ctrl+Shift+S"),
                        )
                        .clicked()
                    {
                        self.save(true);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(enabled, egui::Button::new("Extract all"))
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
                    let undo_label = self.edit_history.undo_label().map_or_else(
                        || "Undo   Ctrl+Z".to_owned(),
                        |label| format!("Undo {label}   Ctrl+Z"),
                    );
                    if ui
                        .add_enabled(
                            self.edit_history.undo_label().is_some(),
                            egui::Button::new(undo_label),
                        )
                        .clicked()
                    {
                        self.undo_edit();
                        ui.close();
                    }
                    let redo_label = self.edit_history.redo_label().map_or_else(
                        || "Redo   Ctrl+Shift+Z".to_owned(),
                        |label| format!("Redo {label}   Ctrl+Shift+Z"),
                    );
                    if ui
                        .add_enabled(
                            self.edit_history.redo_label().is_some(),
                            egui::Button::new(redo_label),
                        )
                        .clicked()
                    {
                        self.redo_edit();
                        ui.close();
                    }
                    ui.separator();
                    let has_selection = !self.selected.is_empty();
                    if ui
                        .add_enabled(has_selection, egui::Button::new("Copy   Ctrl+C"))
                        .clicked()
                    {
                        request_copy = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            has_selection && archive_editable,
                            egui::Button::new("Cut   Ctrl+X"),
                        )
                        .clicked()
                    {
                        request_cut = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(archive_editable, egui::Button::new("Paste   Ctrl+V"))
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
                        .add_enabled(archive_editable, egui::Button::new("Add files…"))
                        .clicked()
                    {
                        self.add_files();
                        ui.close();
                    }
                    if ui
                        .add_enabled(archive_editable, egui::Button::new("Add directory…"))
                        .clicked()
                    {
                        self.add_directory();
                        ui.close();
                    }
                    if ui
                        .add_enabled(archive_editable, egui::Button::new("Merge archive…"))
                        .clicked()
                    {
                        self.merge();
                        ui.close();
                    }
                    if ui
                        .add_enabled(archive_editable, egui::Button::new("Edit description…"))
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
                ui.menu_button("Tools", |ui| {
                    ui.label("Neverwinter Nights installation");
                    if let Some(path) = self.nwn_installation.as_ref() {
                        let display = path.display().to_string();
                        ui.label(RichText::new(&display).small().weak())
                            .on_hover_text(display);
                    } else {
                        ui.label(RichText::new("Not configured").small().weak());
                    }
                    ui.separator();
                    if ui.button("Choose installation directory…").clicked() {
                        self.choose_nwn_installation();
                        ui.close();
                    }
                    if ui.button("Auto-detect installation").clicked() {
                        self.auto_detect_nwn_installation();
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.nwn_installation.is_some(),
                            egui::Button::new("Clear configured directory"),
                        )
                        .clicked()
                    {
                        self.nwn_installation = None;
                        self.model_preview = None;
                        self.status = "Cleared the configured NWN installation directory".into();
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
        let native_drag_position = (self.hovered_drop_file_count > 0)
            .then(|| native_drag_local_position(&ctx))
            .flatten();
        self.tab_drop_rects.clear();
        egui::Panel::top("document_tabs")
            .frame(
                egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(ui, |ui| {
                // Keep the horizontal scrollbar in its own row so it cannot cover
                // the lower edge of the archive tabs when many tabs are open.
                ui.spacing_mut().scroll.floating = false;
                ui.horizontal_top(|ui| {
                    let open_clicked = ui
                        .add_sized([69.0, 31.0], egui::Button::new("Open"))
                        .on_hover_text("Open archive (Ctrl+O)")
                        .clicked();
                    if open_clicked {
                        self.open_dialog();
                    }
                    let recent_archives = self.recent_archives.clone();
                    let mut requested_recent = None;
                    let mut clear_recent = false;
                    // The standard menu button reserves label space for a disclosure
                    // indicator, which makes this short label look visibly off-center.
                    // Paint the label over an empty native button instead.
                    let recent_response = ui.add_sized([83.0, 31.0], egui::Button::new(""));
                    recent_response
                        .clone()
                        .on_hover_text("Open a recent archive");
                    ui.painter().text(
                        recent_response.rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Recent",
                        egui::TextStyle::Button.resolve(ui.style()),
                        ui.style().interact(&recent_response).text_color(),
                    );
                    let _ = egui::Popup::menu(&recent_response).show(|ui| {
                        ui.set_min_width(260.0);
                        if recent_archives.is_empty() {
                            ui.add_enabled(false, egui::Button::new("No recent archives"));
                            return;
                        }
                        for path in recent_archives {
                            let filename = path
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string());
                            if ui
                                .button(filename)
                                .on_hover_text(path.display().to_string())
                                .clicked()
                            {
                                requested_recent = Some(path);
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("Clear recent files").clicked() {
                            clear_recent = true;
                            ui.close();
                        }
                    });
                    if clear_recent {
                        self.recent_archives.clear();
                    } else if let Some(path) = requested_recent {
                        if path.is_file() {
                            self.open_path(path);
                        } else {
                            self.recent_archives.retain(|recent| recent != &path);
                            self.status =
                                format!("Removed missing recent archive: {}", path.display());
                        }
                    }
                    ui.add_space(4.0);
                    let tabs_width = ui.available_width();
                    egui::ScrollArea::horizontal()
                        .id_salt("open_archive_tabs")
                        .max_width(tabs_width)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for (index, tab_state) in self.tabs.iter().enumerate() {
                                    let active = self.active_tab == Some(index);
                                    let dirty = if active { self.dirty } else { tab_state.dirty };
                                    let label = format!(
                                        "{}{}",
                                        if dirty { "* " } else { "" },
                                        tab_state.label()
                                    );
                                    // Keep this document-tab treatment visually identical to Aurora TLK
                                    // Explorer without changing AHE's general resource-selection colour.
                                    let selection = if ui.visuals().dark_mode {
                                        Color32::from_rgb(45, 67, 82)
                                    } else {
                                        Color32::from_rgb(178, 205, 226)
                                    };
                                    let title_color = if active {
                                        ui.visuals().strong_text_color()
                                    } else {
                                        ui.visuals().text_color()
                                    };
                                    let title_font = egui::TextStyle::Body.resolve(ui.style());
                                    let title_width = ui
                                        .painter()
                                        .layout_no_wrap(
                                            label.clone(),
                                            title_font.clone(),
                                            title_color,
                                        )
                                        .size()
                                        .x;
                                    let tab_size = egui::vec2(
                                        12.0 + title_width
                                            + ui.spacing().item_spacing.x
                                            + 23.0
                                            + 12.0,
                                        31.0,
                                    );
                                    let (tab_rect, _) =
                                        ui.allocate_exact_size(tab_size, egui::Sense::hover());
                                    let tab_id = ui.make_persistent_id(("archive_tab", index));
                                    let close_rect = egui::Rect::from_min_size(
                                        egui::pos2(tab_rect.right() - 35.0, tab_rect.top() + 4.0),
                                        egui::vec2(23.0, 23.0),
                                    );
                                    let title_rect = egui::Rect::from_min_max(
                                        egui::pos2(tab_rect.left() + 12.0, tab_rect.top() + 4.0),
                                        egui::pos2(
                                            close_rect.left() - ui.spacing().item_spacing.x,
                                            tab_rect.bottom() - 4.0,
                                        ),
                                    );
                                    let title = ui.interact(
                                        title_rect,
                                        tab_id.with("title"),
                                        egui::Sense::click(),
                                    );
                                    let close = ui
                                        .interact(
                                            close_rect,
                                            tab_id.with("close"),
                                            egui::Sense::click(),
                                        )
                                        .on_hover_text("Close tab");
                                    if active {
                                        ui.painter().rect_filled(
                                            tab_rect,
                                            4.0,
                                            selection.gamma_multiply(0.82),
                                        );
                                    }
                                    if close.hovered() {
                                        ui.painter().rect_filled(
                                            close_rect,
                                            4.0,
                                            selection.gamma_multiply(0.72),
                                        );
                                    }
                                    ui.painter().text(
                                        egui::pos2(title_rect.left(), title_rect.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        &label,
                                        title_font,
                                        title_color,
                                    );
                                    ui.painter().text(
                                        close_rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        "×",
                                        egui::FontId::proportional(14.0),
                                        ui.visuals().strong_text_color(),
                                    );
                                    self.tab_drop_rects.push((index, tab_rect));
                                    if active {
                                        ui.painter().rect_stroke(
                                            tab_rect,
                                            4.0,
                                            egui::Stroke::new(1.0, selection.gamma_multiply(0.95)),
                                            egui::StrokeKind::Inside,
                                        );
                                        ui.painter().line_segment(
                                            [
                                                egui::pos2(
                                                    tab_rect.left() + 4.0,
                                                    tab_rect.bottom(),
                                                ),
                                                egui::pos2(
                                                    tab_rect.right() - 4.0,
                                                    tab_rect.bottom(),
                                                ),
                                            ],
                                            egui::Stroke::new(2.0, selection),
                                        );
                                    } else if title.hovered() || close.hovered() {
                                        ui.painter().rect_stroke(
                                            tab_rect,
                                            4.0,
                                            egui::Stroke::new(1.0, selection.gamma_multiply(0.75)),
                                            egui::StrokeKind::Inside,
                                        );
                                    }
                                    title.context_menu(|ui| {
                                        if ui.button("Close tab").clicked() {
                                            requested_close = Some(index);
                                            ui.close();
                                        }
                                    });
                                    if title.clicked() {
                                        requested_switch = Some(index);
                                    }
                                    if native_drag_position
                                        .is_some_and(|position| tab_rect.contains(position))
                                        && !active
                                    {
                                        requested_switch = Some(index);
                                    }
                                    if close.clicked() {
                                        requested_close = Some(index);
                                    }
                                    if middle_click
                                        .is_some_and(|position| tab_rect.contains(position))
                                    {
                                        requested_close = Some(index);
                                    }
                                    ui.add_space(2.0);
                                }
                                if self.tabs.is_empty() {
                                    let empty_font = egui::TextStyle::Body.resolve(ui.style());
                                    let empty_color = ui.visuals().weak_text_color();
                                    let empty_width = ui
                                        .painter()
                                        .layout_no_wrap(
                                            "No archives open".into(),
                                            empty_font.clone(),
                                            empty_color,
                                        )
                                        .size()
                                        .x;
                                    let (empty_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(empty_width, 31.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().text(
                                        egui::pos2(empty_rect.left(), empty_rect.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        "No archives open",
                                        empty_font,
                                        empty_color,
                                    );
                                }
                            });
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
            let ctrl_shift = egui::Modifiers {
                ctrl: true,
                shift: true,
                ..Default::default()
            };
            if ctx.input_mut(|i| i.consume_key(ctrl_shift, egui::Key::Z)) {
                self.redo_edit();
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Z)) {
                self.undo_edit();
            }
            if ctx.input_mut(|i| i.consume_key(ctrl_shift, egui::Key::S)) {
                self.save(true);
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::S)) {
                self.save(false);
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::N)) {
                self.show_new = true;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A)) {
                request_select_all = true;
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::E)) {
                self.export_selected();
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
        let resource_view = self.resource_view();
        let archive_info = resource_view.as_ref().map(|(summary, _)| {
            (
                summary.name.clone(),
                summary.count,
                summary.bytes,
                summary.version,
                summary.kind.clone(),
            )
        });
        let categories = resource_view
            .as_ref()
            .map(|(summary, _)| &summary.categories);
        let compiled_models = resource_view
            .as_ref()
            .map_or(0, |(summary, _)| summary.compiled_models);
        let uncompiled_models = resource_view
            .as_ref()
            .map_or(0, |(summary, _)| summary.uncompiled_models);
        let new_count = resource_view
            .as_ref()
            .map_or(0, |(summary, _)| summary.new_count);
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
                        self.set_category(None);
                    }
                    ui.indent("categories", |ui| {
                        let active = self.category.as_deref() == Some("New");
                        if resource_tree_row(ui, active, "New", new_count).clicked() {
                            self.set_category(Some("New".to_owned()));
                            self.selected.clear();
                            self.selection_anchor = None;
                            self.selection_cursor = None;
                        }
                        let mut category_rows =
                            categories.into_iter().flatten().collect::<Vec<_>>();
                        category_rows.sort_by_key(|(category, _)| {
                            (category.as_str() == "Other", category.as_str())
                        });
                        for (category, amount) in category_rows {
                            if category == "Models" {
                                for (label, count) in [
                                    ("Models All", *amount),
                                    ("Models Compiled", compiled_models),
                                    ("Models Uncompiled", uncompiled_models),
                                ] {
                                    let active = self.category.as_deref() == Some(label)
                                        || (label == "Models All"
                                            && self.category.as_deref() == Some("Models"));
                                    if resource_tree_row(ui, active, label, count).clicked() {
                                        self.set_category(Some(label.to_owned()));
                                        self.selected.clear();
                                        self.selection_anchor = None;
                                        self.selection_cursor = None;
                                    }
                                }
                                continue;
                            }
                            let active = self.category.as_deref() == Some(category);
                            if resource_tree_row(ui, active, category, *amount).clicked() {
                                self.set_category(Some(category.clone()));
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
                        let extension = entry.extension();
                        let tileset_name = if extension == "set" {
                            entry
                                .read_prefix(256 * 1024)
                                .ok()
                                .and_then(|bytes| tileset_unlocalized_name(&bytes))
                        } else {
                            None
                        };
                        let title = tileset_name.map_or_else(
                            || entry.filename(),
                            |name| format!("{} — {name}", entry.filename()),
                        );
                        ui.heading(title);
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
                            if is_previewable_image(&extension) {
                                self.show_image_preview(ui, entry);
                            } else if extension == "mdl" {
                                if let Some(action) = self.show_model_preview(ui, entry) {
                                    self.export_selected_models(action);
                                }
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
                            if ui
                                .add_enabled(
                                    archive_editable,
                                    egui::Button::new("Remove from archive"),
                                )
                                .clicked()
                            {
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
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.filter)
                                .hint_text("Search resources…")
                                .desired_width(260.0),
                        );
                    });
                }
            });
            ui.separator();
            if let Some(a) = self.archive.as_ref() {
                let reset_resource_scroll =
                    std::mem::take(&mut self.resource_scroll_reset_pending);
                let visible_indices = resource_view
                    .as_ref()
                    .map(|(_, indices)| Arc::clone(indices))
                    .unwrap_or_else(|| Arc::from([]));
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
                let mut request_model_export = None;
                let mut request_drag = None;
                let type_column_width = 100.0;
                let size_column_width = 100.0;
                let name_column_width = (ui.available_width()
                    - type_column_width
                    - size_column_width
                    - ui.spacing().item_spacing.x * 2.0)
                    .max(100.0);
                egui::Grid::new("entries_header")
                    .min_col_width(100.0)
                    .show(ui, |ui| {
                        if sort_button(
                            ui,
                            "Name",
                            name_column_width,
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
                            type_column_width,
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
                            size_column_width,
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
                    });
                let row_height = ui.spacing().interact_size.y;
                let resource_scroll_id = ui.make_persistent_id(("resource_entries", self.active_tab));
                let requested_scroll_offset = if reset_resource_scroll {
                    Some(0.0)
                } else {
                    jump_target
                        .or(keyboard_target)
                        .and_then(|target| visible_indices.iter().position(|item| *item == target))
                        .and_then(|row| {
                            let current_offset =
                                egui::scroll_area::State::load(&ctx, resource_scroll_id)
                                    .unwrap_or_default()
                                    .offset
                                    .y;
                            let viewport_height = ui.available_height().max(row_height);
                            requested_virtualized_scroll_offset(
                                row,
                                row_height,
                                ui.spacing().item_spacing.y,
                                current_offset,
                                viewport_height,
                            )
                        })
                };
                let mut resource_scroll_area = egui::ScrollArea::vertical()
                    .id_salt(("resource_entries", self.active_tab))
                    .auto_shrink(false);
                if let Some(offset) = requested_scroll_offset {
                    resource_scroll_area = resource_scroll_area.vertical_scroll_offset(offset);
                }
                let resource_scroll = resource_scroll_area.show_rows(
                    ui,
                    row_height,
                    visible_indices.len(),
                    |ui, row_range| {
                        egui::Grid::new("entries")
                            .striped(true)
                            .min_col_width(100.0)
                            .show(ui, |ui| {
                                for row in row_range {
                                    let index = visible_indices[row];
                                    let entry = &a.entries[index];
                                    let ext = entry.extension();
                                    let selected = self.selected.contains(&index);
                                    let response = ui.add_sized(
                                        [name_column_width, row_height],
                                        egui::Button::selectable(selected, entry.filename())
                                            .sense(egui::Sense::click_and_drag()),
                                    );
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
                                        if ui
                                            .button(format!("Export Selected ({count})   Ctrl+E"))
                                            .clicked()
                                        {
                                            request_export = true;
                                            ui.close();
                                        }
                                        let model_export_action =
                                            selected_model_export_action(a, &self.selected);
                                        if let Some(model_export_action) = model_export_action {
                                            let model_export_label = match model_export_action {
                                                ModelExportAction::Compile => {
                                                    format!("Compile & Export MDL ({count})")
                                                }
                                                ModelExportAction::Decompile => {
                                                    format!("Decompile & Export MDL ({count})")
                                                }
                                            };
                                            if ui.button(model_export_label).clicked() {
                                                request_model_export = Some(model_export_action);
                                                ui.close();
                                            }
                                        }
                                        if ui
                                            .add_enabled(
                                                archive_editable,
                                                egui::Button::new(format!(
                                                    "Delete Selected ({count})   Del"
                                                )),
                                            )
                                            .clicked()
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
                    },
                );
                let (middle_pressed, primary_pressed, escape_pressed, pointer_position, delta_time) =
                    ctx.input(|input| {
                        (
                            input.pointer.button_pressed(egui::PointerButton::Middle),
                            input.pointer.button_pressed(egui::PointerButton::Primary),
                            input.key_pressed(egui::Key::Escape),
                            input.pointer.hover_pos(),
                            input.stable_dt.min(0.05),
                        )
                    });
                if self.blocking_dialog_open() {
                    // A modal must never leave browser-style autoscroll running
                    // behind it, even if the middle mouse button remains held.
                    self.resource_middle_scroll_anchor = None;
                } else {
                    if self.resource_middle_scroll_anchor.is_some()
                        && (middle_pressed || primary_pressed || escape_pressed)
                    {
                        self.resource_middle_scroll_anchor = None;
                    } else if middle_pressed
                        && pointer_position
                            .is_some_and(|position| resource_scroll.inner_rect.contains(position))
                    {
                        self.resource_middle_scroll_anchor = pointer_position;
                    }
                    if let (Some(anchor), Some(position)) =
                        (self.resource_middle_scroll_anchor, pointer_position)
                    {
                        let distance = position.y - anchor.y;
                        // Match browser autoscroll: retain fine control close to the anchor,
                        // then accelerate sharply enough to traverse very large archives.
                        let speed = distance.signum()
                            * ((distance.abs() - 6.0).max(0.0).powf(1.55) * 1.3).min(6_000.0);
                        let mut state = resource_scroll.state;
                        let maximum_offset = (resource_scroll.content_size.y
                            - resource_scroll.inner_rect.height())
                        .max(0.0);
                        state.offset.y =
                            (state.offset.y + speed * delta_time).clamp(0.0, maximum_offset);
                        state.store(&ctx, resource_scroll.id);
                        let painter = ui.painter();
                        painter.circle_stroke(
                            anchor,
                            7.0,
                            egui::Stroke::new(1.0, ui.visuals().text_color()),
                        );
                        painter.line_segment(
                            [anchor - egui::vec2(10.0, 0.0), anchor + egui::vec2(10.0, 0.0)],
                            egui::Stroke::new(1.0, ui.visuals().text_color()),
                        );
                        painter.line_segment(
                            [anchor - egui::vec2(0.0, 10.0), anchor + egui::vec2(0.0, 10.0)],
                            egui::Stroke::new(1.0, ui.visuals().text_color()),
                        );
                        ctx.set_cursor_icon(egui::CursorIcon::AllScroll);
                        ctx.request_repaint();
                    }
                }
                if request_drag.is_some() {
                    self.drag_selected(frame);
                } else if let Some(action) = request_model_export {
                    self.export_selected_models(action);
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
                    if ui
                        .add_enabled(archive_editable, egui::Button::new("Add…"))
                        .clicked()
                    {
                        self.add_files();
                    }
                    if ui
                        .add_enabled(
                            archive_editable && !self.selected.is_empty(),
                            egui::Button::new("Delete"),
                        )
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
                            ui.label(
                                "You can also drop a .hak, .erf, .mod, .sav, or .bif file here.",
                            );
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
                let before = self.archive.as_ref().map(Archive::description);
                if let Some(before) = before
                    && before != self.description_buffer
                {
                    let selected = self.selected_resource_keys();
                    let category = self.category.clone();
                    let transaction = EditTransaction {
                        label: "archive description change".into(),
                        edit: ArchiveEdit::Description {
                            before,
                            after: self.description_buffer.clone(),
                        },
                        selected_before: selected.clone(),
                        selected_after: selected,
                        category_before: category.clone(),
                        category_after: category,
                        dirty_before: self.dirty,
                        dirty_after: true,
                        estimated_bytes: 0,
                    };
                    if let Some(archive) = self.archive.as_mut() {
                        archive.set_description(self.description_buffer.clone());
                    }
                    self.dirty = true;
                    self.status = "Updated archive description".into();
                    if !self.record_edit(transaction) {
                        self.status
                            .push_str(" — operation exceeded the bounded undo-history limit");
                    }
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
                                "Aurora Hak Explorer (AHE) {DISPLAY_VERSION}"
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
        if let Some(job) = self.model_compile_job.as_ref() {
            let mut cancel = false;
            let theme = self.active_theme(&ctx);
            let elapsed = job.started_at.elapsed();
            let remaining = estimated_time_remaining(elapsed, job.completed, job.total);
            let fraction = if job.total == 0 {
                0.0
            } else {
                job.completed as f32 / job.total as f32
            };
            egui::Modal::new(egui::Id::new("model_compilation_modal"))
                .frame(egui::Frame::popup(&ctx.style_of(theme)).inner_margin(24.0))
                .show(&ctx, |ui| {
                    ui.set_min_width(520.0);
                    ui.spacing_mut().item_spacing.y = 12.0;
                    ui.label(RichText::new("Compiling models").size(22.0).strong());
                    ui.separator();
                    ui.label(RichText::new(&job.phase).size(17.0));
                    if !job.current.is_empty() {
                        ui.label(RichText::new(&job.current).monospace());
                    }
                    ui.add(
                        egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                            .show_percentage()
                            .text(format!("{} of {}", job.completed, job.total)),
                    );
                    egui::Grid::new("model_compilation_timing")
                        .num_columns(2)
                        .spacing([28.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Time elapsed:");
                            ui.label(format_compilation_duration(elapsed));
                            ui.end_row();
                            ui.label("Estimated time remaining:");
                            ui.label(remaining.map_or_else(
                                || "Calculating…".to_owned(),
                                |duration| {
                                    format!("About {}", format_compilation_duration(duration))
                                },
                            ));
                            ui.end_row();
                        });
                    ui.add_space(5.0);
                    ui.horizontal_centered(|ui| {
                        if ui
                            .add_sized(
                                [140.0, 38.0],
                                egui::Button::new(RichText::new("Cancel").size(16.0)),
                            )
                            .clicked()
                        {
                            cancel = true;
                        }
                    });
                });
            if cancel {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        if let Some(message) = self.warning.clone() {
            let mut close = false;
            let theme = self.active_theme(&ctx);
            let modal = egui::Modal::new(egui::Id::new("archive_compatibility_warning_modal"))
                .frame(egui::Frame::popup(&ctx.style_of(theme)).inner_margin(24.0))
                .show(&ctx, |ui| {
                    ui.set_min_width(500.0);
                    ui.set_max_width(650.0);
                    ui.spacing_mut().item_spacing.y = 14.0;
                    ui.label(
                        RichText::new("Compatibility warning")
                            .size(22.0)
                            .strong()
                            .color(Color32::from_rgb(255, 205, 110)),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(message)
                            .size(17.0)
                            .color(Color32::from_rgb(255, 220, 150)),
                    );
                    ui.add_space(6.0);
                    if ui
                        .add_sized(
                            [120.0, 38.0],
                            egui::Button::new(RichText::new("Continue").size(16.0)),
                        )
                        .clicked()
                    {
                        close = true;
                    }
                });
            if modal.should_close() || close {
                self.warning = None;
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

fn selected_model_export_action(
    archive: &Archive,
    selected: &BTreeSet<usize>,
) -> Option<ModelExportAction> {
    let mut action = None;
    for index in selected {
        let entry = archive.entries.get(*index)?;
        if entry.extension() != "mdl" {
            return None;
        }
        let entry_action = model_export_action(entry).ok()?;
        if action.is_some_and(|current| current != entry_action) {
            return None;
        }
        action = Some(entry_action);
    }
    action
}

fn model_export_action(entry: &archive::Entry) -> Result<ModelExportAction, String> {
    let prefix = entry.read_prefix(4).map_err(|error| error.to_string())?;
    if prefix.len() < 4 {
        return Err("model is too small to identify".into());
    }
    if prefix == [0, 0, 0, 0] {
        Ok(ModelExportAction::Decompile)
    } else {
        Ok(ModelExportAction::Compile)
    }
}

#[derive(Clone)]
struct ModelDependency {
    bytes: Vec<u8>,
    origin: String,
}

struct ModelDependencyResolver<'a> {
    archives: Vec<(&'a Archive, String)>,
    sibling_archives: Vec<PathBuf>,
    loose_directories: Vec<PathBuf>,
    game_resources: game_resources::GameResourceIndex,
    cache: BTreeMap<String, Result<Option<ModelDependency>, String>>,
}

impl<'a> ModelDependencyResolver<'a> {
    fn new(active: &'a Archive, tabs: &'a [TabState], nwn_installation: Option<&Path>) -> Self {
        let mut archives = vec![(active, "the current archive".to_owned())];
        for tab in tabs {
            if tab.archive.view_key().0 == active.view_key().0 {
                continue;
            }
            archives.push((
                &tab.archive,
                tab.archive.path.as_ref().map_or_else(
                    || "an open unsaved archive".to_owned(),
                    |path| path.display().to_string(),
                ),
            ));
        }

        let open_paths: BTreeSet<PathBuf> = archives
            .iter()
            .filter_map(|(archive, _)| archive.path.as_ref())
            .filter_map(|path| fs::canonicalize(path).ok())
            .collect();
        let mut sibling_archives = Vec::new();
        for (archive, _) in &archives {
            let Some(directory) = archive.path.as_ref().and_then(|path| path.parent()) else {
                continue;
            };
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
                if !path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("hak"))
                {
                    continue;
                }
                let canonical = fs::canonicalize(&path).unwrap_or(path);
                if !open_paths.contains(&canonical) {
                    sibling_archives.push(canonical);
                }
            }
        }
        sibling_archives.sort();
        sibling_archives.dedup();

        let mut document_roots = Vec::new();
        for (archive, _) in &archives {
            if let Some(directory) = archive.path.as_ref().and_then(|path| path.parent())
                && directory
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("hak"))
                && let Some(root) = directory.parent()
            {
                document_roots.push(root.to_path_buf());
            }
        }
        if let Some(home) = home_directory() {
            document_roots.push(home.join("Documents/Neverwinter Nights"));
        }
        document_roots.sort();
        document_roots.dedup();

        let mut loose_directories = std::env::var_os("AHE_MODEL_PATH")
            .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .unwrap_or_default();
        for root in &document_roots {
            loose_directories.push(root.join("development"));
            loose_directories.push(root.join("override"));
        }
        loose_directories.retain(|path| path.is_dir());
        loose_directories.sort();
        loose_directories.dedup();

        let install_roots = discover_nwn_installations(nwn_installation);
        let game_resources = game_resources::GameResourceIndex::build(&install_roots, 0x07d2);
        Self {
            archives,
            sibling_archives,
            loose_directories,
            game_resources,
            cache: BTreeMap::new(),
        }
    }

    fn resolve(&mut self, name: &str) -> Result<Option<ModelDependency>, String> {
        let key = name.to_ascii_lowercase();
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }
        let result = self.resolve_uncached(name);
        self.cache.insert(key, result.clone());
        result
    }

    fn resolve_uncached(&self, name: &str) -> Result<Option<ModelDependency>, String> {
        for (archive, origin) in &self.archives {
            if let Some(entry) = find_model_entry(archive, name) {
                return read_model_dependency(entry, origin.clone()).map(Some);
            }
        }
        for path in &self.sibling_archives {
            let Ok(archive) = Archive::open(path) else {
                continue;
            };
            if let Some(entry) = find_model_entry(&archive, name) {
                return read_model_dependency(entry, path.display().to_string()).map(Some);
            }
        }
        let filename = format!("{name}.mdl");
        for directory in &self.loose_directories {
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            let path = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.is_file()
                        && path.file_name().is_some_and(|candidate| {
                            candidate.to_string_lossy().eq_ignore_ascii_case(&filename)
                        })
                });
            if let Some(path) = path {
                let bytes = fs::read(&path).map_err(|error| error.to_string())?;
                return checked_model_dependency(bytes, path.display().to_string()).map(Some);
            }
        }
        if let Some((bytes, path)) = self
            .game_resources
            .load(name, 0x07d2)
            .map_err(|error| error.to_string())?
        {
            return checked_model_dependency(bytes, path.display().to_string()).map(Some);
        }
        Ok(None)
    }
}

fn find_model_entry<'a>(archive: &'a Archive, name: &str) -> Option<&'a archive::Entry> {
    archive
        .entries
        .iter()
        .find(|entry| entry.type_id == 0x07d2 && entry.name.eq_ignore_ascii_case(name))
}

fn read_model_dependency(
    entry: &archive::Entry,
    origin: String,
) -> Result<ModelDependency, String> {
    let size = entry.size().map_err(|error| error.to_string())?;
    if size > MAX_MODEL_RENDER_BYTES {
        return Err(format!(
            "supermodel {} exceeds the {} compilation limit",
            entry.filename(),
            human_size(MAX_MODEL_RENDER_BYTES)
        ));
    }
    let bytes = entry.read_prefix(size).map_err(|error| error.to_string())?;
    checked_model_dependency(bytes, origin)
}

fn checked_model_dependency(bytes: Vec<u8>, origin: String) -> Result<ModelDependency, String> {
    if bytes.len() as u64 > MAX_MODEL_RENDER_BYTES {
        return Err(format!(
            "supermodel from {origin} exceeds the {} compilation limit",
            human_size(MAX_MODEL_RENDER_BYTES)
        ));
    }
    Ok(ModelDependency { bytes, origin })
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn save_nwn_installation(storage: &mut dyn eframe::Storage, path: Option<&Path>) {
    if let Some(path) = path {
        eframe::set_value(
            storage,
            "nwn_installation",
            &path.to_string_lossy().into_owned(),
        );
    } else {
        storage.remove_string("nwn_installation");
    }
}

fn discover_nwn_installations(preferred: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = preferred
        .into_iter()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    candidates.extend(
        std::env::var_os("AHE_NWN_INSTALL")
            .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    let mut steam_roots = Vec::new();
    if let Some(home) = home_directory() {
        steam_roots.extend([
            home.join(".steam/steam"),
            home.join(".local/share/Steam"),
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        ]);
        candidates.extend([
            home.join(".beamdog/00829"),
            home.join("GOG Games/Neverwinter Nights Enhanced Edition"),
        ]);
    }
    for variable in ["PROGRAMFILES(X86)", "PROGRAMFILES"] {
        if let Some(directory) = std::env::var_os(variable) {
            let directory = PathBuf::from(directory);
            steam_roots.push(directory.join("Steam"));
            candidates.extend([
                directory.join("GOG Galaxy/Games/Neverwinter Nights Enhanced Edition"),
                directory.join("Beamdog/Neverwinter Nights"),
            ]);
        }
    }
    let mut steam_libraries = steam_roots.clone();
    for root in steam_roots {
        for vdf in [
            root.join("steamapps/libraryfolders.vdf"),
            root.join("config/libraryfolders.vdf"),
        ] {
            steam_libraries.extend(steam_library_paths(&vdf));
        }
    }
    for library in steam_libraries {
        let steamapps = if library
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("steamapps"))
        {
            library
        } else {
            library.join("steamapps")
        };
        let manifest = steamapps.join("appmanifest_704450.acf");
        let install_name =
            steam_install_directory(&manifest).unwrap_or_else(|| "Neverwinter Nights".to_owned());
        candidates.push(steamapps.join("common").join(install_name));
    }

    let mut roots = Vec::new();
    for candidate in candidates {
        if let Some(root) = normalize_nwn_installation(&candidate)
            && !roots.contains(&root)
        {
            roots.push(root);
        }
    }
    roots
}

fn normalize_nwn_installation(path: &Path) -> Option<PathBuf> {
    if !path.is_dir() {
        return None;
    }
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if directory_has_key_file(&path) {
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("data"))
            && let Some(parent) = path.parent()
        {
            return Some(parent.to_path_buf());
        }
        return Some(path);
    }
    directory_has_key_file(&path.join("data")).then_some(path)
}

fn directory_has_key_file(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry.path().is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("key"))
        })
    })
}

fn steam_library_paths(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| quoted_vdf_value(line, "path"))
        .map(|value| PathBuf::from(value.replace("\\\\", "\\")))
        .collect()
}

fn steam_install_directory(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|line| quoted_vdf_value(line, "installdir"))
}

fn quoted_vdf_value(line: &str, wanted_key: &str) -> Option<String> {
    let mut quoted = line.split('"');
    let _before = quoted.next()?;
    let key = quoted.next()?;
    let _between = quoted.next()?;
    let value = quoted.next()?;
    key.eq_ignore_ascii_case(wanted_key)
        .then(|| value.to_owned())
}

fn stage_model_dependencies(
    resolver: &mut ModelDependencyResolver<'_>,
    source: &[u8],
    workspace: &Path,
    visited: &mut BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let mut origins = Vec::new();
    stage_model_dependencies_inner(resolver, source, workspace, visited, &mut origins)?;
    Ok(origins)
}

fn stage_model_dependencies_inner(
    resolver: &mut ModelDependencyResolver<'_>,
    source: &[u8],
    workspace: &Path,
    visited: &mut BTreeSet<String>,
    origins: &mut Vec<String>,
) -> Result<(), String> {
    let text = String::from_utf8_lossy(source);
    let supermodel = text.lines().find_map(|line| {
        let mut tokens = line.split_whitespace();
        let directive = tokens.next()?;
        if !directive.eq_ignore_ascii_case("setsupermodel") {
            return None;
        }
        let _model_name = tokens.next()?;
        tokens.next().map(str::to_string)
    });
    let Some(supermodel) = supermodel else {
        return Ok(());
    };
    if supermodel.eq_ignore_ascii_case("null") || supermodel.is_empty() {
        return Ok(());
    }
    let key = supermodel.to_ascii_lowercase();
    if !visited.insert(key) {
        return Ok(());
    }
    let Some(dependency) = resolver.resolve(&supermodel)? else {
        // The compiled MDL stores the supermodel name but does not inline its
        // payload. Match the Rust compiler and legacy game behavior by
        // allowing dangling custom-content references to compile.
        return Ok(());
    };
    fs::write(
        workspace.join(format!("{supermodel}.mdl")),
        &dependency.bytes,
    )
    .map_err(|error| error.to_string())?;
    origins.push(format!("{supermodel}.mdl from {}", dependency.origin));
    if dependency.bytes.get(..4) != Some(&[0, 0, 0, 0]) {
        stage_model_dependencies_inner(resolver, &dependency.bytes, workspace, visited, origins)?;
    }
    Ok(())
}

struct ModelCompiler {
    path: PathBuf,
    // The app retains an extracted Windows helper from first use until exit.
    // Linux packages use a permanent AppImage helper.
    _temporary_directory: Option<tempfile::TempDir>,
}

fn model_compiler() -> Result<ModelCompiler, String> {
    if let Some(path) = std::env::var_os("AHE_MODEL_COMPILER").map(PathBuf::from) {
        if path.is_file() {
            return Ok(ModelCompiler {
                path,
                _temporary_directory: None,
            });
        }
        return Err(format!(
            "AHE_MODEL_COMPILER points to a missing file: {}",
            path.display()
        ));
    }
    bundled_model_compiler()
}

#[cfg(target_os = "windows")]
fn bundled_model_compiler() -> Result<ModelCompiler, String> {
    const COMPILER: &[u8] = include_bytes!("../tools/windows/nwnmdlcomp.exe");
    let directory = tempfile::Builder::new()
        .prefix("ahe-model-compiler-")
        .tempdir()
        .map_err(|error| format!("could not prepare the embedded model compiler: {error}"))?;
    drag_cleanup::register(directory.path(), Duration::ZERO);
    let path = directory.path().join("nwnmdlcomp.exe");
    fs::write(&path, COMPILER)
        .map_err(|error| format!("could not extract the embedded model compiler: {error}"))?;
    Ok(ModelCompiler {
        path,
        _temporary_directory: Some(directory),
    })
}

#[cfg(not(target_os = "windows"))]
fn bundled_model_compiler() -> Result<ModelCompiler, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the AHE executable: {error}"))?;
    let executable_directory = executable
        .parent()
        .ok_or_else(|| "could not locate the AHE executable directory".to_string())?;
    let helper_name = "nwnmdlcomp";
    let candidates = [
        executable_directory.join(helper_name),
        executable_directory
            .parent()
            .unwrap_or(executable_directory)
            .join("libexec")
            .join("aurora-hak-explorer")
            .join(helper_name),
        Path::new("tools")
            .join(std::env::consts::OS)
            .join(helper_name),
    ];
    let path = candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            "the bundled NWN model compiler could not be found; reinstall Aurora Hak Explorer"
                .to_string()
        })?;
    let path = fs::canonicalize(&path).unwrap_or(path);
    Ok(ModelCompiler {
        path,
        _temporary_directory: None,
    })
}

struct ModelCompileRequest {
    archive: Archive,
    tabs: Vec<TabState>,
    nwn_installation: Option<PathBuf>,
    model_indices: Vec<usize>,
    compiler: PathBuf,
    single_path: Option<PathBuf>,
    directory: Option<PathBuf>,
}

struct PreparedModel {
    filename: String,
    input: PathBuf,
    destination: PathBuf,
}

fn compile_models_worker(
    request: ModelCompileRequest,
    cancel: Arc<AtomicBool>,
    sender: mpsc::Sender<ModelCompileEvent>,
) {
    const BATCH_SIZE: usize = 128;
    let total = request.model_indices.len();
    let report_path = request.directory.as_ref().map(|directory| {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        directory.join(format!("AHE-model-compilation-failures-{timestamp}.txt"))
    });
    let mut outcome = ModelCompileOutcome {
        exported: 0,
        skipped: 0,
        canceled: false,
        single_path: request.single_path.clone(),
        directory: request.directory.clone(),
        report_path: report_path.clone(),
        fatal_error: None,
    };
    let workspace = match tempfile::Builder::new()
        .prefix("ahe-model-compile-")
        .tempdir()
    {
        Ok(workspace) => workspace,
        Err(error) => {
            outcome.fatal_error = Some(format!("could not prepare model workspace: {error}"));
            let _ = sender.send(ModelCompileEvent::Finished(outcome));
            return;
        }
    };
    drag_cleanup::register(workspace.path(), Duration::ZERO);
    let mut resolver = ModelDependencyResolver::new(
        &request.archive,
        &request.tabs,
        request.nwn_installation.as_deref(),
    );
    let mut staged_dependencies = BTreeSet::new();
    let mut failure_report = None;
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut visited = 0usize;

    for index in request.model_indices {
        if cancel.load(Ordering::Relaxed) {
            outcome.canceled = true;
            break;
        }
        let Some(entry) = request.archive.entries.get(index) else {
            continue;
        };
        let display_name = entry.filename();
        let prepared = (|| -> Result<PreparedModel, String> {
            let filename = entry.safe_filename().map_err(|error| error.to_string())?;
            let size = entry.size().map_err(|error| error.to_string())?;
            if size > MAX_MODEL_RENDER_BYTES {
                return Err(format!(
                    "exceeds the {} compilation limit",
                    human_size(MAX_MODEL_RENDER_BYTES)
                ));
            }
            let source = entry.read_prefix(size).map_err(|error| error.to_string())?;
            stage_model_dependencies(
                &mut resolver,
                &source,
                workspace.path(),
                &mut staged_dependencies,
            )?;
            // The compiler accepts glob patterns as inputs. Stage under an
            // opaque neutral filename so archive resource names (which can
            // legitimately contain glob metacharacters) are always literal.
            let input = workspace.path().join(format!("model-{index}.mdl.ascii"));
            fs::write(&input, &source).map_err(|error| error.to_string())?;
            let destination = request.single_path.clone().unwrap_or_else(|| {
                request
                    .directory
                    .as_ref()
                    .expect("bulk compilation has a destination")
                    .join(&filename)
            });
            Ok(PreparedModel {
                filename,
                input,
                destination,
            })
        })();
        match prepared {
            Ok(prepared) => batch.push(prepared),
            Err(error) => {
                let message = format!("{display_name}: {error}");
                if let Err(error) = record_model_compilation_failure(
                    &mut outcome,
                    &mut failure_report,
                    report_path.as_deref(),
                    &message,
                ) {
                    outcome.fatal_error = Some(error);
                    break;
                }
            }
        }
        visited += 1;
        if visited.is_multiple_of(16) || visited == total {
            let _ = sender.send(ModelCompileEvent::Progress {
                completed: outcome.exported + outcome.skipped,
                phase: "Preparing models".into(),
                current: display_name,
            });
        }
        if (batch.len() == BATCH_SIZE || visited == total)
            && let Err(error) = compile_prepared_batch(
                &request.compiler,
                workspace.path(),
                &mut batch,
                &staged_dependencies,
                &cancel,
                &sender,
                &mut outcome,
                &mut failure_report,
                report_path.as_deref(),
            )
        {
            if error == "canceled" {
                outcome.canceled = true;
            } else {
                outcome.fatal_error = Some(error);
            }
            break;
        }
    }
    if let Some(report) = failure_report.as_mut()
        && let Err(error) = std::io::Write::flush(report)
    {
        outcome.fatal_error = Some(format!("could not finish the failure report: {error}"));
    }
    let _ = sender.send(ModelCompileEvent::Finished(outcome));
}

#[allow(clippy::too_many_arguments)]
fn compile_prepared_batch(
    compiler: &Path,
    workspace: &Path,
    batch: &mut Vec<PreparedModel>,
    staged_dependencies: &BTreeSet<String>,
    cancel: &AtomicBool,
    sender: &mpsc::Sender<ModelCompileEvent>,
    outcome: &mut ModelCompileOutcome,
    failure_report: &mut Option<std::io::BufWriter<fs::File>>,
    report_path: Option<&Path>,
) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }
    let _ = sender.send(ModelCompileEvent::Progress {
        completed: outcome.exported + outcome.skipped,
        phase: format!("Compiling batch of {} models", batch.len()),
        current: batch
            .first()
            .map_or_else(String::new, |model| model.filename.clone()),
    });
    let inputs = batch
        .iter()
        .map(|model| model.input.clone())
        .collect::<Vec<_>>();
    let output = run_model_compiler_batch(compiler, workspace, &inputs, cancel)?;
    let diagnostics = compiler_diagnostics(&output.stdout, &output.stderr);
    let mut diagnostics_reported = false;

    for model in batch.drain(..) {
        let output_path = model.input.with_extension("");
        let result = (|| -> Result<(), String> {
            let compiled = fs::read(&output_path).map_err(|error| {
                missing_compiler_output_error(&error, &diagnostics, &mut diagnostics_reported)
            })?;
            if !valid_compiled_model(&compiled) {
                return Err("compiler output was not a valid binary MDL".into());
            }
            // The preview parser is intentionally conservative and does not
            // understand every valid compiled skin/controller layout. The
            // compiler has already performed a structural binary validation;
            // do not reject that valid output merely because the optional
            // preview cannot reconstruct its faces.
            fs::copy(&output_path, &model.destination).map_err(|error| error.to_string())?;
            Ok(())
        })();
        let _ = fs::remove_file(&model.input);
        let model_name = model
            .filename
            .strip_suffix(".mdl")
            .unwrap_or(&model.filename)
            .to_ascii_lowercase();
        if !staged_dependencies.contains(&model_name) {
            let _ = fs::remove_file(&output_path);
        }
        match result {
            Ok(()) => outcome.exported += 1,
            Err(error) => {
                let message = format!("{}: {error}", model.filename);
                record_model_compilation_failure(outcome, failure_report, report_path, &message)?;
            }
        }
    }
    let _ = sender.send(ModelCompileEvent::Progress {
        completed: outcome.exported + outcome.skipped,
        phase: "Validating compiled models".into(),
        current: String::new(),
    });
    Ok(())
}

fn run_model_compiler_batch(
    compiler: &Path,
    workspace: &Path,
    inputs: &[PathBuf],
    cancel: &AtomicBool,
) -> Result<std::process::Output, String> {
    let mut command = Command::new(compiler);
    command
        .arg("--quiet")
        .arg("compile")
        .arg("--force")
        .arg("--output-dir")
        .arg(workspace);
    command.args(inputs);
    command
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start model compiler: {error}"))?;
    let stdout = child.stdout.take().map(|mut stdout| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stdout, &mut bytes);
            bytes
        })
    });
    let stderr = child.stderr.take().map(|mut stderr| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stderr, &mut bytes);
            bytes
        })
    });
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("canceled".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(error) => return Err(format!("could not monitor model compiler: {error}")),
        }
    };
    let stdout = stdout
        .map(|reader| reader.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr
        .map(|reader| reader.join().unwrap_or_default())
        .unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn record_model_compilation_failure(
    outcome: &mut ModelCompileOutcome,
    report: &mut Option<std::io::BufWriter<fs::File>>,
    report_path: Option<&Path>,
    message: &str,
) -> Result<(), String> {
    let Some(report_path) = report_path else {
        return Err(message.to_owned());
    };
    outcome.skipped += 1;
    append_model_compilation_failure(report, report_path, message)
}

fn append_model_compilation_failure(
    report: &mut Option<std::io::BufWriter<fs::File>>,
    report_path: &Path,
    message: &str,
) -> Result<(), String> {
    if report.is_none() {
        let file = fs::File::create(report_path).map_err(|error| {
            format!(
                "could not create compilation failure report {}: {error}",
                report_path.display()
            )
        })?;
        *report = Some(std::io::BufWriter::new(file));
    }
    let writer = report.as_mut().expect("failure report was initialized");
    std::io::Write::write_all(writer, message.as_bytes())
        .and_then(|()| std::io::Write::write_all(writer, b"\n"))
        .map_err(|error| format!("could not write compilation failure report: {error}"))
}

fn compiler_diagnostics(stdout: &[u8], stderr: &[u8]) -> String {
    let mut diagnostics = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        if !diagnostics.is_empty() {
            diagnostics.push('\n');
        }
        diagnostics.push_str(stderr);
    }
    const MAX_DIAGNOSTICS: usize = 4_000;
    if diagnostics.len() > MAX_DIAGNOSTICS {
        diagnostics.truncate(MAX_DIAGNOSTICS);
        diagnostics.push('…');
    }
    diagnostics
}

fn missing_compiler_output_error(
    error: &std::io::Error,
    diagnostics: &str,
    diagnostics_reported: &mut bool,
) -> String {
    if diagnostics.is_empty() {
        format!("compiler did not create an output model: {error}")
    } else if *diagnostics_reported {
        format!(
            "compiler did not create an output model: {error} \
             (batch diagnostics reported with the first failed model)"
        )
    } else {
        *diagnostics_reported = true;
        format!("compiler did not create an output model: {diagnostics}")
    }
}

fn valid_compiled_model(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || bytes[..4] != [0, 0, 0, 0] {
        return false;
    }
    let structured_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let raw_size = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    12usize
        .checked_add(structured_size)
        .and_then(|size| size.checked_add(raw_size))
        .is_some_and(|size| size <= bytes.len())
}

fn files_in_dropped_directory(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut directories = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file() {
                files.push(entry.path());
                if files.len() > MAX_DROPPED_DIRECTORY_FILES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("directory contains more than {MAX_DROPPED_DIRECTORY_FILES} files"),
                    ));
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

fn add_incoming(
    archive: &mut Archive,
    batch: &mut AddBatch,
    path: PathBuf,
    entry: Entry,
    replacement_index: Option<usize>,
) -> bool {
    let key = Archive::incoming_entry_identity(&entry);
    let before = replacement_index.and_then(|index| archive.entries.get(index).cloned());
    let after = entry.clone();
    match archive.add_prepared_entry_unsorted(entry, replacement_index) {
        Ok(true) => {
            batch.replaced += 1;
            batch
                .changes
                .entry(key.clone())
                .and_modify(|change| change.after = Some(after.clone()))
                .or_insert(ResourceChange {
                    key,
                    before,
                    after: Some(after),
                });
            true
        }
        Ok(false) => {
            batch.added += 1;
            batch
                .changes
                .entry(key.clone())
                .and_modify(|change| change.after = Some(after.clone()))
                .or_insert(ResourceChange {
                    key,
                    before,
                    after: Some(after),
                });
            true
        }
        Err(error) => {
            batch.failures.push(format!("{}: {error}", path.display()));
            false
        }
    }
}

fn resource_transaction(
    label: String,
    changes: Vec<ResourceChange>,
    selected_before: BTreeSet<(String, u16)>,
    selected_after: BTreeSet<(String, u16)>,
    category_before: Option<String>,
    category_after: Option<String>,
    dirty_before: bool,
) -> EditTransaction {
    EditTransaction {
        label,
        edit: ArchiveEdit::Resources(changes),
        selected_before,
        selected_after,
        category_before,
        category_after,
        dirty_before,
        dirty_after: true,
        estimated_bytes: 0,
    }
}

fn apply_archive_edit(archive: &mut Archive, edit: &ArchiveEdit, redo: bool) -> Result<(), String> {
    match edit {
        ArchiveEdit::Resources(changes) => {
            for entry in changes.iter().filter_map(|change| {
                if redo {
                    change.after.as_ref()
                } else {
                    change.before.as_ref()
                }
            }) {
                validate_history_entry(entry)?;
            }
            for change in changes {
                let index = archive.entries.iter().position(|entry| {
                    entry.type_id == change.key.1 && entry.name.eq_ignore_ascii_case(&change.key.0)
                });
                let target = if redo {
                    change.after.as_ref()
                } else {
                    change.before.as_ref()
                };
                match (index, target) {
                    (Some(index), Some(entry)) => archive.entries[index] = entry.clone(),
                    (Some(index), None) => {
                        archive.entries.remove(index);
                    }
                    (None, Some(entry)) => archive.entries.push(entry.clone()),
                    (None, None) => {}
                }
            }
            archive.finish_bulk_add();
        }
        ArchiveEdit::Description { before, after } => {
            archive.set_description(if redo { after.clone() } else { before.clone() });
        }
    }
    Ok(())
}

fn validate_history_entry(entry: &Entry) -> Result<(), String> {
    match &entry.data {
        EntryData::ArchiveSlice { path, offset, size } => {
            let length = fs::metadata(path)
                .map_err(|error| format!("{} is unavailable: {error}", path.display()))?
                .len();
            if offset.checked_add(*size).is_none_or(|end| end > length) {
                return Err(format!(
                    "{} no longer contains the recorded resource data",
                    path.display()
                ));
            }
        }
        EntryData::ExternalFile(path) => {
            if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
                return Err(format!("{} is no longer available", path.display()));
            }
        }
        EntryData::Memory(_) => {}
    }
    Ok(())
}

fn estimate_transaction_bytes(transaction: &EditTransaction) -> usize {
    let mut bytes = std::mem::size_of::<EditTransaction>()
        .saturating_add(transaction.label.len())
        .saturating_add(estimate_keys(&transaction.selected_before))
        .saturating_add(estimate_keys(&transaction.selected_after))
        .saturating_add(transaction.category_before.as_ref().map_or(0, String::len))
        .saturating_add(transaction.category_after.as_ref().map_or(0, String::len));
    match &transaction.edit {
        ArchiveEdit::Resources(changes) => {
            bytes = bytes.saturating_add(
                changes
                    .iter()
                    .map(|change| {
                        std::mem::size_of::<ResourceChange>()
                            .saturating_add(change.key.0.len())
                            .saturating_add(change.before.as_ref().map_or(0, estimate_entry_bytes))
                            .saturating_add(change.after.as_ref().map_or(0, estimate_entry_bytes))
                    })
                    .fold(0usize, usize::saturating_add),
            );
        }
        ArchiveEdit::Description { before, after } => {
            bytes = bytes
                .saturating_add(before.len())
                .saturating_add(after.len());
        }
    }
    bytes
}

fn estimate_keys(keys: &BTreeSet<(String, u16)>) -> usize {
    keys.iter()
        .map(|(name, _)| std::mem::size_of::<(String, u16)>().saturating_add(name.len()))
        .fold(0usize, usize::saturating_add)
}

fn estimate_entry_bytes(entry: &Entry) -> usize {
    let data = match &entry.data {
        EntryData::ArchiveSlice { path, .. } | EntryData::ExternalFile(path) => {
            path.to_string_lossy().len()
        }
        EntryData::Memory(bytes) => bytes.len(),
    };
    std::mem::size_of::<Entry>()
        .saturating_add(entry.name.len())
        .saturating_add(data)
}

fn summarize_import_failures(failures: &[String]) -> String {
    const SHOWN_FAILURES: usize = 25;
    let mut message = failures
        .iter()
        .take(SHOWN_FAILURES)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if failures.len() > SHOWN_FAILURES {
        message.push_str(&format!(
            "\n… and {} more file(s) could not be imported",
            failures.len() - SHOWN_FAILURES
        ));
    }
    message
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

fn estimated_time_remaining(elapsed: Duration, completed: usize, total: usize) -> Option<Duration> {
    if completed == 0 || total == 0 {
        return None;
    }
    if completed >= total {
        return Some(Duration::ZERO);
    }

    let seconds = elapsed.as_secs_f64() * (total - completed) as f64 / completed as f64;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(Duration::from_secs(
        seconds.ceil().min(u64::MAX as f64) as u64
    ))
}

fn format_compilation_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn native_drag_local_position(ctx: &egui::Context) -> Option<egui::Pos2> {
    let (x, y) = drag_out::pointer_position()?;
    let window_origin = ctx.input(|input| input.viewport().inner_rect.map(|rect| rect.min))?;
    let pixels_per_point = ctx.pixels_per_point();
    Some(egui::pos2(
        x as f32 / pixels_per_point - window_origin.x,
        y as f32 / pixels_per_point - window_origin.y,
    ))
}

fn resource_tree_row(ui: &mut egui::Ui, active: bool, label: &str, count: usize) -> egui::Response {
    // Keep the resource category selection in the same blue-gray family as
    // the active archive tab, but slightly stronger for clear sidebar focus.
    let selection = if ui.visuals().dark_mode {
        Color32::from_rgb(35, 70, 88)
    } else {
        Color32::from_rgb(160, 196, 220)
    };
    let row = egui::Frame::new()
        .fill(if active {
            selection
        } else {
            Color32::TRANSPARENT
        })
        .stroke(if active {
            egui::Stroke::new(1.0, selection.gamma_multiply(0.9))
        } else {
            egui::Stroke::NONE
        })
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::Label::new(RichText::new(label).color(if active {
                        ui.visuals().strong_text_color()
                    } else {
                        ui.visuals().text_color()
                    }))
                    .selectable(false)
                    .sense(egui::Sense::click()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(count.to_string()).color(if active {
                        ui.visuals().strong_text_color()
                    } else {
                        ui.visuals().text_color()
                    }));
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

fn resource_matches_filter(name: &str, extension: &str, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let extension_filter = filter.strip_prefix('.').unwrap_or(filter);
    name.to_ascii_lowercase().contains(filter) || extension.contains(extension_filter)
}

fn requested_virtualized_scroll_offset(
    row: usize,
    row_height: f32,
    row_spacing: f32,
    current_offset: f32,
    viewport_height: f32,
) -> Option<f32> {
    let row_top = row as f32 * (row_height + row_spacing);
    let row_bottom = row_top + row_height;
    (row_top < current_offset || row_bottom > current_offset + viewport_height)
        .then_some((row_top - (viewport_height - row_height) * 0.5).max(0.0))
}

fn sort_button(
    ui: &mut egui::Ui,
    label: &str,
    width: f32,
    column: SortColumn,
    current: SortColumn,
    ascending: bool,
) -> bool {
    let arrow = if column == current {
        if ascending { " ^" } else { " v" }
    } else {
        ""
    };
    ui.add_sized(
        [width, ui.spacing().interact_size.y],
        egui::Button::new(format!("{label}{arrow}")),
    )
    .clicked()
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
                "hak" | "erf" | "mod" | "sav" | "bif"
            )
        })
}

fn internal_drag_returns_to_source(origin: &InternalDragOrigin, target_tab: Option<usize>) -> bool {
    target_tab == Some(origin.source_tab)
}

fn prune_internal_drag_origins(origins: &mut BTreeMap<PathBuf, InternalDragOrigin>) {
    origins.retain(|directory, _| directory.is_dir());
    while origins.len() > MAX_INTERNAL_DRAG_ORIGINS {
        origins.pop_first();
    }
}

fn category_for(extension: &str) -> &'static str {
    match extension {
        "2da" => "2DA",
        "set" => "Tileset",
        "nss" | "ncs" => "Scripts",
        "mdl" | "mtr" | "wok" | "pwk" | "dwk" => "Models",
        "tga" | "dds" | "plt" | "txi" | "bmp" | "jpg" | "png" | "ktx" => "Textures",
        "wav" | "bmu" => "Music",
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

fn unsupported_import_extension(path: &Path) -> Option<String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    match extension {
        Some(extension) if resource_types::is_nwn_ee_extension(&extension) => None,
        Some(extension) => Some(format!(".{extension}")),
        None => Some("(no extension)".to_owned()),
    }
}

fn unsupported_archive_resource_summary(archive: &Archive) -> Option<String> {
    let mut types = BTreeMap::<String, usize>::new();
    for entry in &archive.entries {
        if !resource_types::is_nwn_ee_type(entry.type_id) {
            *types.entry(format!(".{}", entry.extension())).or_default() += 1;
        }
    }
    let total = types.values().sum::<usize>();
    (total > 0).then(|| {
        let mut labels = types
            .iter()
            .take(6)
            .map(|(extension, count)| format!("{extension} ({count})"))
            .collect::<Vec<_>>();
        if types.len() > labels.len() {
            labels.push(format!("and {} more type(s)", types.len() - labels.len()));
        }
        format!("{total} unsupported resource(s): {}", labels.join(", "))
    })
}

fn tileset_unlocalized_name(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes).lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("UnlocalizedName")
            .then(|| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn entry_matches_category(entry: &archive::Entry, category: Option<&str>) -> bool {
    match category {
        None => true,
        Some("New") => entry.is_new(),
        Some("Models" | "Models All") => category_for(&entry.extension()) == "Models",
        Some("Models Compiled") => entry.model_compiled() == Some(true),
        Some("Models Uncompiled") => entry.model_compiled() == Some(false),
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

fn mtr_diffuse_texture(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes).lines().find_map(|line| {
        let line = line.split("//").next().unwrap_or_default().trim();
        let mut fields = line.split_whitespace();
        let directive = fields.next()?;
        if !directive.eq_ignore_ascii_case("texture0") {
            return None;
        }
        let value = fields.next()?.trim_matches('"');
        (!value.is_empty() && !value.eq_ignore_ascii_case("null")).then(|| value.to_owned())
    })
}

fn dot3(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn show_model_scene(
    ui: &mut egui::Ui,
    scene: &Result<mdl::Scene, String>,
    textures: &BTreeMap<String, ModelTexture>,
    yaw: &mut f32,
    pitch: &mut f32,
    zoom: &mut f32,
) {
    let scene = match scene {
        Ok(scene) => scene,
        Err(error) => {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.colored_label(Color32::LIGHT_RED, error);
            });
            return;
        }
    };
    let desired = egui::vec2(
        ui.available_width().max(160.0),
        ui.available_height().max(300.0),
    );
    let (response, painter) = ui.allocate_painter(desired, egui::Sense::click_and_drag());
    let rect = response.rect.shrink(8.0);
    painter.rect_filled(rect, 4.0, Color32::from_rgb(16, 20, 23));
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
    draw_model_viewport_grid(&painter, rect);
    if response.double_clicked_by(egui::PointerButton::Primary) {
        reset_model_view(yaw, pitch, zoom);
    } else if response.dragged_by(egui::PointerButton::Primary) {
        let delta = ui.ctx().input(|input| input.pointer.delta());
        *yaw += delta.x * 0.012;
        *pitch = (*pitch + delta.y * 0.012).clamp(-1.5, 1.5);
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    } else if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    if response.hovered() {
        let scroll = ui.ctx().input(|input| input.smooth_scroll_delta.y);
        if scroll != 0.0 {
            *zoom = (*zoom * (scroll * 0.0025).exp()).clamp(0.15, 8.0);
        }
    }
    let center = [
        (scene.bounds_min[0] + scene.bounds_max[0]) * 0.5,
        (scene.bounds_min[1] + scene.bounds_max[1]) * 0.5,
        (scene.bounds_min[2] + scene.bounds_max[2]) * 0.5,
    ];
    let extent = (0..3)
        .map(|axis| scene.bounds_max[axis] - scene.bounds_min[axis])
        .fold(0.0_f32, f32::max)
        .max(1.0e-4);
    let scale = rect.width().min(rect.height()) * 0.72 / extent * *zoom;
    let (sin_yaw, cos_yaw) = yaw.sin_cos();
    let (sin_pitch, cos_pitch) = pitch.sin_cos();
    let rotate = |point: [f32; 3]| {
        let point = [
            point[0] - center[0],
            point[1] - center[1],
            point[2] - center[2],
        ];
        let x = cos_yaw * point[0] - sin_yaw * point[1];
        let y = sin_yaw * point[0] + cos_yaw * point[1];
        [
            x,
            cos_pitch * point[2] - sin_pitch * y,
            sin_pitch * point[2] + cos_pitch * y,
        ]
    };
    struct DrawFace {
        points: [egui::Pos2; 3],
        depth: f32,
        color: Color32,
        vertex_colors: [Color32; 3],
        texture: Option<(egui::TextureId, [egui::Pos2; 3])>,
    }
    let mut draw_faces = Vec::with_capacity(scene.face_count.min(300_000));
    let light = [0.35_f32, -0.45, 0.82];
    for mesh in &scene.meshes {
        let rotated: Vec<[f32; 3]> = mesh.vertices.iter().copied().map(rotate).collect();
        let rotated_normals: Vec<[f32; 3]> = mesh
            .normals
            .iter()
            .map(|normal| {
                let x = cos_yaw * normal[0] - sin_yaw * normal[1];
                let y = sin_yaw * normal[0] + cos_yaw * normal[1];
                [
                    x,
                    cos_pitch * normal[2] - sin_pitch * y,
                    sin_pitch * normal[2] + cos_pitch * y,
                ]
            })
            .collect();
        let texture = mesh
            .texture_name
            .as_deref()
            .and_then(|name| {
                name.rsplit_once('.')
                    .map_or(Some(name), |(stem, _)| Some(stem))
            })
            .and_then(|name| textures.get(&name.to_ascii_lowercase()));
        let texture = texture.or_else(|| textures.get(&scene.name.to_ascii_lowercase()));
        for (face_index, face) in mesh.faces.iter().enumerate() {
            if draw_faces.len() >= 300_000 {
                break;
            }
            let Some(&a) = rotated.get(face[0] as usize) else {
                continue;
            };
            let Some(&b) = rotated.get(face[1] as usize) else {
                continue;
            };
            let Some(&c) = rotated.get(face[2] as usize) else {
                continue;
            };
            let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let normal = [
                ab[1] * ac[2] - ab[2] * ac[1],
                ab[2] * ac[0] - ab[0] * ac[2],
                ab[0] * ac[1] - ab[1] * ac[0],
            ];
            let length =
                (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
            let edge_squared = dot3(ab, ab).max(dot3(ac, ac)).max(dot3(
                [c[0] - b[0], c[1] - b[1], c[2] - b[2]],
                [c[0] - b[0], c[1] - b[1], c[2] - b[2]],
            ));
            // Bad or helper geometry occasionally contains almost zero-area
            // triangles with extremely long edges. Their outlines appear as
            // rays across the preview, so discard only these degenerates.
            if !length.is_finite()
                || length < 1.0e-8
                || edge_squared > 0.0 && length / edge_squared < 1.0e-5
            {
                continue;
            }
            let illumination =
                ((normal[0] * light[0] + normal[1] * light[1] + normal[2] * light[2]) / length)
                    .abs()
                    .mul_add(0.62, 0.32)
                    .clamp(0.22, 1.0);
            let color = visible_model_color(mesh.color, illumination);
            let vertex_colors = face.map(|index| {
                let vertex_illumination = rotated_normals
                    .get(index as usize)
                    .map(|normal| {
                        dot3(*normal, light)
                            .abs()
                            .mul_add(0.68, 0.24)
                            .clamp(0.12, 1.0)
                    })
                    .unwrap_or(illumination);
                Color32::from_gray((vertex_illumination * 255.0) as u8)
            });
            let project = |point: [f32; 3]| {
                egui::pos2(
                    rect.center().x + point[0] * scale,
                    rect.center().y - point[1] * scale,
                )
            };
            let texture = texture.and_then(|texture| {
                let indices = mesh.texture_faces.get(face_index)?;
                let uv = indices.map(|index| {
                    let value = mesh.texture_vertices.get(index as usize).copied()?;
                    let vertical = if texture.flip_vertical {
                        1.0 - value[1]
                    } else {
                        value[1]
                    };
                    Some(egui::pos2(value[0], vertical))
                });
                Some((texture.handle.id(), [uv[0]?, uv[1]?, uv[2]?]))
            });
            draw_faces.push(DrawFace {
                points: [project(a), project(b), project(c)],
                depth: (a[2] + b[2] + c[2]) / 3.0,
                color: if texture.is_some() {
                    Color32::from_gray((illumination * 255.0) as u8)
                } else {
                    color
                },
                vertex_colors,
                texture,
            });
        }
    }
    draw_faces.sort_by(|left, right| left.depth.total_cmp(&right.depth));
    for face in draw_faces {
        if let Some((texture, uv)) = face.texture {
            let mut mesh = egui::Mesh::with_texture(texture);
            for ((pos, uv), color) in face.points.into_iter().zip(uv).zip(face.vertex_colors) {
                mesh.vertices.push(egui::epaint::Vertex { pos, uv, color });
            }
            mesh.indices.extend_from_slice(&[0, 1, 2]);
            painter.add(egui::Shape::mesh(mesh));
        } else {
            painter.add(egui::Shape::convex_polygon(
                face.points.to_vec(),
                face.color,
                egui::Stroke::NONE,
            ));
        }
    }
    painter.text(
        rect.left_top() + egui::vec2(8.0, 8.0),
        egui::Align2::LEFT_TOP,
        format!(
            "{} — {} vertices, {} faces",
            scene.name, scene.vertex_count, scene.face_count
        ),
        egui::FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
    painter.text(
        rect.left_bottom() + egui::vec2(8.0, -8.0),
        egui::Align2::LEFT_BOTTOM,
        "Drag to rotate • Wheel to zoom • Double-click to reset",
        egui::FontId::proportional(11.0),
        ui.visuals().weak_text_color(),
    );
}

fn draw_model_viewport_grid(painter: &egui::Painter, rect: egui::Rect) {
    const GRID_SPACING: f32 = 32.0;
    let grid = egui::Stroke::new(1.0, Color32::from_rgb(24, 31, 36));
    let axis = egui::Stroke::new(1.0, Color32::from_rgb(37, 52, 60));
    let center = rect.center();
    let mut x = center.x;
    while x >= rect.left() {
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            grid,
        );
        x -= GRID_SPACING;
    }
    let mut x = center.x + GRID_SPACING;
    while x <= rect.right() {
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            grid,
        );
        x += GRID_SPACING;
    }
    let mut y = center.y;
    while y >= rect.top() {
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            grid,
        );
        y -= GRID_SPACING;
    }
    let mut y = center.y + GRID_SPACING;
    while y <= rect.bottom() {
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            grid,
        );
        y += GRID_SPACING;
    }
    painter.line_segment(
        [
            egui::pos2(center.x, rect.top()),
            egui::pos2(center.x, rect.bottom()),
        ],
        axis,
    );
    painter.line_segment(
        [
            egui::pos2(rect.left(), center.y),
            egui::pos2(rect.right(), center.y),
        ],
        axis,
    );
}

fn visible_model_color(material: [f32; 3], illumination: f32) -> Color32 {
    let material = material.map(|channel| channel.clamp(0.0, 1.0));
    let luminance = material[0] * 0.2126 + material[1] * 0.7152 + material[2] * 0.0722;
    let fallback_weight = ((0.18 - luminance) / 0.18).clamp(0.0, 1.0);
    let fallback = [0.48, 0.56, 0.61];
    Color32::from_rgb(
        ((material[0] + (fallback[0] - material[0]) * fallback_weight) * illumination * 255.0)
            as u8,
        ((material[1] + (fallback[1] - material[1]) * fallback_weight) * illumination * 255.0)
            as u8,
        ((material[2] + (fallback[2] - material[2]) * fallback_weight) * illumination * 255.0)
            as u8,
    )
}

fn reset_model_view(yaw: &mut f32, pitch: &mut f32, zoom: &mut f32) {
    *yaw = -0.65;
    *pitch = 0.35;
    *zoom = 1.0;
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

    let palettes = default_plt_palettes()?;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for pixel in bytes[HEADER_SIZE..HEADER_SIZE + payload_size].chunks_exact(2) {
        let color = palettes
            .get(pixel[1] as usize)
            .and_then(|palette| palette.get(pixel[0] as usize))
            .copied()
            .unwrap_or([255, 0, 255, 255]);
        // NWN Explorer's fixed-function model renderer does not blend these
        // palette alpha values for ordinary meshes. Keep the preview opaque
        // to avoid exposing back-facing triangles through solid equipment.
        rgba.extend_from_slice(&[color[0], color[1], color[2], 255]);
    }

    let buffer = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| "Could not construct PLT preview image".to_owned())?;
    Ok(image::DynamicImage::ImageRgba8(buffer))
}

fn default_plt_palettes() -> Result<&'static [[[u8; 4]; 256]; 10], String> {
    static PALETTES: OnceLock<Result<[[[u8; 4]; 256]; 10], String>> = OnceLock::new();
    PALETTES
        .get_or_init(|| {
            const SOURCES: [&[u8]; 10] = [
                include_bytes!("../assets/pal_skin01.tga"),
                include_bytes!("../assets/pal_hair01.tga"),
                include_bytes!("../assets/pal_armor01.tga"),
                include_bytes!("../assets/pal_armor02.tga"),
                include_bytes!("../assets/pal_cloth01.tga"),
                include_bytes!("../assets/pal_cloth01.tga"),
                include_bytes!("../assets/pal_leath01.tga"),
                include_bytes!("../assets/pal_leath01.tga"),
                include_bytes!("../assets/pal_tattoo01.tga"),
                include_bytes!("../assets/pal_tattoo01.tga"),
            ];
            let mut palettes = [[[0_u8; 4]; 256]; 10];
            for (layer, source) in SOURCES.iter().enumerate() {
                let image = image::load_from_memory_with_format(source, image::ImageFormat::Tga)
                    .map_err(|error| format!("Could not decode PLT palette {layer}: {error}"))?
                    .into_rgba8();
                if image.width() != 256 || image.height() != 256 {
                    return Err(format!(
                        "PLT palette {layer} has unexpected dimensions {} x {}",
                        image.width(),
                        image.height()
                    ));
                }
                // NWN Explorer defaults each material selection to palette
                // zero, represented by the visually top row of these TGAs.
                for index in 0..256 {
                    palettes[layer][index as usize] = image.get_pixel(index, 0).0;
                }
            }
            Ok(palettes)
        })
        .as_ref()
        .map_err(Clone::clone)
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

    #[derive(Default)]
    struct TestStorage(BTreeMap<String, String>);

    impl eframe::Storage for TestStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }

        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }

        fn flush(&mut self) {}
    }

    #[test]
    fn compilation_times_are_formatted_readably() {
        assert_eq!(format_compilation_duration(Duration::from_secs(5)), "5s");
        assert_eq!(
            format_compilation_duration(Duration::from_secs(65)),
            "1m 05s"
        );
        assert_eq!(
            format_compilation_duration(Duration::from_secs(3_661)),
            "1h 01m 01s"
        );
    }

    #[test]
    fn compilation_eta_uses_completed_work_rate() {
        assert_eq!(
            estimated_time_remaining(Duration::from_secs(20), 0, 100),
            None
        );
        assert_eq!(
            estimated_time_remaining(Duration::from_secs(20), 20, 100),
            Some(Duration::from_secs(80))
        );
        assert_eq!(
            estimated_time_remaining(Duration::from_secs(20), 100, 100),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn dotted_extensions_match_resource_filters() {
        assert!(resource_matches_filter("nwscript", "nss", ".nss"));
        assert!(resource_matches_filter("texture", "dds", ".dds"));
        assert!(resource_matches_filter("my_model", "mdl", "model"));
        assert!(!resource_matches_filter("nwscript", "nss", ".dds"));
    }

    #[test]
    fn recognizes_bif_archive_extensions() {
        assert!(is_archive_path(Path::new("aurora_tds.bif")));
        assert!(!is_archive_path(Path::new("not_an_archive.bifx")));
    }

    #[test]
    fn recognizes_nwn_installation_roots_and_their_data_directories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("Neverwinter Nights");
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("nwn_base.key"), b"KEY V1  ").unwrap();
        let expected = fs::canonicalize(&root).unwrap();
        assert_eq!(normalize_nwn_installation(&root), Some(expected.clone()));
        assert_eq!(normalize_nwn_installation(&data), Some(expected));
    }

    #[test]
    fn reads_steam_library_and_nwn_manifest_paths() {
        let directory = tempfile::tempdir().unwrap();
        let libraries = directory.path().join("libraryfolders.vdf");
        fs::write(
            &libraries,
            "\"libraryfolders\"\n{\n \"1\" { \n  \"path\" \"D:\\\\SteamLibrary\"\n }\n}\n",
        )
        .unwrap();
        assert_eq!(
            steam_library_paths(&libraries),
            vec![PathBuf::from(r"D:\SteamLibrary")]
        );
        let manifest = directory.path().join("appmanifest_704450.acf");
        fs::write(
            &manifest,
            "\"AppState\"\n{\n \"appid\" \"704450\"\n \"installdir\" \"Neverwinter Nights\"\n}\n",
        )
        .unwrap();
        assert_eq!(
            steam_install_directory(&manifest).as_deref(),
            Some("Neverwinter Nights")
        );
    }

    #[test]
    fn resolves_diffuse_texture_from_enhanced_edition_material() {
        let material = b"// material\nrenderhint NormalAndSpecMapped\ntexture0 mehrunes\ntexture1 mehrunes_n\n";
        assert_eq!(mtr_diffuse_texture(material).as_deref(), Some("mehrunes"));
        assert_eq!(mtr_diffuse_texture(b"texture0 null"), None);
    }

    #[test]
    fn internal_drag_is_ignored_only_in_its_source_tab() {
        let origin = InternalDragOrigin { source_tab: 3 };
        assert!(internal_drag_returns_to_source(&origin, Some(3)));
        assert!(!internal_drag_returns_to_source(&origin, Some(2)));
        assert!(!internal_drag_returns_to_source(&origin, None));
    }

    #[test]
    fn internal_drag_origin_bookkeeping_is_bounded_and_prunes_expired_paths() {
        let directories = (0..MAX_INTERNAL_DRAG_ORIGINS + 4)
            .map(|_| tempfile::tempdir().unwrap())
            .collect::<Vec<_>>();
        let mut origins = directories
            .iter()
            .enumerate()
            .map(|(source_tab, directory)| {
                (
                    directory.path().to_path_buf(),
                    InternalDragOrigin { source_tab },
                )
            })
            .collect();
        prune_internal_drag_origins(&mut origins);
        assert_eq!(origins.len(), MAX_INTERNAL_DRAG_ORIGINS);

        let expired = origins.keys().next().unwrap().clone();
        fs::remove_dir_all(expired).unwrap();
        prune_internal_drag_origins(&mut origins);
        assert_eq!(origins.len(), MAX_INTERNAL_DRAG_ORIGINS - 1);
    }

    #[test]
    fn clearing_nwn_installation_removes_the_persisted_path() {
        let mut storage = TestStorage::default();
        let path = Path::new("/games/Neverwinter Nights");
        save_nwn_installation(&mut storage, Some(path));
        assert_eq!(
            eframe::get_value::<String>(&storage, "nwn_installation").as_deref(),
            Some("/games/Neverwinter Nights")
        );
        save_nwn_installation(&mut storage, None);
        assert!(!storage.0.contains_key("nwn_installation"));
    }

    #[test]
    fn dropped_directories_expand_to_their_files() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let first = directory.path().join("first.mdl");
        let second = nested.join("second.dds");
        fs::write(&first, b"model").unwrap();
        fs::write(&second, b"texture").unwrap();

        let files = files_in_dropped_directory(directory.path()).unwrap();
        assert_eq!(files, vec![first, second]);
    }

    #[test]
    fn distant_virtualized_rows_include_inter_row_spacing() {
        let offset = requested_virtualized_scroll_offset(10_000, 20.0, 4.0, 0.0, 400.0)
            .expect("a distant row should request scrolling");
        let expected = 10_000.0 * 24.0 - 190.0;
        assert!((offset - expected).abs() < f32::EPSILON);
        assert_eq!(
            requested_virtualized_scroll_offset(10, 20.0, 4.0, 200.0, 400.0),
            None
        );
    }

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
        assert_eq!(category_for("set"), "Tileset");
        assert_eq!(category_for("bmu"), "Music");
        assert_eq!(category_for("mp3"), "Other");
        assert_eq!(category_for("ogg"), "Other");
        assert_eq!(
            tileset_unlocalized_name(b"[GENERAL]\nUnlocalizedName=DDIWD: Frozen Interiors\n"),
            Some("DDIWD: Frozen Interiors".into())
        );
        assert_eq!(tileset_unlocalized_name(b"UnlocalizedName=\n"), None);
        assert!(is_text_type("mtr"));
        assert!(is_text_type("lua"));
        assert!(is_text_type("ids"));
        assert!(is_text_type("shd"));
        assert!(is_text_type("jui"));
    }

    #[test]
    fn accepts_only_nwn_ee_resource_types_for_import() {
        for extension in ["mp3", "ogg", "pdf", "zip", "fbx", "obj", "md"] {
            assert_eq!(
                unsupported_import_extension(Path::new(&format!("resource.{extension}"))),
                Some(format!(".{extension}"))
            );
        }
        for extension in ["png", "jpg", "mdl", "bmu", "wav", "utc", "jui"] {
            assert_eq!(
                unsupported_import_extension(Path::new(&format!("resource.{extension}"))),
                None
            );
        }
    }

    #[test]
    fn reports_unsupported_types_in_existing_archives_without_rejecting_them() {
        let directory = tempfile::tempdir().unwrap();
        let zip = directory.path().join("legacy.zip");
        fs::write(&zip, b"legacy resource").unwrap();
        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        archive.add_file(&zip).unwrap();

        assert_eq!(
            unsupported_archive_resource_summary(&archive),
            Some("1 unsupported resource(s): .zip (1)".into())
        );
        assert_eq!(archive.entries.len(), 1);
    }

    #[test]
    fn separates_compiled_and_uncompiled_model_categories() {
        let directory = tempfile::tempdir().unwrap();
        let compiled_path = directory.path().join("compiled.mdl");
        let uncompiled_path = directory.path().join("uncompiled.mdl");
        let material_path = directory.path().join("surface.mtr");
        fs::write(&compiled_path, [0_u8; 12]).unwrap();
        fs::write(
            &uncompiled_path,
            b"newmodel example\nbeginmodelgeom example\n",
        )
        .unwrap();
        fs::write(&material_path, b"renderhint NormalAndSpecMapped\n").unwrap();

        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        archive.add_file(compiled_path).unwrap();
        archive.add_file(uncompiled_path).unwrap();
        archive.add_file(material_path).unwrap();

        let compiled = archive
            .entries
            .iter()
            .find(|entry| entry.name == "compiled")
            .unwrap();
        let uncompiled = archive
            .entries
            .iter()
            .find(|entry| entry.name == "uncompiled")
            .unwrap();
        let material = archive
            .entries
            .iter()
            .find(|entry| entry.name == "surface")
            .unwrap();

        assert!(entry_matches_category(compiled, Some("Models All")));
        assert!(entry_matches_category(compiled, Some("Models Compiled")));
        assert!(!entry_matches_category(compiled, Some("Models Uncompiled")));
        assert!(entry_matches_category(uncompiled, Some("Models All")));
        assert!(entry_matches_category(
            uncompiled,
            Some("Models Uncompiled")
        ));
        assert!(!entry_matches_category(uncompiled, Some("Models Compiled")));
        assert!(entry_matches_category(material, Some("Models All")));
        assert!(!entry_matches_category(material, Some("Models Compiled")));
        assert!(!entry_matches_category(material, Some("Models Uncompiled")));
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
        let palettes = default_plt_palettes().expect("bundled palettes should decode");
        let expected_cloth = palettes[4][255];
        let expected_second_cloth = palettes[5][128];
        assert_eq!(
            pixels.get_pixel(0, 0).0,
            [expected_cloth[0], expected_cloth[1], expected_cloth[2], 255]
        );
        assert_eq!(
            pixels.get_pixel(1, 0).0,
            [
                expected_second_cloth[0],
                expected_second_cloth[1],
                expected_second_cloth[2],
                255
            ]
        );
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

    #[test]
    fn stages_recursive_supermodels_from_an_open_archive() {
        let directory = tempfile::tempdir().unwrap();
        let dependency_path = directory.path().join("pfg2.mdl");
        fs::write(
            &dependency_path,
            b"newmodel pfg2\nsetsupermodel pfg2 a_fa2\nbeginmodelgeom pfg2\nendmodelgeom pfg2\ndonemodel pfg2\n",
        )
        .unwrap();
        let root_path = directory.path().join("a_fa2.mdl");
        fs::write(
            &root_path,
            b"newmodel a_fa2\nsetsupermodel a_fa2 NULL\nbeginmodelgeom a_fa2\nendmodelgeom a_fa2\ndonemodel a_fa2\n",
        )
        .unwrap();

        let active = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        let mut dependencies = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        dependencies.add_file(&dependency_path).unwrap();
        dependencies.add_file(&root_path).unwrap();
        let tabs = vec![TabState::new(dependencies, false)];
        let mut resolver = ModelDependencyResolver::new(&active, &tabs, None);
        let workspace = tempfile::tempdir().unwrap();
        let source = b"newmodel cloak\nsetsupermodel cloak pfg2\nbeginmodelgeom cloak\nendmodelgeom cloak\ndonemodel cloak\n";
        let origins = stage_model_dependencies(
            &mut resolver,
            source,
            workspace.path(),
            &mut BTreeSet::new(),
        )
        .expect("the open archive should satisfy the complete supermodel chain");

        assert_eq!(origins.len(), 2);
        assert!(workspace.path().join("pfg2.mdl").is_file());
        assert!(workspace.path().join("a_fa2.mdl").is_file());
        assert!(origins.iter().all(|origin| origin.contains("open")));
    }

    #[test]
    fn allows_compilation_when_a_declared_supermodel_is_missing() {
        let active = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        let mut resolver = ModelDependencyResolver::new(&active, &[], None);
        let workspace = tempfile::tempdir().unwrap();
        let source = b"newmodel custom\nsetsupermodel custom missing_pheno\nbeginmodelgeom custom\nendmodelgeom custom\ndonemodel custom\n";
        let origins = stage_model_dependencies(
            &mut resolver,
            source,
            workspace.path(),
            &mut BTreeSet::new(),
        )
        .expect("a missing supermodel should not prevent standalone compilation");

        assert!(origins.is_empty());
        assert!(!workspace.path().join("missing_pheno.mdl").exists());
    }

    #[test]
    fn bundled_compiler_batches_multiple_models() {
        let workspace = tempfile::tempdir().unwrap();
        let source = |name: &str| {
            format!(
                "newmodel {name}\nsetsupermodel {name} NULL\nbeginmodelgeom {name}\nnode trimesh mesh\n parent NULL\n verts 3\n 0 0 0\n 1 0 0\n 0 1 0\n faces 1\n 0 1 2 1 0 1 2 0\nendnode\nendmodelgeom {name}\ndonemodel {name}\n"
            )
        };
        let inputs = ["first", "second"]
            .into_iter()
            .map(|name| {
                let path = workspace.path().join(format!("{name}.mdl.ascii"));
                fs::write(&path, source(name)).unwrap();
                path
            })
            .collect::<Vec<_>>();
        let compiler = bundled_model_compiler().unwrap();
        let output = run_model_compiler_batch(
            &compiler.path,
            workspace.path(),
            &inputs,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(
            output.status.success(),
            "{}",
            compiler_diagnostics(&output.stdout, &output.stderr)
        );
        assert!(valid_compiled_model(
            &fs::read(workspace.path().join("first.mdl")).unwrap()
        ));
        assert!(valid_compiled_model(
            &fs::read(workspace.path().join("second.mdl")).unwrap()
        ));
    }

    #[test]
    fn batch_compiler_diagnostics_are_reported_only_once() {
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "missing output");
        let diagnostics = "compiler diagnostic details";
        let mut reported = false;

        let first = missing_compiler_output_error(&error, diagnostics, &mut reported);
        let second = missing_compiler_output_error(&error, diagnostics, &mut reported);

        assert!(first.contains(diagnostics));
        assert!(!second.contains(diagnostics));
        assert!(second.contains("reported with the first failed model"));
    }

    #[test]
    fn dark_untextured_models_remain_visible() {
        let color = visible_model_color([0.0, 0.0, 0.0], 0.22);
        assert!(color.r() >= 24);
        assert!(color.g() >= 28);
        assert!(color.b() >= 30);
    }

    #[test]
    fn reset_model_view_restores_the_default_camera() {
        let (mut yaw, mut pitch, mut zoom) = (1.0, -1.0, 4.0);
        reset_model_view(&mut yaw, &mut pitch, &mut zoom);
        assert_eq!((yaw, pitch, zoom), (-0.65, 0.35, 1.0));
    }

    #[test]
    fn resource_edit_round_trips_added_and_replaced_entries() {
        let original_directory = tempfile::tempdir().unwrap();
        let incoming_directory = tempfile::tempdir().unwrap();
        let original = original_directory.path().join("sample.txt");
        let replacement = incoming_directory.path().join("sample.txt");
        let added_path = incoming_directory.path().join("added.txt");
        fs::write(&original, b"original").unwrap();
        fs::write(&replacement, b"replacement").unwrap();
        fs::write(&added_path, b"added").unwrap();

        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        archive.add_file(&original).unwrap();
        let before = archive.entries[0].clone();
        let after = archive.prepare_incoming_file(&replacement).unwrap();
        let added = archive.prepare_incoming_file(&added_path).unwrap();
        let edit = ArchiveEdit::Resources(vec![
            ResourceChange {
                key: ("sample".into(), before.type_id),
                before: Some(before),
                after: Some(after),
            },
            ResourceChange {
                key: ("added".into(), added.type_id),
                before: None,
                after: Some(added),
            },
        ]);

        apply_archive_edit(&mut archive, &edit, true).unwrap();
        assert_eq!(archive.entries.len(), 2);
        let sample = archive
            .entries
            .iter()
            .find(|entry| entry.name == "sample")
            .unwrap();
        assert_eq!(sample.read_prefix(64).unwrap(), b"replacement");

        apply_archive_edit(&mut archive, &edit, false).unwrap();
        assert_eq!(archive.entries.len(), 1);
        assert_eq!(archive.entries[0].read_prefix(64).unwrap(), b"original");
    }

    #[test]
    fn edit_history_is_bounded_and_new_edits_clear_redo() {
        let transaction = |label: String| EditTransaction {
            label,
            edit: ArchiveEdit::Description {
                before: "before".into(),
                after: "after".into(),
            },
            selected_before: BTreeSet::new(),
            selected_after: BTreeSet::new(),
            category_before: None,
            category_after: None,
            dirty_before: false,
            dirty_after: true,
            estimated_bytes: 1,
        };
        let mut history = EditHistory::default();
        for index in 0..MAX_UNDO_STEPS + 5 {
            assert!(history.record(transaction(index.to_string())));
        }
        assert_eq!(history.undo.len(), MAX_UNDO_STEPS);

        let undone = history.undo.pop_back().unwrap();
        history.redo.push_back(undone);
        assert!(history.record(transaction("new".into())));
        assert!(history.redo.is_empty());

        let mut oversized = transaction("oversized".into());
        oversized.estimated_bytes = MAX_UNDO_BYTES + 1;
        assert!(!history.record(oversized));
        assert!(history.undo.is_empty());
    }

    #[test]
    fn description_edit_round_trips() {
        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        let original = archive.description();
        let edit = ArchiveEdit::Description {
            before: original.clone(),
            after: "Changed".into(),
        };
        apply_archive_edit(&mut archive, &edit, true).unwrap();
        assert_eq!(archive.description(), "Changed");
        apply_archive_edit(&mut archive, &edit, false).unwrap();
        assert_eq!(archive.description(), original);
    }

    #[test]
    fn redo_refuses_missing_external_resources_without_mutating_archive() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.txt");
        fs::write(&path, b"temporary").unwrap();
        let archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        let entry = archive.prepare_incoming_file(&path).unwrap();
        let edit = ArchiveEdit::Resources(vec![ResourceChange {
            key: (entry.name.to_ascii_lowercase(), entry.type_id),
            before: None,
            after: Some(entry),
        }]);
        fs::remove_file(path).unwrap();

        let mut archive = archive;
        assert!(apply_archive_edit(&mut archive, &edit, true).is_err());
        assert!(archive.entries.is_empty());
    }

    #[test]
    fn background_compilation_pipeline_exports_and_validates_models() {
        let source_directory = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let source = |name: &str| {
            format!(
                "newmodel {name}\nsetsupermodel {name} NULL\nbeginmodelgeom {name}\nnode trimesh mesh\n parent NULL\n verts 3\n 0 0 0\n 1 0 0\n 0 1 0\n tverts 3\n 0 0 0\n 1 0 0\n 0 1 0\n faces 1\n 0 1 2 1 0 1 2 0\nendnode\nendmodelgeom {name}\ndonemodel {name}\n"
            )
        };
        let mut archive = Archive::new(ArchiveKind::Hak, ArchiveVersion::V1_0);
        for index in 0..130 {
            let name = format!("m{index:03}");
            let path = source_directory.path().join(format!("{name}.mdl"));
            let source = if index == 0 {
                source(&name).replace(
                    &format!("setsupermodel {name} NULL"),
                    &format!("setsupermodel {name} missing_custom_supermodel"),
                )
            } else {
                source(&name)
            };
            fs::write(&path, source).unwrap();
            archive.add_file(&path).unwrap();
        }
        let compiler = bundled_model_compiler().unwrap();
        let (sender, receiver) = mpsc::channel();
        compile_models_worker(
            ModelCompileRequest {
                archive,
                tabs: Vec::new(),
                nwn_installation: None,
                model_indices: (0..130).collect(),
                compiler: compiler.path.clone(),
                single_path: None,
                directory: Some(destination.path().to_path_buf()),
            },
            Arc::new(AtomicBool::new(false)),
            sender,
        );
        let outcome = receiver
            .into_iter()
            .find_map(|event| match event {
                ModelCompileEvent::Finished(outcome) => Some(outcome),
                ModelCompileEvent::Progress { .. } => None,
            })
            .unwrap();

        let report = outcome
            .report_path
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .unwrap_or_default();
        assert!(outcome.fatal_error.is_none(), "{:?}", outcome.fatal_error);
        assert_eq!(outcome.exported, 130, "{report}");
        assert_eq!(outcome.skipped, 0, "{report}");
        assert!(valid_compiled_model(
            &fs::read(destination.path().join("m000.mdl")).unwrap()
        ));
        assert!(valid_compiled_model(
            &fs::read(destination.path().join("m129.mdl")).unwrap()
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn embedded_windows_compiler_creates_a_binary_model() {
        let workspace = tempfile::tempdir().unwrap();
        let input = workspace.path().join("test.mdl.ascii");
        let source = b"newmodel test\nsetsupermodel test NULL\nbeginmodelgeom test\nnode trimesh mesh\n parent NULL\n verts 3\n 0 0 0\n 1 0 0\n 0 1 0\n faces 1\n 0 1 2 1 0 1 2 0\nendnode\nendmodelgeom test\ndonemodel test\n";
        fs::write(&input, source).unwrap();
        let compiler = bundled_model_compiler().unwrap();
        assert_eq!(
            fs::read(&compiler.path).unwrap(),
            include_bytes!("../tools/windows/nwnmdlcomp.exe")
        );
        let output = input.with_extension("");
        let status = Command::new(&compiler.path)
            .arg("--quiet")
            .arg("compile")
            .arg("--force")
            .arg("--output")
            .arg(&output)
            .arg(&input)
            .current_dir(workspace.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(valid_compiled_model(&fs::read(output).unwrap()));
    }
}
