# Aurora Hak Explorer

Aurora Hak Explorer (AHE) is a native, dark-themed, open-source archive editor for Neverwinter Nights. It is intended as a modern Linux replacement for the classic `nwhak.exe` utility.

## Features

- Open and validate ERF V1.0 and V1.1 containers
- Create HAK, ERF, MOD, and SAV archives
- Add individual resources or every supported file in a directory
- Extract selected resources or an entire archive
- Delete resources and merge archives
- Preserve unknown resource type IDs
- Stream large resources instead of loading an entire archive into memory
- Rewrite archives through a temporary file to avoid corrupting the original
- Prompt to save or discard unsaved archives before closing tabs or quitting
- Native dark interface with multiple archive tabs
- Switch between remembered Dark and Normal appearance modes (Dark by default)
- Preview TGA, DDS, PLT, PNG, JPEG, BMP, GIF, TIFF, WebP, and other supported images in the Details pane
- Remembered compact mode that hides the resource tree and details panes
- Search and type-to-select, sortable columns, multi-selection, drag files in or out, and keyboard shortcuts
- Export one or several selected resources from the right-click menu
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

HAK, ERF, MOD, and SAV are variations of BioWare's ERF container. V1.0 uses 16-byte resource names and is used by NWN/NWN:EE. V1.1 uses 32-byte names and is used by NWN2. ERF V1.x uses 32-bit offsets, so an archive cannot exceed 4 GiB.

## License

Author and copyright © 2026 Winternite.

Licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).
