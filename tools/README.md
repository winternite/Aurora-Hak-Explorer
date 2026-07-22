# Bundled model compiler

`linux/nwnmdlcomp` and `windows/nwnmdlcomp.exe` are release builds of the
memory-safe Rust compiler in `aurora-nwnmdlcomp/` (currently version 0.1.1). It converts authored ASCII
MDL files to the explicit 32-bit little-endian NWN:EE binary format without
mapping model data onto native Rust structures.

AHE invokes its parallel multi-file CLI only for an explicit **Compile and
Export** action. AHE retains control of dependency staging, cancellation,
compiled-output validation, and failure reporting; it does not launch or
control Neverwinter Nights.

The Linux helper is built for x86-64 with a glibc 2.17 compatibility ceiling.
The Windows helper is x86-64 and embedded directly in Aurora Hak Explorer. It
is extracted to a private temporary directory on first use and cached for the
remainder of that AHE session, so the portable release remains one EXE.

See `aurora-nwnmdlcomp/README.md` for compiler build and verification commands.
Its GPL and third-party notices are included with the source and summarized in
`../THIRD_PARTY_NOTICES.md`.
