#!/usr/bin/env bash
# Builds a release Scopewerk.app into dist/ (bundle id com.neuhaus.scopewerk, generated icon,
# ad-hoc signed). The app carries the `scopewerk` CLI / MCP server next to it:
#   dist/Scopewerk.app/Contents/MacOS/scopewerk mcp
# Installs nothing; pass --install to copy the app to /Applications. UNIVERSAL=1 builds Apple
# Silicon + Intel in one bundle.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$root/target}"
dist="$root/dist"
app="$dist/Scopewerk.app"
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$root/Cargo.toml")"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
if [[ -n "${UNIVERSAL:-}" ]]; then
    for triple in aarch64-apple-darwin x86_64-apple-darwin; do
        echo "building scopewerk $version (release, $triple)"
        CARGO_TARGET_DIR="$target_dir" cargo build --release -p scopewerk-app -p scopewerk-cli \
            --target "$triple" --manifest-path "$root/Cargo.toml"
    done
    for bin in scopewerk-app scopewerk; do
        lipo -create -output "$app/Contents/MacOS/$bin" \
            "$target_dir/aarch64-apple-darwin/release/$bin" "$target_dir/x86_64-apple-darwin/release/$bin"
    done
else
    echo "building scopewerk $version (release)"
    CARGO_TARGET_DIR="$target_dir" cargo build --release -p scopewerk-app -p scopewerk-cli \
        --manifest-path "$root/Cargo.toml"
    cp "$target_dir/release/scopewerk-app" "$app/Contents/MacOS/scopewerk-app"
    cp "$target_dir/release/scopewerk" "$app/Contents/MacOS/scopewerk"
fi

echo "drawing the icon"
work="$(mktemp -d "$dist/.icon.XXXXXX")"
trap 'rm -rf "$work"' EXIT
swift "$root/scripts/make-icon.swift" "$work/Scopewerk.iconset"
iconutil -c icns "$work/Scopewerk.iconset" -o "$app/Contents/Resources/Scopewerk.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Scopewerk</string>
    <key>CFBundleDisplayName</key>
    <string>Scopewerk</string>
    <key>CFBundleIdentifier</key>
    <string>com.neuhaus.scopewerk</string>
    <key>CFBundleExecutable</key>
    <string>scopewerk-app</string>
    <key>CFBundleIconFile</key>
    <string>Scopewerk</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>MIT License</string>
</dict>
</plist>
PLIST

printf 'APPL????' > "$app/Contents/PkgInfo"
codesign --force --sign - "$app/Contents/MacOS/scopewerk" >/dev/null
codesign --force --sign - "$app" >/dev/null
echo "bundled $app"

if [[ "${1:-}" == "--install" ]]; then
    rm -rf /Applications/Scopewerk.app
    cp -R "$app" /Applications/Scopewerk.app
    echo "installed /Applications/Scopewerk.app"
fi
