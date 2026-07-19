# AHE Windows-only dependency patch

Aurora Hak Explorer uses `drag` 2.1.1 only on Windows. This local package omits
the crate's Linux/BSD and macOS dependency declarations so Cargo does not lock
unused GTK 3 and platform crates for AHE's supported Linux and Windows builds.

The Windows implementation also wraps `ILCreateFromPathW` results so every PIDL
is released with `ILFree`, balances each successful `OleInitialize` with
`OleUninitialize`, and propagates Shell allocation errors instead of panicking.
Already-absolute drag paths bypass redundant canonicalization. Selections of at
least 1,024 files use a direct `CF_HDROP` data object instead of allocating one
Shell PIDL per file, which keeps very large Explorer drags practical.
