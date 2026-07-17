# Changelog

All notable changes to Aurora Hak Explorer are recorded here.

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
