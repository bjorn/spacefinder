# Space

A disk-space cleanup tool. Point it at a directory and it surfaces what
is **old**, **bulky**, or **redundant**, so you can clear it out with
confidence.

This is not a general-purpose file manager. Nautilus and its peers already
do that job well. Space is optimized for one question: **what should I
delete to free up disk?**

## Features so far

- **Recursive directory sizes.** A parallel `jwalk` walker fills a shared
  cache in the background. The UI stays responsive during the cold pass.
- **Three views of the same data.**
  - List with sortable columns (name, modified, size) and stable fallback
    for unknown sizes.
  - Grid of tile icons that reflows to window width.
  - Hierarchical **icicle columns** view: each column is one directory
    depth, with cells sized proportionally to byte totals. Click a folder
    to zoom in, click the root block to zoom out.
- **Selection + standard actions.** Click / ctrl-click / shift-click,
  context menu (cut, copy, paste, copy path, rename, move to trash,
  delete permanently), keyboard shortcuts.
- **Free-space readout in the status bar.** Tracks the device that hosts
  the current path, live as you navigate.
- **Persisted preferences.** View mode, sort, hidden-file toggle, window
  size, last location.
- **Localization.** German and English ship; strings live in gettext
  `.po` catalogs and get bundled into the binary at build time via
  Slint's translation support.

## What it is not (yet)

See [TODO.md](TODO.md) for the roadmap. Short version: filter bar,
aggregate summary header, treemap view, duplicate detection (core module
exists, UI pending), cleanup presets in the sidebar, thumbnails, batch
operations with undo, watcher-based auto-refresh.

## Tech stack

- Rust 2024 edition
- [Slint](https://slint.dev/) for the UI, targeting 1.16
- `jwalk` + `rayon` for parallel directory walking
- `trash` for move-to-trash
- `xdg-mime` and `freedesktop-icons` for MIME resolution (kept minimal for
  speed; richer icons are on the roadmap)

Slint's built-in translation system handles i18n via gettext `.po` files
compiled into the binary by `build.rs`.

## Building

```
cargo build --release
```

Debug builds also work for iteration. First build fetches and compiles
deps, so it takes a few minutes. Rebuilds are fast thanks to Slint's
incremental code generation and (optionally) `sccache`.

Optional: set up `sccache` as `RUSTC_WRAPPER` in `~/.cargo/config.toml` for
faster rebuilds across checkouts.

## Running

```
cargo run --release
```

The app opens at your home directory by default. Preferences are stored at
`$XDG_CONFIG_HOME/space/config.json` (typically `~/.config/space/config.json`).

Switch views via the three toggle buttons in the header (list, grid,
columns). Right-click for the context menu. F2 renames, Delete moves to
trash, F5 refreshes. Ctrl+L focuses the path bar.

## Status

Prototype. Usable for browsing and basic cleanup, but the scope of the
project is larger than what ships today. Bug reports and suggestions
welcome via the issue tracker at
[todo.sr.ht/~thorbjorn/space](https://todo.sr.ht/~thorbjorn/space).

## License

GPL-3.0-only. See individual source files for header comments.
