# `packaging/`

Small, honest packaging assets for TeamTrim. Nothing here builds a package by
itself — the actual build/install commands live in
[`../docs/PACKAGING.md`](../docs/PACKAGING.md); this directory only holds the
files those instructions install.

| File | Purpose |
|---|---|
| `dev.teamtrim.teamtrim.desktop` | Freedesktop launcher entry for Linux. `StartupWMClass` matches the app id `dev.teamtrim.teamtrim` set in `crates/teamtrim-app/src/main.rs`. |
| `dev.teamtrim.teamtrim.svg` | Placeholder app icon (vector). Install it as `dev.teamtrim.teamtrim.svg` under an icon theme so the `.desktop` entry finds it. |

There are deliberately **no** `build-appimage.sh` / `make-msi.ps1` scripts that
cannot be run or verified in this environment (no display, no network, no
tenant). A contributor with the toolchain runs the commands in
`docs/PACKAGING.md`; the honest state of cross-platform packaging is recorded in
`docs/STATUS.md`.

## Linux quick install (from a release build)

```sh
install -Dm755 target/release/teamtrim "$HOME/.local/bin/teamtrim"
install -Dm644 packaging/dev.teamtrim.teamtrim.desktop \
  "$HOME/.local/share/applications/dev.teamtrim.teamtrim.desktop"
install -Dm644 packaging/dev.teamtrim.teamtrim.svg \
  "$HOME/.local/share/icons/hicolor/scalable/apps/dev.teamtrim.teamtrim.svg"
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
```

Then launch **TeamTrim** from the application menu (or run `teamtrim`).
