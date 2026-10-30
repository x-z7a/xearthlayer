#!/usr/bin/env bash
# Build and sign XEarthLayer.app, which carries the FSKit module that mounts
# scenery without macFUSE's kernel extension (macOS 26 and later, #210).
#
# Usage: scripts/macos/build-fskit.sh <version> <out-dir>
#
#   version  without the leading v, e.g. 0.5.0 or 0.5.0-rc.1
#   out-dir  where to write XEarthLayer.app (replaced if present)
#
# The module's entitlement, com.apple.developer.fskit.fsmodule, is a managed
# capability: macOS only loads a module signed by a team whose provisioning
# profile grants it. An ad-hoc signed bundle builds and verifies but will
# not mount, so a real identity and profiles are needed to test a mount.
#
# Environment:
#   MACOS_FSKIT_IDENTITY     codesign identity, e.g. "Apple Development: ..."
#                            or "Developer ID Application: ...". Default: ad-hoc.
#   MACOS_FSKIT_APP_PROFILE  provisioning profile embedded in the app.
#   MACOS_FSKIT_EXT_PROFILE  provisioning profile embedded in the module; must
#                            grant the FSKit module capability.
#   MACOS_FSKIT_TEAM_ID      team id prefixed to application identifiers when
#                            signing with profiles.
#   MACOS_FSKIT_BUNDLE_PREFIX  reverse-DNS prefix of the bundle ids: the app is
#                            <prefix>.xearthlayer, the module
#                            <prefix>.xearthlayer.fskit. Must match the App IDs
#                            the profiles were made for. Default: net.defreyssi.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo_root=$(cd "$here/../.." && pwd)
src="$repo_root/macos/fskit"

if [ "$(uname)" != "Darwin" ]; then
    echo "error: the FSKit module builds on macOS only" >&2
    exit 1
fi
if [ "$#" -ne 2 ]; then
    sed -n '2,9p' "$0" >&2
    exit 2
fi
version=$1 out_dir=$2
# CFBundleVersion must be dot-separated integers: drop any pre-release suffix.
bundle_version=${version%%-*}
case "$bundle_version" in
    '' | *[!0-9.]*) echo "error: version $version does not start with x.y.z" >&2; exit 2 ;;
esac

identity=${MACOS_FSKIT_IDENTITY:--}
prefix=${MACOS_FSKIT_BUNDLE_PREFIX:-net.defreyssi}
case "$prefix" in
    '' | *[!A-Za-z0-9.-]*) echo "error: bundle prefix '$prefix' is not reverse-DNS" >&2; exit 2 ;;
esac
app_id="$prefix.xearthlayer"
ext_id="$prefix.xearthlayer.fskit"
sdk=$(xcrun --show-sdk-path)
target=arm64-apple-macos26.0
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

app="$work/XEarthLayer.app"
ext="$app/Contents/Extensions/XEarthLayerFS.appex"
mkdir -p "$app/Contents/MacOS" "$ext/Contents/MacOS"

render_plist() {
    sed -e "s/@VERSION@/$bundle_version/g" -e "s/@BUNDLE_PREFIX@/$prefix/g" "$1" > "$2"
    plutil -lint -s "$2"
}

echo "Compiling the FSKit module"
xcrun swiftc -sdk "$sdk" -target "$target" -swift-version 5 -O \
    -parse-as-library -application-extension -module-name XEarthLayerFS \
    -o "$ext/Contents/MacOS/XEarthLayerFS" "$src"/Extension/Sources/*.swift
render_plist "$src/Extension/Info.plist" "$ext/Contents/Info.plist"

echo "Compiling the host app"
xcrun swiftc -sdk "$sdk" -target "$target" -swift-version 5 -O \
    -module-name XEarthLayer -o "$app/Contents/MacOS/XEarthLayer" "$src/App/main.swift"
render_plist "$src/App/Info.plist" "$app/Contents/Info.plist"

# Entitlements as signed: with a team, application identifiers must carry its
# prefix to match the provisioning profiles.
ext_entitlements="$work/ext.entitlements"
app_entitlements="$work/app.entitlements"
cp "$src/Extension/XEarthLayerFS.entitlements" "$ext_entitlements"
cp "$src/App/XEarthLayer.entitlements" "$app_entitlements"
if [ -n "${MACOS_FSKIT_TEAM_ID:-}" ]; then
    team=$MACOS_FSKIT_TEAM_ID
    /usr/libexec/PlistBuddy -c "Add :com.apple.application-identifier string $team.$ext_id" "$ext_entitlements"
    /usr/libexec/PlistBuddy -c "Add :com.apple.developer.team-identifier string $team" "$ext_entitlements"
    /usr/libexec/PlistBuddy -c "Add :com.apple.application-identifier string $team.$app_id" "$app_entitlements"
    /usr/libexec/PlistBuddy -c "Add :com.apple.developer.team-identifier string $team" "$app_entitlements"
fi
if [ -n "${MACOS_FSKIT_EXT_PROFILE:-}" ]; then
    cp "$MACOS_FSKIT_EXT_PROFILE" "$ext/Contents/embedded.provisionprofile"
fi
if [ -n "${MACOS_FSKIT_APP_PROFILE:-}" ]; then
    cp "$MACOS_FSKIT_APP_PROFILE" "$app/Contents/embedded.provisionprofile"
fi

if [ "$identity" = "-" ]; then
    echo "warning: ad-hoc signing; macOS will not load the module without a team identity and profile" >&2
    timestamp=()
else
    timestamp=(--timestamp)
fi

# Inside out: the module first, then the app that seals it.
codesign --force --sign "$identity" --options runtime ${timestamp[@]+"${timestamp[@]}"} \
    --entitlements "$ext_entitlements" "$ext"
codesign --force --sign "$identity" --options runtime ${timestamp[@]+"${timestamp[@]}"} \
    --entitlements "$app_entitlements" "$app"
codesign --verify --deep --strict "$app"

mkdir -p "$out_dir"
rm -rf "${out_dir:?}/XEarthLayer.app"
mv "$app" "$out_dir/"
echo "Built $out_dir/XEarthLayer.app ($app_id, module $ext_id)"
