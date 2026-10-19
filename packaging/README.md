# `packaging/`

Small, honest packaging assets for TeamTrim. Nothing here builds a package by
itself — the actual build/install commands live in
[`../docs/PACKAGING.md`](../docs/PACKAGING.md); this directory only holds the
files those instructions install.

| File | Purpose |
|---|---|
| `dev.teamtrim.teamtrim.desktop` | Freedesktop launcher entry for Linux. `StartupWMClass` matches the app id `dev.teamtrim.teamtrim` set in `crates/teamtrim-app/src/main.rs`. |
| `icons/dev.teamtrim.teamtrim-<size>.png` | The app icon as RGBA PNGs (16, 24, 32, 48, 64, 128, 256, 512 and 1024 px). Linux installs these into the matching `hicolor/<size>x<size>/apps/` directories. |
| `icons/dev.teamtrim.teamtrim.ico` | Windows icon (16–256 px) for shortcuts and installer-generated entries. |
| `icons/dev.teamtrim.teamtrim.icns` | macOS icon (16–1024 px) for `TeamTrim.app/Contents/Resources/`. |

The running binary does **not** read this directory: `crates/teamtrim-app/assets/icon.png`
(the 256 px copy of `icons/dev.teamtrim.teamtrim-256.png`) is embedded with
`include_bytes!` and set via `ViewportBuilder::with_icon`, so the window and
taskbar icon work even for an uninstalled `cargo run`.

## Artwork

A blue/purple rounded tile with two "T"s. Only rasters are committed (there is no
vector source): the PNGs are cropped to the tile with transparent corners, so
they compose on light and dark backgrounds. `icons/dev.teamtrim.teamtrim-1024.png`
is the largest copy — treat it as the master.

To change the artwork, replace that master and regenerate the rest (needs
`ffmpeg`):

```sh
for size in 16 24 32 48 64 128 256 512 1024; do
  ffmpeg -v error -i new-master.png -vf "scale=$size:$size:flags=lanczos" \
    -pix_fmt rgba -compression_level 9 -pred mixed \
    -frames:v 1 -y "icons/dev.teamtrim.teamtrim-$size.png"
done
```

The `.ico` is an `ICONDIR` whose entries hold the 16/24/32/48/64/128/256 px PNGs;
the `.icns` is an `icns` container with `icp4`/`icp5`/`icp6` (16/32/64 px) and
`ic07`–`ic10` (128/256/512/1024 px) PNG chunks. Both are rebuilt from the PNGs
above (an icon editor, `icotool` or macOS `iconutil` does the same). Finally,
refresh the embedded copy and re-run the guard test:

```sh
cp icons/dev.teamtrim.teamtrim-256.png ../crates/teamtrim-app/assets/icon.png
cargo test -p teamtrim-app embedded_app_icon_decodes
```

## Linux quick install (from a release build)

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

Then launch **TeamTrim** from the application menu (or run `teamtrim`).
