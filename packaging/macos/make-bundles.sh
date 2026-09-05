#!/bin/sh
# Builds relocatable .app bundles for the manager and the display agent
# with Homebrew Qt's macdeployqt, plus a flat bin/ of the Rust daemon,
# workers, and CLI, under dist/kwe-macos/. Unsigned (ad-hoc): fine for the
# maintainer's own Mac; distribution needs Developer ID signing and
# notarization (plan MP-8, pending).
#
#   packaging/macos/make-bundles.sh [--no-build]
set -eu
here="$(cd "$(dirname "$0")" && pwd -P)"
root="$(cd "$here/../.." && pwd -P)"
case "$(uname -s)" in Darwin) ;; *) echo "macOS only" >&2; exit 2;; esac
qt_prefix="$(brew --prefix qt@6 2>/dev/null || brew --prefix qt)"
macdeployqt="$qt_prefix/bin/macdeployqt"
[ -x "$macdeployqt" ] || { echo "macdeployqt not found under $qt_prefix/bin" >&2; exit 1; }

if [ "${1:-}" != "--no-build" ]; then
  (cd "$root" && cargo build --workspace --release)
  (cd "$root" && cmake -S . -B build/agent -G Ninja -DCMAKE_BUILD_TYPE=Release \
     -DCMAKE_PREFIX_PATH="$qt_prefix" -DBUILD_TESTING=OFF && cmake --build build/agent --parallel)
fi

dist="$root/dist/kwe-macos"
rm -rf "$dist"; mkdir -p "$dist/bin"
for bin in kwe-daemon kwe kwe-test-renderer kwe-video-renderer kwe-web-renderer kwe-scene-renderer \
           kwe-audio-worker kwe-vulkan kwe-scene-inspector kwe-shader-compiler; do
  [ -x "$root/target/release/$bin" ] && cp "$root/target/release/$bin" "$dist/bin/"
done

# Wrap a plain executable into a minimal .app, then let macdeployqt pull in
# Qt frameworks, plugins, and the QML modules the executable imports.
make_app() {
  exe="$1"; name="$2"; bundle_id="$3"; ui_element="$4"
  app="$dist/$name.app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cp "$exe" "$app/Contents/MacOS/$name"
  cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundleDisplayName</key><string>$name</string>
  <key>CFBundleIdentifier</key><string>$bundle_id</string>
  <key>CFBundleExecutable</key><string>$name</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSUIElement</key><$ui_element/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
  # QML sources for macdeployqt's import scan; -always-overwrite keeps
  # re-runs idempotent; ad-hoc signature so Gatekeeper accepts the local
  # copy (Developer ID + notarization is the distribution follow-up).
  "$macdeployqt" "$app" -qmldir="$root/apps" -qmlimport="$root/build/agent/qml" -always-overwrite -verbose=1 >/dev/null
  codesign --force --deep --sign - "$app" >/dev/null 2>&1 || true
  echo "bundled $app"
}
make_app "$root/build/agent/apps/kwe-manager/kwe-manager" "KWE Manager" "org.kde.kwe.manager" false
make_app "$root/build/agent/apps/kwe-display-macos/kwe-display-macos" "KWE Display" "org.kde.kwe.display" true
# The manager looks for the agent beside itself.
ln -sfn "../../KWE Display.app/Contents/MacOS/KWE Display" "$dist/KWE Manager.app/Contents/MacOS/kwe-display-macos"
echo "dist: $dist (bin/ + two .app bundles). LaunchAgents: point install-dev.sh's plists at $dist/bin if you move it."
