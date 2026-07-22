# Production qualification

This document records the release qualification performed on 2026-07-22.

The complete MDL contents of 15 local `cep3_*.hak` archives were tested:
17,368 ASCII models and 95,793 compiled models (113,161 total inputs).

- 17,362 valid ASCII inputs compiled, decompiled, recompiled, and decompiled
  again successfully.
- All 95,793 existing binaries decompiled successfully; 95,764 completed a
  canonical round trip.
- The remaining 29 legacy binaries contain empty/cyclic root hierarchies.
  Canonical compilation safely rejects those structures, while
  `--preserve-compiled-source` restored all 29 byte-for-byte.
- Six incomplete ASCII sources were deliberately rejected: the four
  `c_horse_pack1.mdl` through `c_horse_pack4.mdl` models have face UV indices
  but no UV table, while `pfe14_pelvis156.mdl` and `pfe17_pelvis156.mdl`
  declare 118 vertices but provide only 117 complete coordinates.

Workspace tests, strict Clippy and formatting checks pass. A deterministic
1,024-case malformed-input mutation test found no panics. Valgrind reported
zero errors and zero definitely lost bytes for compile and decompile, and
`cargo audit` reported no known vulnerabilities.

No NWN:EE game client executable was available in the qualification
environment. In-engine rendering, animation, collision, and interaction remain
release acceptance tests. Keep original sources and HAKs backed up, compile to
a separate directory, validate the output, and test representative creatures,
armor, doors, placeables, tiles, emitters, and walkmeshes in the client build
you ship.
