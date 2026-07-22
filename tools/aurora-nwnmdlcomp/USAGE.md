# Using Aurora nwnmdlcomp

Aurora `nwnmdlcomp` converts Neverwinter Nights and Neverwinter Nights:
Enhanced Edition (NWN:EE) MDL files between authored ASCII and the game's
compiled 32-bit MDL format. The compiler is a modern 64-bit Rust executable,
but it intentionally writes the original 32-bit little-endian format required
by NWN:EE.

The release executable is:

```text
target/release/nwnmdlcomp
```

On Linux, either run it from the project directory as shown below or add its
directory to `PATH`. Aurora Hak Explorer (AHE) bundles the same compiler for
its **Compile and Export MDL** action.

## Quick start

Compile an ASCII model:

```sh
target/release/nwnmdlcomp compile my_model.mdl.ascii
```

This writes `my_model.mdl`. To choose the precise output name:

```sh
target/release/nwnmdlcomp compile \
  --output my_model.mdl \
  source/my_model.mdl.ascii
```

Decompile a game-ready model back to readable ASCII:

```sh
target/release/nwnmdlcomp decompile my_model.mdl
```

This writes `my_model.mdl.ascii` beside the input.

## Commands

### `compile`

Converts ASCII MDL source into an NWN:EE compiled MDL.

```sh
nwnmdlcomp compile [OPTIONS] INPUT...
```

Useful options:

- `-o, --output FILE` — exact output filename; use with one input only.
- `-D, --output-dir DIR` — put all generated files in `DIR`.
- `-f, --force` — replace existing output files.
- `--strict` — reject legacy shorthand instead of normalizing it.
- `--jobs COUNT` — set the number of parallel workers.
- `-q, --quiet` — suppress successful per-model messages.

Examples:

```sh
# Compile one source model into a release directory.
nwnmdlcomp compile -D release source/robe.mdl.ascii

# Compile a quoted glob. nwnmdlcomp expands it on every platform.
nwnmdlcomp --jobs 8 compile -D release 'source/*.mdl.ascii'

# Compile every ASCII model in a directory and overwrite earlier results.
nwnmdlcomp --quiet compile --force -D compiled 'models/*.mdl'
```

If the source already has a `.mdl` extension, its default compiled name is
`name.compiled.mdl`; this prevents an authored ASCII source from being
overwritten accidentally.

### `decompile`

Converts a compiled NWN/NWN:EE MDL into canonical ASCII MDL.

```sh
nwnmdlcomp decompile [OPTIONS] INPUT...
```

Examples:

```sh
nwnmdlcomp decompile -D ascii 'compiled/*.mdl'
nwnmdlcomp decompile --output sword.mdl.ascii sword.mdl
```

Use `--preserve-compiled-source` only when you need an embedded copy of the
original binary for archival/byte-exact restoration workflows. Normal editing
and recompilation do not need it.

### `convert`

Detects each input's encoding automatically and converts it to the other
format.

```sh
nwnmdlcomp convert -D converted 'models/*.mdl'
```

ASCII inputs become compiled MDLs; compiled inputs become `.ascii` files.

### `validate`

Reads and deeply validates models without writing anything. This is the best
post-build check before packaging a HAK.

```sh
nwnmdlcomp validate release/my_model.mdl
nwnmdlcomp --quiet validate 'release/*.mdl'
```

Validation accepts either ASCII or compiled input. ASCII validation includes a
trial compilation; compiled validation checks binary structure and semantic
lowering.

### `extract`

Extracts model resources from an NWN/NWN:EE client `KEY`/`BIF` installation.

```sh
nwnmdlcomp extract \
  --key '/path/to/Neverwinter Nights/data/nwn_base.key' \
  --output-dir extracted \
  'c_*'
```

Add `--decompile` to write extracted compiled models as ASCII. The final
pattern is case-insensitive and may be supplied with or without `.mdl`.

This command reads `KEY`/`BIF` archives, not HAK files. Use AHE to browse,
extract, or compile models from a HAK.

## Compatibility mode and strict mode

`compile` defaults to compatibility mode so it can handle common old NWMax and
custom-content quirks. It safely normalizes:

- a nameless `donemodel` terminator;
- a recognizable exporter timestamp preamble before the MDL header;
- a filename prefix accidentally glued to the non-semantic `#MAXMODEL`
  comment marker;
- a face row with seven values, where the historical optional surface/material
  value is missing (it defaults to `0`, matching the legacy compiler).
- node names longer than the 31-byte compiled MDL limit. They receive stable,
  unique aliases and all matching hierarchy, skin, and animation references
  are updated together.

Use strict mode for source-quality enforcement:

```sh
nwnmdlcomp compile --strict source/my_model.mdl.ascii
```

Strict mode rejects those shorthand forms rather than changing them. Neither
mode invents missing geometry, textures, animations, or supermodel data.

## Supermodels and AHE

A compiled MDL stores its `setsupermodel` name as a reference; it does not
embed the supermodel's contents. The Rust compiler can therefore compile a
model's own geometry when that dependency is unavailable. At game runtime, the
referenced supermodel must still be available if the model relies on inherited
animation or geometry.

AHE stages available supermodels automatically. If a custom model has a
dangling supermodel reference, current AHE releases compile it instead of
skipping it, while preserving the reference in the compiled output.

## Output safety and batch behavior

Generated files are written atomically in the target directory. Existing files
are never replaced unless `--force` is provided. Batch operations use all
available CPU cores by default, so large source sets compile in parallel.

For a repeatable release workflow:

```sh
nwnmdlcomp --quiet compile --force -D build/models 'source/*.mdl.ascii'
nwnmdlcomp --quiet validate 'build/models/*.mdl'
```

Keep authored ASCII files under version control. Treat compiled MDLs as build
artifacts and test final content in the exact NWN:EE client version you plan to
ship against.

## Troubleshooting

`input is already a compiled binary MDL`

: Use `decompile`, or use `convert` if you want automatic direction detection.

`input is already an ASCII MDL`

: Use `compile`, or use `convert`.

`output already exists`

: Choose another output path or add `--force` after confirming the target is
  safe to replace.

`compiled MDL permits at most ...`

: A fixed-width field is too long for the on-disk MDL layout. Texture,
  material, supermodel, reference, flare, and emitter texture names use their
  actual MDL field widths (normally up to 63 bytes); generic NWN resource names
  are not incorrectly restricted to 16 bytes.

`zero-length` or client-data extraction errors

: The selected installation likely contains dedicated-server placeholders.
  Point `extract --key` at a full NWN/NWN:EE client installation.

For all flags and current command syntax:

```sh
nwnmdlcomp --help
nwnmdlcomp compile --help
nwnmdlcomp extract --help
```
