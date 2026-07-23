#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -1)"
target="x86_64-pc-windows-msvc"
binary="$root/target/$target/release/aurora-hak-explorer.exe"
output="${1:-$root/build/Aurora-Hak-Explorer-${version}-Windows-x86_64.zip}"

if ! command -v cargo-xwin >/dev/null 2>&1; then
  echo "cargo-xwin is required to build the Windows executable" >&2
  exit 1
fi

if command -v llvm-rc >/dev/null 2>&1; then
  rc_dir="$(dirname "$(command -v llvm-rc)")"
elif [[ -x /usr/lib64/rocm/llvm/bin/llvm-rc ]]; then
  rc_dir=/usr/lib64/rocm/llvm/bin
else
  echo "llvm-rc is required to embed the Windows application icon" >&2
  exit 1
fi

PATH="$rc_dir:$PATH" \
  cargo xwin build --manifest-path "$root/Cargo.toml" \
    --locked --release --target "$target"

test -f "$binary"
mkdir -p "$root/build"
package="$(mktemp -d "$root/build/Windows-${version}.XXXXXX")"
trap 'find "$package" -depth -delete' EXIT

install -Dm755 "$binary" "$package/Aurora-Hak-Explorer.exe"
install -Dm644 "$root/CHANGELOG.md" "$package/CHANGELOG.md"

rm -f "$output"
(cd "$package" && zip -q -9 -r "$output" .)
echo "$output"
