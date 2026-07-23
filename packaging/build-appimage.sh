#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -1)"
if [[ $# -ge 1 ]]; then
  binary="$1"
else
  binary="$root/target-portable/x86_64-unknown-linux-gnu/release/aurora-hak-explorer"
  "$root/packaging/build-portable-linux-binary.sh" >/dev/null
fi
output="${2:-$root/build/Aurora-Hak-Explorer-${version}-x86_64.AppImage}"
appimagetool="${APPIMAGETOOL:-$HOME/Desktop/Aurora-TLK-Explorer/dist/tools/appimagetool-x86_64.AppImage}"

test -x "$binary"
"$root/packaging/check-linux-portability.sh" "$binary"
test -x "$root/tools/linux/nwnmdlcomp"
"$root/packaging/check-linux-portability.sh" "$root/tools/linux/nwnmdlcomp"
test -x "$appimagetool"
mkdir -p "$root/build"
appdir="$(mktemp -d "$root/build/AppDir-${version}.XXXXXX")"
trap 'find "$appdir" -depth -delete' EXIT

install -Dm755 "$root/packaging/AppRun" "$appdir/AppRun"
install -Dm755 "$binary" "$appdir/usr/bin/aurora-hak-explorer"
install -Dm755 "$root/tools/linux/nwnmdlcomp" \
  "$appdir/usr/libexec/aurora-hak-explorer/nwnmdlcomp"
install -Dm644 "$root/packaging/aurora-hak-explorer.desktop" \
  "$appdir/aurora-hak-explorer.desktop"
install -Dm644 "$root/packaging/aurora-hak-explorer.desktop" \
  "$appdir/usr/share/applications/aurora-hak-explorer.desktop"
install -Dm644 "$root/assets/aurora-hak-explorer.png" \
  "$appdir/aurora-hak-explorer.png"
install -Dm644 "$root/assets/aurora-hak-explorer.png" \
  "$appdir/usr/share/icons/hicolor/512x512/apps/aurora-hak-explorer.png"
install -Dm644 "$root/assets/kwin-active-output.js" \
  "$appdir/usr/share/aurora-hak-explorer/kwin-active-output.js"
ln -s aurora-hak-explorer.png "$appdir/.DirIcon"
install -Dm644 "$root/packaging/aurora-hak-explorer.appdata.xml" \
  "$appdir/usr/share/metainfo/aurora-hak-explorer.appdata.xml"
install -Dm644 "$root/packaging/aurora-hak-explorer-mime.xml" \
  "$appdir/usr/share/mime/packages/aurora-hak-explorer-mime.xml"
for document in README.md CHANGELOG.md LICENSE THIRD_PARTY_NOTICES.md; do
  install -Dm644 "$root/$document" "$appdir/usr/share/doc/aurora-hak-explorer/$document"
done

ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$appimagetool" -n "$appdir" "$output"
chmod +x "$output"
echo "$output"
