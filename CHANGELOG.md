# Changelog

All notable changes to Aurora Hak Explorer are recorded here.

## 1.2.2 — 2026-07-19

- Improve very large drag-and-drop operations: imports avoid duplicate file
  metadata work and use adaptive UI batches, outgoing staging uses bounded
  parallelism, Linux KDE drops extract lazily from the archive, and large
  Windows drags avoid per-file Shell PIDL setup.
- Ensure drag, clipboard, and model-workspace temporary directories are removed
  after Aurora exits by handing their cleanup deadlines to a durable helper.
- Remove completed drag staging directories after a fixed one-minute grace
  period on both Linux and Windows.
- Recover abandoned `ahe-drag-*` directories at startup after crashes or system
  shutdowns, deleting those at least 15 minutes old immediately and scheduling
  newer leftovers for their remaining safe-retention time.
- Open BioWare BIFF V1 archives (`.bif`) for browsing, model and resource previews, drag/copy export, and extraction.
- Match NWN Explorer's standalone-BIF naming convention (`resN.ext`) and keep BIF archives read-only to protect installed game data.
- Automatically locate Steam, GOG, and Beamdog NWN installations, with a remembered manual installation-directory option under the Tools menu.
- Bound and clean outgoing-drag staging on both platforms, release Windows Shell allocations correctly, persist installation-path clearing, and honor the manually selected NWN installation when duplicate game resources exist.

## 1.2.1 — 2026-07-19

- Compile models in bounded batches with a shared workspace and dependency cache, greatly reducing helper-process and temporary-file overhead.
- Keep AHE responsive during model compilation with progress reporting and cancellation.
- Show live elapsed time and an adaptive estimated time remaining while models compile.
- Cache the embedded Windows model compiler for the application session and suppress its transient console windows.
- Reliably drag selections containing tens or hundreds of thousands of resources into KDE file managers using KIO's archive-extraction protocol.

## 1.2.0 — 2026-07-19

- Keep resource browsing responsive in very large archives by caching metadata, filtering, and sorting while drawing only visible rows.
- Resolve model supermodel chains from open and sibling HAKs, override and development folders, and installed NWN KEY/BIF data so skinmeshes can be compiled without launching the game.
- Support dotted extension searches such as `.nss`, `.dds`, and `.tga`.
- Correct distant keyboard and type-ahead jumps in the virtualized resource list.
- Resolve Enhanced Edition MTR diffuse textures across sibling HAKs and preserve valid model geometry in textured previews.
- Build Linux releases against glibc 2.17 and reject non-portable binaries during AppImage packaging.

## 1.1.0 — 2026-07-18

- Render compiled and uncompiled Aurora MDL geometry in the Details pane, with PLT, TGA, and DDS textures resolved from the open archive and nearby HAKs.
- Compile ASCII models and decompile binary models without launching Neverwinter Nights, including multi-selection export actions.
- Add Resource Tree filters for all, compiled, and uncompiled models.
- Export any selected resources with Ctrl+E.
- Keep sibling texture discovery responsive by indexing it in the background.
- Harden model parsing against malformed hierarchies, excessive element counts, and unsafe allocations.
- Embed the model compiler in the Windows application so the portable release remains a single executable.

## 1.0.0 — 2026-07-17

- Close archive tabs by middle-clicking them.
- Scroll the resource list by holding the middle mouse button and dragging.
- Add a remembered eight-item recent-archives list to the welcome screen.
- Support opening multiple archives at once from the Open file picker.
- Refine the welcome-screen archive actions and recent-file controls.
- Add a remembered System appearance that follows the desktop theme.
- Display encoding, channel, sample-rate, bitrate, bit-depth, and duration details for RIFF/WAVE resources and MP3 payloads stored under the WAV resource type.
- Distinguish compiled and uncompiled MDL resources, showing ASCII source for uncompiled models and summary plus extracted-string views for compiled models.

## 0.3.0 — 2026-07-16

- Refresh the archive tabs and Resource Tree with consistent selection and hover styling.
- Add a right-click `Close tab` action while retaining unsaved-change protection.
- Move the `Open` action into the document bar beside the new-archive button.
- Redesign and center the About panel with clearer cross-platform information.
- Center the action buttons in the unsaved-changes dialog.
- Improve compatibility with archives created by older ERF command-line tools.
- Center and enlarge error dialogs for improved readability.
- Harden malformed-archive handling against excessive localized-string allocations.
- Keep imports tied to their originating tab and preserve selections when resources are re-sorted.
- Prevent background keyboard shortcuts and file drops from acting through open dialogs.
- Bound image-preview dimensions and memory use for standard, DDS, and PLT images.
- Reduce X11 clipboard-key polling overhead and promptly clean up superseded clipboard exports.
- Correct ERF build dates across leap years.
- Reject resource names that could escape extraction, drag, or clipboard directories while retaining compatibility with real-world community archives.
- Bound archive key and resource table allocations before parsing untrusted files.
- Route directory imports through the same per-file and replace-all overwrite confirmation used by other imports.
- Avoid cloning large active archives every UI frame.
- Make the new-archive and description editors modal so background actions cannot change their target.

## 0.2.3 — 2026-07-16

- Display MTR material resources as selectable text in the Details pane.
- Recognize BMU music resources and display their MP3 encoding, bitrate, sample rate, channels, and approximate duration.

## 0.2.2 — 2026-07-16

- Add remembered Dark and Normal appearance modes, with Dark mode as the default.
- Add previews for ATI2/BC5-compressed DDS normal maps.
- Add PLT previews with representative colors for all ten material layers.
- Categorize PLT resources as textures.
- Add previews for legacy NWN DDS, TGA, PNG, JPEG, and other common image formats.

## 0.2.1 — 2026-07-15

- Make Compact mode display every resource regardless of the previously selected tree category.
- Add clearer right-click deletion for selected resources.

## 0.1.0 — 2026-07-15

- Initial open-source release for Linux and Windows.
- Open, create, edit, save, and validate HAK, ERF, MOD, and SAV archives.
- Add tabbed archives, search, sorting, multi-selection, keyboard navigation, drag-and-drop, clipboard operations, importing, exporting, and overwrite confirmation.
