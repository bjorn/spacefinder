# Space: roadmap

A disk-space cleanup tool. Point it at a directory, it scans and surfaces
what is **old**, **bulky**, or **redundant** so you can clear it out with
confidence. Nautilus already does general file management well, so this
app does not try to compete.

The current code is a generic file browser prototype inherited from an
earlier port. Most of it stays useful (list/grid, selection, context menu,
trash action), but the **framing** and **signals** shown in the UI need to
pivot. This TODO is written with that pivot in mind.

Items are roughly ordered, and each is independent unless `Depends on:`
says otherwise. Pick, reorder, drop as you like.

---

## 1. Recursive directory sizes

**Scope:** Show accurate size for directories. Today the list/grid shows
an empty string for dirs because `fs_scan::Entry::size_text` returns `""`.
Without this the app cannot tell you what is bulky.

**Files:**
- `src/fs_scan.rs::Entry`: add `size: u64` that includes contents for dirs
- `src/fs_scan.rs::scan`: after the cheap `read_dir` pass, kick off a
  recursive size walk per entry
- `src/controller.rs::push_ui_state`: use the computed dir size, show
  "Calculating..." while pending

**Approach:**
1. After the synchronous shallow scan returns, spawn a background task
   (`tokio::task::spawn_blocking`) per directory entry that walks with
   `walkdir` and sums `.len()` via `metadata()`.
2. Post results back via `slint::invoke_from_event_loop` and update a
   single row's `size_text` in the Slint model (`VecModel::set_row_data`).
3. Generation counter so stale results from an old directory are dropped
   when the user navigates.
4. Cache results by canonical path plus mtime so re-entering a dir is
   instant.

**Gotchas:**
- Symlink loops: pass `walkdir::WalkDir::new(p).follow_links(false)`.
- Permission errors are common, swallow them silently, report as `?` size.
- Don't block on NFS or mounted remotes indefinitely. Give each task a
  reasonable timeout (e.g. 30s) and mark partial results.

**Acceptance:** Open `~`, every directory row shows a real size within a
few seconds, subsequent visits are instant.

---

## 2. Age / last-accessed column

**Scope:** Add a visible "Last used" column (atime or best-effort)
alongside the existing "Modified" column, plus a sort option.

**Files:**
- `src/fs_scan.rs::Entry`: add `accessed: SystemTime`, `age_days: i32`
- `src/fs_scan.rs::SortCol`: add `Age`
- `ui/file_list.slint`: add column, sort header
- `ui/main.slint`: default sort becomes `Size desc` on first open

**Gotchas:**
- atime is unreliable on `noatime`-mounted filesystems (common on Linux
  these days). Fall back to `max(mtime, ctime)` and label the column
  "Last changed" when atime is inactive. Detect by checking `st_atime`
  equals `st_mtime` on many samples or by asking the mount options.
- Show ages as "3 days ago" or "2 years ago" (humanized), not raw dates,
  for this column. It is the one the user cares about most.

**Acceptance:** Sort by age, old files rise to the top.

---

## 3. Filter bar: size threshold, age threshold, file type

**Scope:** The core interaction. Below the header bar, a horizontal row of
controls: `Size >= [slider: 0 B .. 10 GB]`, `Age >= [slider: 0d .. 10y]`,
`Type [dropdown: any / images / videos / archives / code / docs / cache]`.

**Files:**
- `ui/filter_bar.slint` (new)
- `ui/main.slint`: insert between HeaderBar and content
- `src/controller.rs::rebuild_view`: apply filters during the existing
  filtered-indices computation

**Approach:**
- `FilterState { min_size: u64, min_age_days: u32, type_group: TypeGroup }`.
- `TypeGroup` maps to a set of MIME prefixes / extensions.
- Show per-slider markers for helpful defaults: 100 MB, 1 GB, 6 mo, 1 y.
- Persist the bar's state per session (see #10).

**Gotchas:**
- Filtering runs on every drag. Keep it cheap by only reading already
  scanned entries, with no allocations in the loop.
- Empty state: if filters hide everything, show a "No matches, loosen the
  filters" panel, not a blank grid.

**Acceptance:** Slide "Size >= 1 GB" in Home, only big files remain. Slide
"Age >= 1 year" further narrows them.

---

## 4. Aggregate summary header

**Scope:** Above the list, a strip showing:
- Total size of the current directory
- "Selected: 3 items, 4.2 GB"
- After filters: "Showing 42 of 556 items (1.8 GB / 12 GB)"
- A small stacked bar visualizing the breakdown by top-level subfolder.

**Files:**
- `ui/summary.slint` (new)
- `src/controller.rs`: maintain totals as rows are filtered or selected

**Depends on:** #1 (accurate dir sizes).

**Acceptance:** Sums update live as filters and selection change.

---

## 5. Treemap / size-visualization view

**Scope:** A third view mode next to list and grid. A treemap (squarified)
showing each direct child sized proportionally. Click to drill down.

**Files:**
- `ui/treemap.slint` (new): thin Slint shell, layout computed by Rust
- `src/treemap.rs` (new): squarified treemap algorithm, outputs
  `Vec<{ index, x, y, w, h }>`
- `ui/main.slint`: add third toggle in view-mode group

**Approach:**
- Classic squarified treemap (Bruls, Huijsen, van Wijk). Rust computes
  rectangles for a single level, nested drilldown reruns on click.
- Each tile labeled with name and size, dimmed when below threshold,
  clickable.

**Depends on:** #1.

**Acceptance:** Treemap view of `~` instantly reveals the biggest
consumers.

---

## 6. Duplicate detection

**Scope:** Find groups of identical files so you can keep one and delete
the rest.

**Files:**
- `src/dupes.rs` (new): size bucket, then partial hash, then full hash
  pipeline
- `ui/dupes_view.slint` (new): grouped list, per-group "keep newest / keep
  in path / manual" actions

**Approach:**
1. Bucket entries by size (equal size is the precondition for equal bytes).
2. For each bucket with >=2 entries, compute a fast hash over the first
   4 KiB. Files with equal heads get full-content hashes (md5 or blake3).
3. Render groups sorted by wasted bytes (group_size * (n-1)).
4. Actions per group: select all but one, move to trash.

**Gotchas:**
- Hash only regular files, skip symlinks and device nodes.
- Reading many large files hurts disk caches, so show incremental progress
  so the user can cancel.
- Hardlinks: detect and exclude via `(st_dev, st_ino)` deduplication.

**Acceptance:** Run on `~/Downloads`, identifies redundant copies of large
files, one-click moves all but one to trash.

---

## 7. Cleanup presets in the sidebar

**Scope:** Replace the generic "Places" sidebar with curated cleanup
entrypoints that scan interesting locations directly:

- **Downloads**: things we usually forget
- **Trash**: review before emptying
- **Cache** (`~/.cache`): safe to wipe
- **Dev caches**: `~/.cargo/registry`, `~/.npm`, `~/.cache/pip`, build
  dirs (`target/`, `node_modules/`)
- **Big videos**: preset with type=video, size >= 500 MB (no dir
  restriction)
- **Old archives**: preset with type=archive, age >= 1 y
- **Screenshots**: `~/Pictures/Screenshots/`
- **Logs & crashes**: `~/.xsession-errors*`, `/var/log` (readable parts)

**Files:**
- `src/sidebar.rs`: replace `Places`/`Drives` groups with
  `Cleanup presets`
- `src/controller.rs`: each preset is `(path, FilterState)`, not just a
  path

**Depends on:** #3 (filters), because presets are (location, filter) pairs.

**Acceptance:** Sidebar is a menu of common cleanup tasks, not a
navigation tree.

---

## 8. Per-MIME icons (async resolver)

**Scope:** Restore typed icons so file types are identifiable at a glance.
This matters a lot in a cleanup tool where type groups drive decisions.
See `src/icons.rs`, currently returns the bundled generic icon.

**Why deferred earlier:** `freedesktop_icons::lookup` is about 275 ms per
miss on this machine, so doing it synchronously for 500+ entries at
navigate time blocks the UI.

**Approach:**
1. Synchronous path returns the bundled fallback immediately.
2. Background worker receives `(row_index, PathBuf, extension)`. Extension
   is the cache key: most wins come from sharing icons across files with
   the same extension.
3. Worker returns a `PathBuf` (the resolved theme icon file). Post back
   via `slint::invoke_from_event_loop`, and on the main thread call
   `Image::load_from_path` and `VecModel::set_row_data(i, updated)`.
4. Generation counter drops results from stale navigations.

**Gotchas:**
- `Image` is `!Send`. Keep the conversion on the UI thread.
- Cache must outlive navigation. Store
  `Arc<Mutex<FxHashMap<String, PathBuf>>>`.

**Acceptance:** Home directory shows typed icons within a second, with no
visible blocking.

---

## 9. Thumbnails for images / videos

**Scope:** Freedesktop thumbnail cache integration. Lets users eyeball
which 2 GB video to keep.

**Files:**
- `src/thumbnails.rs` (new)
- `src/icons.rs`: route image and video MIME types through the thumbnailer

**Approach:**
1. Check `$XDG_CACHE_HOME/thumbnails/large/md5(file://URI).png` first.
2. On miss, generate via the `image` crate for raster and via
   `.thumbnailer` desktop files for others.
3. Reuse the async worker from #8.

**Depends on:** #8 for the async plumbing.

**Acceptance:** `~/Videos` in grid view shows frame thumbnails.

---

## 10. Settings persistence

**Scope:** Remember view mode, sort, filter bar state, last scanned
location, window size.

**Files:**
- `src/config.rs` (new)

**Approach:**
- `serde_json` to `$XDG_CONFIG_HOME/space/config.json`.
- Load at startup, debounced save on change (500 ms).

**Acceptance:** Relaunch restores the session.

---

## 11. Batch operations with undo

**Scope:** Selecting 200 files and moving them to trash must not freeze
the UI and must be recoverable.

**Files:**
- `src/ops.rs` (new): `Operation` enum, each async
- `ui/progress.slint` (new): bottom-docked overlay
- `src/undo.rs` (new): LIFO of `UndoEntry`

**Approach:**
1. `tokio::task` per operation, mpsc channel reports progress and errors.
2. `trash::delete` returns a token. Store it in `UndoEntry::Trash` so
   Ctrl+Z restores precisely.
3. Progress overlay aggregates running ops. Click expands.

**Acceptance:** Trash 500 items, press Ctrl+Z, they come back.

---

## 12. Watcher-based auto-refresh

**Scope:** If a background process writes to the scanned dir, the view
updates within about 300 ms.

**Files:**
- `src/watcher.rs` (new)

**Approach:** `notify-debouncer-full` per scanned location. On event,
update just the changed rows (not full rescan) where possible.

**Acceptance:** `touch ~/newfile`, row appears soon after.

---

## 13. Keyboard shortcuts

**Scope:** Cleanup-appropriate shortcuts.

Currently wired: Backspace (up), Delete (trash), F2 (rename), F5
(refresh), Enter (open).

**Add:**
- `Ctrl+A` select all visible
- `Ctrl+D` deselect all
- `Ctrl+F` focus search
- `Escape` clear selection, close context menu
- Arrow keys and Shift+Arrow for focus ring and range selection
- `Shift+Delete` permanent delete
- `Ctrl+Z` undo (see #11)
- Type-to-search: incremental prefix match with 300 ms reset

**Files:** `src/controller.rs::handle_key`, `ui/main.slint` FocusScope.

---

## 14. Arrow-key focus ring

**Scope:** A keyboard-navigable focus distinct from mouse selection.

**Files:** `src/controller.rs`: `focus_index`.
`ui/file_list.slint` and `ui/file_grid.slint`: 1 px accent border on
focused row.

**Depends on:** #13.

---

## 15. Rubber-band selection

**Scope:** Click-drag on empty area selects items inside the rectangle.

**Files:** `ui/file_list.slint`, `ui/file_grid.slint`, `src/controller.rs`.

**Approach:** TouchArea `moved` and `pressed`, translucent rectangle
overlay. Rust recomputes selection from per-row geometry (row height
fixed in list, precomputed `(row, col)` grid positions).

**Acceptance:** Drag over tiles, all inside are selected. Ctrl+Drag adds.

---

## 16. Toast notifications

**Scope:** "Moved 14 items to trash" with an "Undo" action, error toasts
for permission denials.

**Files:** `ui/toast.slint` (new), `src/controller.rs` toast queue.

**Depends on:** #11 for the Undo action target.

---

## 18. Drag-and-drop out to other apps

Lower priority for a cleanup tool. If implemented, source-side DnD via
the Wayland data device requires window-adapter plumbing.

---

## 19. Internationalization

**Scope:** All user-visible strings localized. Wanted even for a disk
cleanup tool, since non-English users appreciate clear labels when they're
deciding what to delete.

**Use Slint's built-in translation system**, not fluent / i18n-embed.
Docs: https://docs.slint.dev/latest/docs/slint/guide/development/translations/

**Marking strings**: wrap in `@tr(...)` inside `.slint` files.
- Plain: `text: @tr("Cancel");`
- Interpolation: `text: @tr("Showing {0} of {1}", shown, total);`
- Plurals: `text: @tr("{n} item" | "{n} items" % count);`
- Disambiguation: `@tr("Toolbar" => "Cut", ...)`. Default context is the
  component name; disable globally via
  `CompilerConfiguration::set_default_translation_context(DefaultTranslationContext::None)`
  if you want a flat namespace.

**Choose the bundled path, not runtime gettext.** Catalogs compile into
the binary via `build.rs`; no runtime dependency on system gettext, no
filesystem lookup at startup, and switching language is a single call.

**Files:**
- `lang/<locale>/LC_MESSAGES/space.po`  (domain name must match the Cargo
  package name `space`)
- `build.rs`: extend the existing `CompilerConfiguration` with
  `.with_bundled_translations("lang")`
- `src/main.rs`: optional explicit pick via
  `slint::select_bundled_translation(&lang)` before creating the window;
  otherwise Slint auto-detects from the locale (requires the `std` feature
  on `slint`, already the default).
- `Cargo.toml`: no extra crate feature needed for the bundled path.
  Would need `slint = { features = ["gettext"] }` only if we were doing
  runtime `.mo` loading (skip).

**Tooling:**
- `cargo install slint-tr-extractor`
- Extraction:
  `find ui -name '*.slint' | xargs slint-tr-extractor -o lang/space.pot`
- Translators produce `lang/<locale>/LC_MESSAGES/space.po` (editors:
  Poedit, Lokalize, or plain text). Bundled mode reads `.po` directly, no
  `msgfmt` step.

**Strings living in Rust:**
The Slint extractor only sees `.slint` files. Status-bar text like
"Calculating..." or "556 items" is currently formatted in Rust
(`controller.rs::push_ui_state`). Two options:
1. **Preferred:** move those into `.slint` via `@tr(...)` by exposing
   the raw counts/flags as properties and formatting on the UI side.
   Example: a `status-text` property becomes
   `status: @tr("{n} item" | "{n} items" % root.total)`.
2. If some strings really must stay Rust-side (e.g. error messages),
   add a thin gettext-like helper or use `rust-i18n` scoped to those;
   keep it small.

Prefer option 1 where possible so the whole catalog comes from one
extractor pass.

**Language switcher:** post-MVP, but trivial to add: a dropdown in
settings triggers `slint::select_bundled_translation(selected_locale)`
and the UI re-renders.

**Acceptance:** `LANG=de_DE cargo run` shows a German UI. Bundled `.po`
files ship inside the binary, nothing read from disk at runtime.

**Gotchas:**
- Domain name is the Cargo package name, so renaming the crate breaks
  translations.
- `slint-tr-extractor` adds a default context per component name;
  translators have to preserve that context. Decide early whether to keep
  it (more structure, more friction) or disable it (flat catalog, short).
- Bundled translations are baked at build time; `cargo run` after editing
  a `.po` needs a rebuild. Not an issue, just a heads-up.

---

## Cross-cutting notes

- **`slint::invoke_from_event_loop`** is the bridge for every background
  worker (dir sizes, icons, thumbnails, watcher, ops).
- **Models:** always prefer `VecModel::set_row_data(i, new)` over
  rebuilding. It preserves scroll and selection state.
- **Slint virtualization:** list and grid already use `ListView`, keep
  additions inside the virtualized viewport.
- **Dev loop:** `cargo run` in this repo's root, debug mode is fine for
  iteration.

---

## Removed from scope

Features intentionally dropped from the earlier file-manager TODO because
they do not fit Space: multi-tab, multi-window, inline rename, Open With
picker, path-bar autocomplete, preview pane / gallery mode, archive
create/extract dialogs, network / GVFS mounts, desktop mode / applet,
per-directory view settings, Ctrl+scroll zoom.

**Cross-app clipboard cut/copy/paste** (uri-list,
x-special/gnome-copied-files) is also out of scope. Space does not shuffle
files between apps, it deletes them. The tiny clipboard story we keep:

- "Copy path" already works (writes the selected paths as text, useful
  for pasting into a terminal or editor).
- When the user pastes a path into the path bar, navigate there. Trivial
  follow-up to the existing path-edit mode, not worth a dedicated task.

Use Nautilus for the real clipboard integration.
