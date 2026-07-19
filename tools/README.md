# Bundled model compiler

`linux/nwnmdlcomp` is a statically linked 32-bit build of the headless NWNTools
model compiler from the NWN Explorer 1.8.5 source distribution. The original
Aurora model structures require a 32-bit build to preserve their on-disk
layout. Aurora Hak Explorer invokes it only for an explicit **Compile and
Export** action; it does not launch or control the game.

The helper includes small portability fixes for modern 64-bit Linux:

- scan the correct byte while validating ASCII input;
- permit compilation without a game data directory (supermodels can instead
  be staged from the open archive).

The compiler's BSD-style license and copyright notice are reproduced in
`../THIRD_PARTY_NOTICES.md`.

The Windows build embeds `windows/nwnmdlcomp.exe` in Aurora Hak Explorer itself.
It is extracted to a private temporary directory on first use and cached for
the remainder of that AHE session, so the portable Windows release still ships
as one EXE without paying repeated extraction and antivirus-scan costs.

AHE invokes the helper's private batch interface for bounded groups of models.
This reuses one process, workspace, and staged supermodel cache across each
group while retaining per-model validation and failure reporting.
