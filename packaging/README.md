# `packaging/`

Small, honest packaging assets for Squads. Nothing here builds a package by
itself — the actual build/install commands live in
[`../docs/PACKAGING.md`](../docs/PACKAGING.md); this directory only holds the
files those instructions install.

| File | Purpose |
|---|---|
| `dev.squads.squads.desktop` | Freedesktop launcher entry for Linux. `StartupWMClass` matches the app id `dev.squads.squads` set in `crates/squads-app/src/main.rs`. |
| `dev.squads.squads.svg` | Placeholder app icon (vector). Install it as `dev.squads.squads.svg` under an icon theme so the `.desktop` entry finds it. |

There are deliberately **no** `build-appimage.sh` / `make-msi.ps1` scripts that
cannot be run or verified in this environment (no display, no network, no
tenant). A contributor with the toolchain runs the commands in
`docs/PACKAGING.md`; the honest state of cross-platform packaging is recorded in
`docs/STATUS.md`.

## Linux quick install (from a release build)

```sh
install -Dm755 target/release/squads "$HOME/.local/bin/squads"
install -Dm644 packaging/dev.squads.squads.desktop \
  "$HOME/.local/share/applications/dev.squads.squads.desktop"
install -Dm644 packaging/dev.squads.squads.svg \
  "$HOME/.local/share/icons/hicolor/scalable/apps/dev.squads.squads.svg"
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
```

Then launch **Squads** from the application menu (or run `squads`).
