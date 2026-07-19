# Aurora Hak Explorer

Aurora Hak Explorer (AHE) is a native, dark-themed, open-source archive and model utility for Neverwinter Nights on Linux and Windows. It is intended as a modern replacement for the classic `nwhak.exe` workflow.

See [CHANGELOG.md](CHANGELOG.md) for release history.

## Features

- Open and validate ERF V1.0/V1.1 containers and read-only BIFF V1 archives
- Create HAK, ERF, MOD, and SAV archives
- Add individual resources or every supported file in a directory
- Extract selected resources or an entire archive
- Delete resources and merge archives
- Preserve unknown resource type IDs
- Stream large resources instead of loading an entire archive into memory
- Rewrite archives through a temporary file to avoid corrupting the original
- Prompt to save or discard unsaved archives before closing tabs or quitting
- Native dark interface with multiple archive tabs
- Switch between remembered System, Dark, and Light appearance modes (Dark by default)
- Preview TGA, DDS, PLT, PNG, JPEG, BMP, GIF, TIFF, WebP, and other supported images in the Details pane
- Preview MTR material files as text and inspect BMU/MP3 audio properties in the Details pane
- Render compiled and uncompiled MDL geometry with archive textures, inspect model metadata, and extract readable strings
- Compile ASCII MDLs or decompile binary MDLs without launching Neverwinter Nights
- Filter all, compiled, or uncompiled models directly from the Resource Tree
- Reopen archives quickly from the remembered recent-files list
- Automatically find Steam, GOG, and Beamdog NWN installations, or remember a manually selected installation directory
- Remembered compact mode that hides the resource tree and details panes
- Search and type-to-select, sortable columns, multi-selection, drag files in or out, and keyboard shortcuts
- Export one or several selected resources from the right-click menu
- Export selected resources with Ctrl+E
- Copy, cut, and paste resources through the Linux file clipboard with Ctrl+C/Ctrl+X/Ctrl+V
- Confirm resource conflicts individually or choose Replace All/Skip All during multi-file imports
- Select every visible resource with Ctrl+A
- Navigate with Up/Down, extend ranges with Shift+Up/Down, or add marked items with Ctrl+Up/Down

## Build

Install a current stable Rust toolchain, then run:

```bash
cargo build --release
```

The binary will be at `target/release/aurora-hak-explorer`.

## Format scope

HAK, ERF, MOD, and SAV are variations of BioWare's ERF container. V1.0 uses 16-byte resource names and is used by NWN/NWN:EE. V1.1 uses 32-byte names and is used by NWN2. ERF V1.x uses 32-bit offsets, so an archive cannot exceed 4 GiB. BioWare BIFF V1 (`.bif`) archives can be browsed, previewed, and extracted; because standalone BIF files contain no resource names, entries use NWN Explorer-compatible names such as `res107.mdl`. BIF archives are opened read-only.

## License

Author and copyright © 2026 Winternite.

Licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).
