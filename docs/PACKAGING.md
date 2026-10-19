# Packaging & distribution

Concrete build and packaging notes for TeamTrim. The assets referenced here live
in [`packaging/`](../packaging/); the honest state of what is actually verified
is in [`STATUS.md`](STATUS.md).

> **Scope.** Sections 1–5 are *instructions* for building and packaging by hand;
> the platform bundles below (AppImage, Flatpak, `.app`, MSI) cannot be built or
> verified in this headless, credential-free environment (B-001/B-002). A
> contributor with the toolchain and a display runs them. What CI **does** now
> produce automatically is described in [section 6](#6-ci-release-artifacts).

## 0. The binary

One binary, `teamtrim`, from `crates/teamtrim-app`:

```sh
cargo build --release            # -> target/release/teamtrim (teamtrim.exe on Windows)
cargo run   -p teamtrim-app        # debug run (needs a display)
```

### Release profile

The workspace sets (root `Cargo.toml`):

```toml
[profile.release]
lto = "thin"          # cross-crate inlining, much cheaper than "fat"
codegen-units = 1     # best codegen at the cost of build time
strip = "symbols"     # smaller binary; tests use the dev profile
```

`strip = "symbols"` only affects the release profile, so `cargo test` (dev
profile) is unaffected. If you need symbolized release backtraces, change it to
`strip = "debuginfo"`. Nothing in the default build is device- or
service-dependent: the `audio`, `video-capture`, `keyring` and
`desktop-notifications` features are all **default-off**.

## 1. Runtime prerequisites

| Platform | Needs |
|---|---|
| Linux | OpenGL 2.1+ (GLX or EGL), an X11 **or** Wayland session, and the OpenSSL runtime (`libssl3` on Debian/Ubuntu) because `reqwest` uses `native-tls`. No GPU is required — egui/glow falls back to software GL. |
| macOS | macOS 11+ (Apple Silicon or Intel). OpenGL is deprecated but present; no extra runtime. |
| Windows | Windows 10+ with the MSVC runtime (`vcruntime140.dll`; bundle it or link `+crt-static`). |

The GUI cannot start without a display server; in a headless session run the
core/CLI instead (see the README troubleshooting note).

## 2. Linux

### 2.1 Build dependencies (Debian/Ubuntu)

```sh
sudo apt-get install -y \
  pkg-config libssl-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libx11-dev libxi-dev libxcursor-dev libxrandr-dev libxinerama-dev \
  libgl1-mesa-dev
```

(`libasound2-dev` is only needed with the `audio` feature; SDL2 only with
`video-capture`. Both are default-off.)

### 2.2 Install

```sh
install -Dm755 target/release/teamtrim "$HOME/.local/bin/teamtrim"
install -Dm644 packaging/dev.teamtrim.teamtrim.desktop \
  "$HOME/.local/share/applications/dev.teamtrim.teamtrim.desktop"
for size in 16 24 32 48 64 128 256 512 1024; do
  install -Dm644 "packaging/icons/dev.teamtrim.teamtrim-$size.png" \
    "$HOME/.local/share/icons/hicolor/${size}x${size}/apps/dev.teamtrim.teamtrim.png"
done
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
```

`StartupWMClass=dev.teamtrim.teamtrim` in the `.desktop` file matches
`ViewportBuilder::with_app_id("dev.teamtrim.teamtrim")` in
`crates/teamtrim-app/src/main.rs`, so the compositor finds the icon and groups the
window; `Icon=dev.teamtrim.teamtrim` resolves to the installed `hicolor` PNGs.
The binary also embeds the 256 px icon for the window/taskbar, so an uninstalled
`cargo run` is not icon-less. For a system-wide install use `/usr/local/...` and
`/usr/share/applications` instead of the `$HOME/.local` paths.

### 2.3 AppImage (sketch)

No AppImage recipe is committed. The usual route:

1. `cargo build --release`.
2. Build an AppDir containing `usr/bin/teamtrim`, the `.desktop` file and the
   icon PNGs.
3. Run [`linuxdeploy`](https://github.com/linuxdeploy/linuxdeploy) over it.

AppImages must bundle or rely on `libssl`, `libxkbcommon`, the Wayland/X11
client libs and the GL loader; `linuxdeploy` bundles most of them, but OpenSSL
licensing and the GL driver are host concerns. Treat the produced image as
needing testing on each target distro.

### 2.4 Flatpak (sketch)

A manifest would use the freedesktop runtime plus the Rust SDK extension:

- Runtime: `org.freedesktop.Platform` / `org.freedesktop.Sdk` `24.08`.
- Extension: `org.freedesktop.Sdk.Extension.rust-stable`.
- `build-commands`: `cargo build --release --offline` (vendored sources) or with
  the network available to the sandbox at build time, then install the binary,
  `.desktop` file and icon into `/app`.
- File dialogs must go through the `org.freedesktop.portal.FileChooser` portal
  once the attachment picker lands (B-012).

The freedesktop runtime already provides OpenSSL, xkbcommon, Wayland/X11 and
Mesa, so the manifest is mostly build plumbing.

## 3. macOS

Build for one architecture, or both then `lipo`:

```sh
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
lipo -create \
  target/aarch64-apple-darwin/release/teamtrim \
  target/x86_64-apple-darwin/release/teamtrim \
  -output target/release/teamtrim
```

`.app` bundle layout:

```text
TeamTrim.app/
└── Contents/
    ├── Info.plist
    ├── MacOS/
    │   └── teamtrim                 # the lipo'd binary, chmod +x
    └── Resources/
        └── dev.teamtrim.teamtrim.icns # committed icon, named by CFBundleIconFile
```

`Info.plist` notes (minimum keys):

| Key | Value |
|---|---|
| `CFBundleName` / `CFBundleDisplayName` | `TeamTrim` |
| `CFBundleIdentifier` | `dev.teamtrim.teamtrim` (matches the app id) |
| `CFBundleExecutable` | `teamtrim` |
| `CFBundleIconFile` | `dev.teamtrim.teamtrim` (the `.icns` in `Resources/`, extension optional) |
| `CFBundlePackageType` | `APPL` |
| `CFBundleShortVersionString` / `CFBundleVersion` | `0.1.0` / build number |
| `LSMinimumSystemVersion` | `11.0` |
| `NSHighResolutionCapable` | `true` |

Add `NSMicrophoneUsageDescription` and `NSCameraUsageDescription` **only** when
building with the `audio`/`video-capture` features, and a notifications usage
string once the desktop notifier is enabled. Unsigned local runs are blocked by
Gatekeeper; for local testing use an ad-hoc signature
(`codesign --force --deep -s - TeamTrim.app`). For distribution: sign with a
Developer ID, enable the hardened runtime, then notarize with `notarytool`.

## 4. Windows

```powershell
cargo build --release            # -> target\release\teamtrim.exe
```

- **Zip** — the simplest honest artifact: `teamtrim.exe`, `LICENSE`, `NOTICE`, and
  `vcruntime140.dll` (or build with `RUSTFLAGS=-C target-feature=+crt-static` to
  avoid the redistributable). No installer scripts are committed.
- **Icon** — `packaging/icons/dev.teamtrim.teamtrim.ico` is the multi-size icon
  for shortcuts and installer-generated entries. The `.exe` itself does **not**
  embed an icon resource yet: that needs a Windows resource compiler at build
  time (`winresource`/`embed-resource` in a build script), which is not wired up
  (B-013).
- **MSI** — [`cargo-wix`](https://github.com/volks73/cargo-wix) can scaffold and
  build one: `cargo install cargo-wix`, `cargo wix init`, `cargo wix`. It uses
  the WiX Toolset and produces `target\wix\*.msi`. Inno Setup is an equivalent
  alternative. Both need to be run on Windows and are unverified here.
- No WebView2 is required (there is no webview); the UI is native egui/OpenGL.

## 5. Where config, data and cache live

Resolved by `teamtrim_core::AppDirs` (the `directories` crate, project id
`dev.teamtrim.teamtrim`):

| | Linux | macOS | Windows |
|---|---|---|---|
| Config | `~/.config/teamtrim` | `~/Library/Application Support/dev.teamtrim.teamtrim` | `%APPDATA%\teamtrim\teamtrim\config` |
| Data | `~/.local/share/teamtrim` | `~/Library/Application Support/dev.teamtrim.teamtrim` | `%APPDATA%\teamtrim\teamtrim\data` |
| Cache | `~/.cache/teamtrim` | `~/Library/Caches/dev.teamtrim.teamtrim` | `%LOCALAPPDATA%\teamtrim\teamtrim\cache` |

Files:

- `settings.json` — preferences (theme, accent, font scale, notifications,
  download directory), atomic writes.
- `tokens.json` / `tokens-<profile>.json` — the five-token set, mode `0600` on
  Unix (ADR-008). **Secret.**
- `store.sqlite` (+ `-wal` / `-shm`) — the offline cache (chats, messages,
  files, call log). **Unencrypted** (B-007).
- `media/` — downloaded files when no `download_dir` is set.

Uninstall removes the binary and the `.desktop`/icon; delete the config/data/
cache directories above to remove local state.

## 6. CI release artifacts

[`.github/workflows/release.yml`](../.github/workflows/release.yml) builds a
downloadable archive per platform on **native runners** (no cross-compilation):

| Runner | Target | Artifact (Actions) | Contents |
|---|---|---|---|
| `ubuntu-latest` | `x86_64-unknown-linux-gnu` | `teamtrim-linux-x86_64` | `teamtrim-<version>-linux-x86_64.tar.gz` — `teamtrim`, `teamtrim-cli`, `LICENSE`, `NOTICE`, `README.md`, `packaging/` (`.desktop` + `icons/`) |
| `macos-14` | `aarch64-apple-darwin` | `teamtrim-macos-arm64` | `teamtrim-macos-arm64.zip` (`TeamTrim.app` with the `.icns`, ad-hoc signed) + `teamtrim-macos-arm64-cli.zip` (`teamtrim-cli` + docs) |
| `macos-14` | `x86_64-apple-darwin` | `teamtrim-macos-x86_64` | same layout as `arm64` (the Intel slice is cross-built on the arm64 runner, since the `macos-13` image is retired) |
| `windows-latest` | `x86_64-pc-windows-msvc` | `teamtrim-windows-x86_64` | `teamtrim-windows-x86_64.zip` — `teamtrim.exe`, `teamtrim-cli.exe`, `teamtrim.ico`, `LICENSE`, `NOTICE`, `README.md` |

**Triggers**

- `push` to `main` — validates packaging and uploads artifacts (no release).
- `push` of a `v*` tag — same, then a `release` job downloads every artifact and
  publishes a GitHub Release with them attached.
- `workflow_dispatch` — run on demand.

**Cutting a release**

```sh
# version comes from the tag (falls back to [workspace.package].version otherwise)
git tag v0.1.0
git push origin v0.1.0
```

The build is `cargo build --locked --release --target <triple>` with **default
features** (the `audio`, `video-capture`, `keyring` and `desktop-notifications`
features stay off), so the artifacts need no ALSA/SDL2/DBus and match the state
verified in [`STATUS.md`](STATUS.md).

**Not done by CI (and why).** No signing/notarization (needs Developer ID /
Authenticode secrets), no MSI/AppImage/Flatpak (need extra tooling and per-distro
testing), and the macOS `.app` is only *ad-hoc* signed so it launches locally
rather than passing Gatekeeper for distribution. Those remain manual steps in
sections 2–4.
