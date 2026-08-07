# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/jolt-<version>-linux-<arch>.tar.gz` containing:

- `jolt` — the binary (headed by default; `jolt headless` runs the engine alone)
- `jolt.desktop` — XDG desktop entry
- `jolt-512.png` and `jolt.png` — 512×512 and 1024×1024 app icons (vector source `jolt.svg`)
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/jolt-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Jolt.app` (Info.plist + icns), ad-hoc
signs it (set `CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a
dmg. CI runs this on tags (`.github/workflows/release.yml`). The manual steps
it automates, for reference (run on a macOS host — gpui needs Metal; no
cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p jolt --target aarch64-apple-darwin
   cargo build --release -p jolt --target x86_64-apple-darwin
   lipo -create -output Jolt \
     target/aarch64-apple-darwin/release/Jolt \
     target/x86_64-apple-darwin/release/Jolt
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Jolt.app/Contents/{MacOS,Resources}
   cp Jolt Jolt.app/Contents/MacOS/Jolt
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Jolt.app/Contents/Info.plist
   ```
3. Icon: generate `jolt.icns` from `dist/jolt.png` (`iconutil`) and place it at
   `Jolt.app/Contents/Resources/jolt.icns`:
   ```sh
   mkdir jolt.iconset && sips -z 256 256 dist/jolt.png --out jolt.iconset/icon_256x256.png
   iconutil -c icns jolt.iconset -o Jolt.app/Contents/Resources/jolt.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Jolt.app
   xcrun notarytool submit Jolt.zip --keychain-profile … --wait
   xcrun stapler staple Jolt.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Jolt -srcfolder Jolt.app -ov -format UDZO Jolt.dmg`).
