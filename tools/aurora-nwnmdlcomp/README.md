# Aurora nwnmdlcomp

Created and led by **Winternite** as part of the Aurora toolset.

`nwnmdlcomp` is a modern, memory-safe Rust compiler and decompiler for
Neverwinter Nights: Enhanced Edition model files. It converts both directions
between authored ASCII MDL and the 32-bit little-endian compiled MDL layout,
validates files, processes batches in parallel, and extracts model resources
from KEY/BIF archives.

It vendors the all-Rust `nwnrs-types` model engine from a pinned Git revision,
with audited MDL compatibility corrections, and adds the standalone CLI,
legacy source normalization, atomic output handling, parallel batches, and
engine-facing BioWare routine-table finalization.

## Compatibility status

The implementation supports the normal NWN and NWN:EE model node families:
dummy, trimesh, skin, animmesh, danglymesh, AABB, light, emitter, reference,
and camera. The semantic compiler handles animations/controllers, multiple UV
layers, authored normals and tangents, vertex colors, materials/MTR references,
skin weights, walkmesh trees, and EE mesh limits.

Local verification performed for this project:

- 953 NWN:EE compiled robe models parsed and semantically validated;
- all 953 decompiled to ASCII;
- all 953 ASCII models recompiled to binary;
- all 953 rebuilt binaries parsed and semantically validated;
- a 1.1 MB legacy NWMax sword model compiled successfully, including its
  nameless `donemodel` terminator;
- an EE model containing 664 authored normals and vertex colors decompiled and
  recompiled successfully;
- generated model, node, and mesh routine tables match shipped EE binaries;
- all 10,588 ASCII models in `cep3_armor.hak` compiled and all generated
  binaries passed semantic validation, with no skipped models.

Those checks are strong format-level coverage, but they are not a claim that
every possible custom-content model has been rendered in every game build.
Keep source models under version control and test release assets in the exact
NWN:EE client build you ship against.

## Build

The project pins its Rust toolchain because the pinned model dependency
currently uses a nightly workspace:

```sh
cargo build --release
```

The resulting executable is `target/release/nwnmdlcomp`.

Run the complete local quality suite with:

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Usage

For a complete command reference and practical release workflow, see
[USAGE.md](USAGE.md).

Compile one legacy or modern ASCII model:

```sh
nwnmdlcomp compile model.mdl.ascii
nwnmdlcomp compile -o model.mdl model.mdl.ascii
```

Decompile a compiled model:

```sh
nwnmdlcomp decompile model.mdl
nwnmdlcomp decompile -o model.mdl.ascii model.mdl
```

Convert automatically based on input encoding:

```sh
nwnmdlcomp convert model.mdl
```

Validate one or many models:

```sh
nwnmdlcomp validate model.mdl
nwnmdlcomp --quiet validate 'models/*.mdl'
```

Batch operations accept paths expanded by the shell or a quoted glob. They run
in parallel and use all available CPUs unless `--jobs COUNT` is supplied:

```sh
nwnmdlcomp decompile -D ascii 'binary/*.mdl'
nwnmdlcomp compile -D rebuilt 'ascii/*.ascii'
```

Extract model resources from a client installation:

```sh
nwnmdlcomp extract \
  --key /path/to/Neverwinter\ Nights/data/nwn_base.key \
  --output-dir extracted \
  --decompile \
  'c_*'
```

Dedicated-server packages commonly contain zero-length placeholders for
client-only model resources. Extraction detects these and reports a clear
error; use a full client KEY/BIF installation for model extraction.

Run `nwnmdlcomp help` or `nwnmdlcomp help COMMAND` for all options.

Default compilation enables legacy compatibility. It accepts nameless
`donemodel` terminators, recognizable timestamp preambles, and the historical
optional eighth face value (defaulting a missing surface/material value to
zero). Use `compile --strict` to reject these shorthand forms.

## Output safety

Generated files are written to a temporary file in the destination directory,
synced, and atomically renamed. Existing files are never replaced unless
`--force` is given. When compiling an ASCII file already named `model.mdl`, the
default output is `model.compiled.mdl`, preventing accidental source loss.

## Library API

The package also exposes a small Rust API:

```rust
use aurora_nwnmdlcomp::{CompileOptions, compile_bytes};

let binary = compile_bytes(
    ascii_source,
    CompileOptions { legacy_compatibility: true },
)?;
# Ok::<(), anyhow::Error>(())
```

See `src/lib.rs` for compile, decompile, convert, and validation entry points.

## Licensing

This project is GPL-3.0-only because it links to `nwnrs-types`. See
`THIRD_PARTY_NOTICES.md` for pinned dependency details.
