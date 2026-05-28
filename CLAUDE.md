# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

A disk-space cleanup tool: a Slint desktop UI over a Rust core. See `README.md`
for features, build/run instructions, the config-file location, and tech stack.
This file covers what those docs don't: the cross-file architecture and the
test/lint commands.

## Commands

```
cargo test                       # all tests (unit tests live inline in each module)
cargo test parse_po              # run tests matching a name (binary crate; no --lib target)
cargo test sort_indices          # e.g. the sort tests in controller.rs
cargo clippy --all-targets
cargo fmt
```

Build and run are in `README.md`. `build.rs` compiles `ui/main.slint` and bundles
the `lang/` translations; touching any `.slint` file or `.po` catalog triggers
regeneration on the next `cargo build`.

## Architecture

**Generated Slint bindings.** `slint::include_modules!()` in `main.rs` pulls in
types generated from `ui/main.slint` at build time (`MainWindow`, `FileItem`,
`Tile`, `ColumnCell`, `Crumb`, `MenuEntry`, ...). These names appear in Rust with
no Rust-side definition; they come from the `.slint` files.

**Single UI thread + background size workers.** Everything in `controller.rs`
runs on the Slint UI thread and is held as `Rc<RefCell<App>>` (not `Send`).
Directory sizing is the only concurrent work: `dir_size::SizeEngine` owns a
shared `rayon` thread pool (via `jwalk`) that walks trees and writes a global
`CACHE` (a `Mutex<FxHashMap<(canonical_path, mtime), CachedAgg>>`). Workers post
results back with `slint::Weak::upgrade_in_event_loop`; because the closure must
be `Send`, it cannot capture the `Rc<App>`, so it fishes the app out of the
thread-local `APP_TLS` instead. When editing async/size code, preserve this
hand-off: no `App` state is touched off the UI thread. The size path is rayon;
there is no tokio/async runtime in this project.

**App state and the entries/filtered/selection indexing invariant.** `App` in
`controller.rs` is the central state machine. `entries: Vec<Entry>` is the
unsorted source of truth. `filtered: Vec<usize>` holds indices into `entries`
for what's currently shown (after hidden-file filter, search, and sort).
Crucially, `selection` and `last_clicked` are also indices into `entries`, never
into `filtered`, so a re-sort or filter toggle keeps the same files highlighted.
Respect this when adding selection or view logic. UI rows are display indices
into `filtered`; the click handlers translate display index -> entries index.

**Cache-first rendering.** `dir_size::lookup_cached_size` / `lookup_cached_total`
are synchronous cache probes used to backfill already-walked sizes before the
first paint, avoiding a "pending" flash; a miss is what schedules the background
walk via `SizeEngine::compute`.

**Two independent translation layers.** Slint `.slint` strings use Slint's
bundled-translation system (compiled by `build.rs`). Rust-side strings go
through `i18n.rs`, a hand-rolled `.po` parser exposing `tr` / `tr_n` / `tr_fmt` /
`tr_n_fmt`. Both read the same `lang/*/LC_MESSAGES/space.po` catalogs. A new
user-facing string must be added on whichever side renders it (and to the `.po`
files for both languages).

**Layout modules feed UI converters.** `columns.rs` (icicle columns) and
`treemap.rs` (squarified treemap) are pure layout algorithms producing
`LaidCell` / `LaidTile`. `controller.rs` converts these to the generated Slint
types via `laid_cell_to_ui` / `laid_tile_to_ui` before pushing them into models.

**Other modules.** `fs_scan.rs` = directory `scan` + the `Entry` model, `SizeState`,
`SortCol`, on-disk byte accounting. `config.rs` = JSON load/atomic-save of
persisted prefs. `disk.rs` = free/total space. `sidebar.rs` = places/drives list.
`icons.rs` = icon loading. Slint callbacks are wired in `wire_callbacks` near the
bottom of `controller.rs`.
