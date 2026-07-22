# Changelog

## 0.1.1 - 2026-07-22

- Qualified 113,161 CEP 3 models, added byte-exact preserved-source restores,
  hardened malformed input handling, and documented six unsafe source rejects.
- Preserved empty skin-weight rows and legacy constraint tables without
  changing visible geometry; literal filenames with glob characters now work.
- Corrected MDL texture, material, flare, emitter, supermodel, and reference
  name validation to use their actual compiled field widths instead of the
  unrelated 16-byte generic ResRef limit.
- Added legacy compatibility for timestamp preambles emitted by damaged old
  exporter headers, filename prefixes accidentally glued to `#MAXMODEL`, and
  face rows whose optional surface value is omitted; strict mode continues to
  reject those forms.
- Added collision-safe aliases for legacy geometry and animation node names
  longer than the compiled MDL's 31-byte node-name limit.
- Compiled and validated all 10,588 ASCII models extracted from
  `cep3_armor.hak`, with zero skipped models.

## 0.1.0 - 2026-07-22

- Added safe Rust ASCII-to-binary and binary-to-ASCII NWN:EE model conversion.
- Added engine-facing BioWare model, animation, node, and mesh routine tables.
- Added legacy `donemodel` normalization without silently accepting unknown
  model statements.
- Added parallel batch compilation, decompilation, conversion, and validation.
- Added KEY/BIF model extraction with EE archive support.
- Added atomic, non-destructive output handling.
- Verified full decompile/recompile/validate round trips over 953 local EE
  compiled models.
