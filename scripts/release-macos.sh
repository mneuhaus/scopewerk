#!/usr/bin/env bash
# Builds the macOS release assets into dist/release/:
#   Scopewerk-<version>-macos-universal.dmg        app (Apple Silicon + Intel, CLI inside), ad-hoc signed
#   scopewerk-cli-<version>-macos-universal.tar.gz the `scopewerk` command line tool and MCP server
#   SHA256SUMS-<version>-macos.txt
# Publishes nothing; upload with `gh release create`. The Windows assets come from CI.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$root/dist/release"
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$root/Cargo.toml")"

rm -rf "$out"
mkdir -p "$out"
UNIVERSAL=1 "$root/scripts/bundle-macos.sh"

echo "packing the disk image"
staging="$(mktemp -d "$root/dist/.dmg.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
cp -R "$root/dist/Scopewerk.app" "$staging/"
ln -s /Applications "$staging/Applications"
# hdiutil's own estimate for -srcfolder can come out too small; size explicitly with headroom.
size_mb=$(($(du -sm "$staging" | cut -f1) + 40))
hdiutil create -quiet -volname "Scopewerk $version" -srcfolder "$staging" -size "${size_mb}m" -ov \
    -format UDZO "$out/Scopewerk-$version-macos-universal.dmg"

echo "packing the command line tool"
cli="$staging/scopewerk-cli-$version-macos-universal"
mkdir -p "$cli"
cp "$root/dist/Scopewerk.app/Contents/MacOS/scopewerk" "$cli/scopewerk"
cp "$root/LICENSE" "$root/README.md" "$cli/"
tar -czf "$out/scopewerk-cli-$version-macos-universal.tar.gz" -C "$staging" "$(basename "$cli")"

(cd "$out" && shasum -a 256 ./*.dmg ./*.tar.gz > "SHA256SUMS-$version-macos.txt")
lipo -info "$root/dist/Scopewerk.app/Contents/MacOS/scopewerk-app"
ls -lh "$out"
