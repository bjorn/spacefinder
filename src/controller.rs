use crate::columns::{self, LaidCell};
use crate::config::{self, Config};
use crate::dir_size::{self, SizeEngine};
use crate::disk;
use crate::fs_scan::{self, Entry, SizeState, SortCol};
use crate::i18n::{tr, tr_fmt, tr_n_fmt};
use crate::icons::Icons;
use crate::sidebar::{self, TRASH_TAG};
use crate::treemap::{self, Tile as LaidTile};
use crate::{ColumnCell, Crumb, FileItem, FileListState, MainWindow, MenuEntry, Tile};
use humansize::{format_size, BINARY};
use rustc_hash::FxHashSet;
use slint::{ComponentHandle, Global, Model, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Minimum spacing between consecutive writes of the config file. Additional
/// state changes inside this window are coalesced into a single follow-up
/// save scheduled via `slint::Timer::single_shot`.
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(500);

thread_local! {
    /// Handle to the running `App`, set in [`App::new`]. Background size
    /// workers post results through `slint::Weak::upgrade_in_event_loop`,
    /// and the closure on the UI thread fishes the app out of here rather
    /// than capturing an `Rc` (which is not `Send`).
    static APP_TLS: RefCell<Option<Weak<RefCell<App>>>> = const { RefCell::new(None) };
}

pub struct App {
    ui: slint::Weak<MainWindow>,
    icons: Icons,

    current: PathBuf,
    history: Vec<PathBuf>,
    history_i: usize,

    entries: Vec<Entry>,
    filtered: Vec<usize>, // indices into `entries` visible after filter+sort
    /// Entries the user has selected. Stored as indices into
    /// `entries` (the unsorted source-of-truth) rather than into
    /// `filtered`, so a sort change or filter toggle keeps the same
    /// files highlighted instead of inheriting the new occupants of
    /// the old display rows.
    selection: FxHashSet<usize>,
    /// Entry index of the user's most-recent focus anchor — drives
    /// shift-range and arrow-key navigation. Same `entries` keying as
    /// `selection` for the same reason.
    last_clicked: Option<usize>,
    cut_paths: FxHashSet<PathBuf>, // paths marked as "cut"
    clipboard_op: Option<ClipOp>,

    show_hidden: bool,
    folders_first: bool,
    search: String,
    sort_col: SortCol,
    sort_asc: bool,

    pending_confirm: Option<ConfirmAction>,
    pending_rename: Option<PathBuf>, // path being renamed via dialog

    /// Shared model driving the list/grid views. Background size workers
    /// mutate individual rows on this model via `set_row_data` rather than
    /// replacing the whole thing, so the UI preserves scroll and selection
    /// while sizes stream in.
    items_model: Rc<VecModel<FileItem>>,
    /// Bumped on every navigation / refresh. Stale size results carrying
    /// an older generation are dropped by [`App::on_size_update`].
    generation: u64,
    /// Parallel directory-size computer.
    size_engine: Arc<SizeEngine>,

    /// Current `view_mode` from the Slint side. Rust is the source of truth
    /// for persistence: we mirror UI changes in here via `set_view_mode`.
    /// 0 = list, 1 = grid, 2 = columns (icicle), 3 = treemap. Matches
    /// `ui/main.slint`.
    view_mode: i32,

    /// Icicle columns view state. Only meaningful while `view_mode == 2`.
    /// The root is now `self.current`; column view shares navigation
    /// with list/grid/crumbs so there is no separate root field.
    ///
    /// `column_selected_path` holds the single path a left-click on a
    /// cell has "selected" for highlighting and right-click context-menu
    /// targeting. Distinct from `selection` (indices into `filtered`)
    /// because the columns view shows paths that may not be direct
    /// children of `current`.
    column_selected_path: Option<PathBuf>,
    /// Shared Slint model backing the columns view. Replaced wholesale on
    /// every [`App::recompute_columns`] rather than mutated row-by-row,
    /// because the layout is a single pass and the cell count varies.
    columns_model: Rc<VecModel<ColumnCell>>,
    /// Rust-side copy of the currently-laid column cells. Kept in sync
    /// with `columns_model` by `recompute_columns`. Used by
    /// [`App::handle_key`] to implement arrow-key navigation without
    /// reparsing paths back out of the Slint model.
    column_cells: Vec<LaidCell>,

    /// Squarified treemap view state. Only meaningful while
    /// `view_mode == 3`. The treemap lays out the direct children of
    /// `self.current` sourced from `filtered`, so selection state is
    /// shared with the list and grid views via the top-level
    /// `selection` set — not a separate path. Ctrl-click toggling and
    /// plain-click single-select both route through [`App::click`].
    ///
    /// Shared Slint model backing the treemap view. Replaced wholesale
    /// on every [`App::recompute_treemap`] rather than mutated
    /// row-by-row, because the layout is a single pass and the tile
    /// count varies.
    treemap_model: Rc<VecModel<Tile>>,
    /// Rust-side copy of the currently-laid tiles. Kept in sync with
    /// `treemap_model` by `recompute_treemap`. Used by
    /// [`App::handle_key_treemap`] to find the nearest-neighbor tile
    /// for arrow-key navigation without reparsing data back out of the
    /// Slint model.
    treemap_tiles: Vec<LaidTile>,
    /// Last-observed window size, in logical pixels. Refreshed on every
    /// `persist()` call so a final `on_close_requested` save catches the
    /// latest dimensions.
    window_size: [u32; 2],
    /// Timestamp of the most recent `config::save` call. `None` means no
    /// save has happened yet. Used by the debounce in `persist`.
    last_save: Option<Instant>,
    /// Set while a follow-up save is queued on the Slint timer. Prevents
    /// stacking more than one pending save while the user mashes through
    /// state changes.
    save_scheduled: Rc<Cell<bool>>,
    /// When `go_up` fires, the name of the directory we just left. After
    /// the parent's listing is rebuilt, `refresh` consults this and sets
    /// the selection to the matching entry so the child the user came
    /// from stays highlighted (and is the anchor for subsequent arrow-
    /// key moves). Cleared after one use.
    select_after_navigate: Option<String>,
}

#[derive(Copy, Clone)]
enum ClipOp {
    Copy,
    Cut,
}

enum ConfirmAction {
    DeleteToTrash(Vec<PathBuf>),
    PermanentDelete(Vec<PathBuf>),
}

impl App {
    pub fn new(ui: &MainWindow, start: PathBuf, cfg: Config) -> Rc<RefCell<Self>> {
        let icons = Icons::new();
        let sidebar::Built { items: sidebar_items, places_to_size } = sidebar::build(&icons);
        let items_model = Rc::new(VecModel::<FileItem>::default());
        ui.set_items(ModelRc::from(items_model.clone()));
        let columns_model = Rc::new(VecModel::<ColumnCell>::default());
        ui.set_column_cells(ModelRc::from(columns_model.clone()));
        let treemap_model = Rc::new(VecModel::<Tile>::default());
        ui.set_treemap_tiles(ModelRc::from(treemap_model.clone()));

        // Translate the persisted sort column enum into the controller's
        // runtime enum. Keep the two types decoupled so the on-disk format
        // can evolve independently of `fs_scan`.
        let sort_col = sort_col_from_config(cfg.sort_col);
        let view_mode = view_mode_from_config(cfg.view_mode);

        let app = Rc::new(RefCell::new(Self {
            ui: ui.as_weak(),
            icons,
            current: start.clone(),
            history: vec![start.clone()],
            history_i: 0,
            entries: Vec::new(),
            filtered: Vec::new(),
            selection: FxHashSet::default(),
            last_clicked: None,
            cut_paths: FxHashSet::default(),
            clipboard_op: None,
            show_hidden: cfg.show_hidden,
            folders_first: cfg.folders_first,
            search: String::new(),
            sort_col,
            sort_asc: cfg.sort_asc,
            pending_confirm: None,
            pending_rename: None,
            items_model,
            generation: 0,
            size_engine: Arc::new(SizeEngine::new()),
            view_mode,
            column_selected_path: None,
            columns_model,
            column_cells: Vec::new(),
            treemap_model,
            treemap_tiles: Vec::new(),
            window_size: cfg.window_size,
            last_save: None,
            save_scheduled: Rc::new(Cell::new(false)),
            select_after_navigate: None,
        }));

        // Stash a weak handle for background workers to reach back through.
        APP_TLS.with(|slot| *slot.borrow_mut() = Some(Rc::downgrade(&app)));

        let sidebar_model = Rc::new(VecModel::from(sidebar_items));
        ui.set_sidebar_items(ModelRc::from(sidebar_model));

        // Fill in the recursive used-space totals for each Places entry via
        // the shared SizeEngine, so cache + on-disk-byte semantics match the
        // file/grid/columns views.
        sidebar::spawn_places_size_jobs(
            app.borrow().size_engine.clone(),
            ui.as_weak(),
            places_to_size,
        );

        wire_callbacks(ui, app.clone());
        {
            let mut a = app.borrow_mut();
            a.refresh();
            // Reflect the persisted view mode into the UI after the first
            // `push_ui_state` so the grid vs list toggle matches on the very
            // first paint.
            if let Some(ui) = a.ui.upgrade() {
                ui.set_view_mode(a.view_mode);
            }
        }
        app
    }

    /// Build a `Config` snapshot from the current in-memory state.
    fn snapshot_config(&self) -> Config {
        Config {
            view_mode: view_mode_to_config(self.view_mode),
            sort_col: sort_col_to_config(self.sort_col),
            sort_asc: self.sort_asc,
            show_hidden: self.show_hidden,
            folders_first: self.folders_first,
            window_size: self.window_size,
            last_location: Some(self.current.clone()),
        }
    }

    /// Update the cached window-size from the live `Window`. Called right
    /// before a save so the snapshot reflects whatever the compositor last
    /// handed us.
    fn refresh_window_size(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let size = ui.window().size();
        let scale = ui.window().scale_factor();
        if scale > 0.0 {
            let w = (size.width as f32 / scale).round() as u32;
            let h = (size.height as f32 / scale).round() as u32;
            if w > 0 && h > 0 {
                self.window_size = [w, h];
            }
        }
    }

    /// Persist the current state to disk with debouncing. Saves happen at
    /// most once per `PERSIST_DEBOUNCE`; while the window is closed (or
    /// the cooldown is active), the state is captured into a single
    /// follow-up timer rather than written immediately.
    fn persist(&mut self) {
        self.refresh_window_size();
        let now = Instant::now();
        let can_save_now = match self.last_save {
            None => true,
            Some(prev) => now.duration_since(prev) >= PERSIST_DEBOUNCE,
        };
        if can_save_now {
            let cfg = self.snapshot_config();
            config::save(&cfg);
            self.last_save = Some(now);
            return;
        }
        // Cooldown active: coalesce into a single scheduled save.
        if self.save_scheduled.get() {
            return;
        }
        self.save_scheduled.set(true);
        let remaining = self
            .last_save
            .map(|prev| {
                PERSIST_DEBOUNCE
                    .checked_sub(now.duration_since(prev))
                    .unwrap_or(Duration::from_millis(0))
            })
            .unwrap_or(PERSIST_DEBOUNCE);
        let flag = self.save_scheduled.clone();
        slint::Timer::single_shot(remaining, move || {
            flag.set(false);
            APP_TLS.with(|slot| {
                let Some(weak) = slot.borrow().clone() else { return };
                let Some(app) = weak.upgrade() else { return };
                if let Ok(mut app) = app.try_borrow_mut() {
                    app.refresh_window_size();
                    let cfg = app.snapshot_config();
                    config::save(&cfg);
                    app.last_save = Some(Instant::now());
                } else {
                    log::trace!("debounced save skipped: app already borrowed");
                }
            });
        });
    }

    /// Persist synchronously regardless of the debounce window. Used at
    /// shutdown to guarantee the latest window size hits disk even when
    /// the user resized just before closing.
    fn persist_now(&mut self) {
        self.refresh_window_size();
        let cfg = self.snapshot_config();
        config::save(&cfg);
        self.last_save = Some(Instant::now());
    }

    fn refresh(&mut self) {
        // Bump before kicking off any async work so in-flight callbacks from
        // a previous scan get dropped.
        self.generation = self.generation.wrapping_add(1);

        // Selection is keyed on `entries` indices. Re-scanning replaces
        // the vector, which would otherwise orphan those indices onto
        // whatever rows happened to land at the same positions —
        // visible to the user as "deleted files leave random others
        // selected." Snapshot paths first, re-resolve below.
        let prev_selected_paths: FxHashSet<PathBuf> = self
            .selection
            .iter()
            .filter_map(|&e| self.entries.get(e))
            .map(|e| e.path.clone())
            .collect();
        let prev_anchor_path: Option<PathBuf> = self
            .last_clicked
            .and_then(|e| self.entries.get(e))
            .map(|e| e.path.clone());
        self.selection.clear();
        self.last_clicked = None;

        self.entries = fs_scan::scan(&self.current).unwrap_or_else(|e| {
            log::warn!("scan failed for {}: {}", self.current.display(), e);
            Vec::new()
        });
        log::info!(
            "scanned {} ({} entries)",
            self.current.display(),
            self.entries.len()
        );
        // Backfill dir sizes (and recursive mtimes) synchronously from
        // the shared cache. `fs_scan::scan` always flags directories as
        // `Calculating`, which is correct on a cold walk but produces a
        // one-frame flash of "…" every time the user revisits a folder
        // we've already settled. The async size pass still runs below;
        // cache hits there no-op since the state is unchanged.
        for entry in &mut self.entries {
            if !entry.is_dir {
                continue;
            }
            if let Some((size, rec_mtime)) = dir_size::lookup_cached_total(&entry.path) {
                entry.size_state = SizeState::Known(size);
                if let Some(t) = rec_mtime {
                    entry.modified = t;
                }
            }
        }
        // Re-resolve the snapshot against the new entries. Files that
        // were deleted, moved, or renamed drop out naturally — exactly
        // what the user expects after a trash or delete.
        if !prev_selected_paths.is_empty() {
            for (i, e) in self.entries.iter().enumerate() {
                if prev_selected_paths.contains(&e.path) {
                    self.selection.insert(i);
                }
            }
        }
        if let Some(p) = prev_anchor_path {
            self.last_clicked = self.entries.iter().position(|e| e.path == p);
        }
        self.rebuild_view();
        // If a preceding `go_up` requested it, re-select the child
        // directory we just left so it stays highlighted (and becomes
        // the keyboard-focus anchor). One-shot.
        if let Some(name) = self.select_after_navigate.take() {
            if let Some(&eidx) = self
                .filtered
                .iter()
                .find(|&&e| self.entries[e].name == name)
            {
                self.selection.clear();
                self.selection.insert(eidx);
                self.last_clicked = Some(eidx);
            }
        }
        self.push_ui_state();
        self.spawn_size_jobs();
        if self.view_mode == 2 {
            self.recompute_columns();
        } else if self.view_mode == 3 {
            self.recompute_treemap();
        }
    }

    /// Run the icicle layout against the current directory and push
    /// the resulting cells into the Slint `VecModel`. Cheap: walks at
    /// most [`columns::VISIBLE_COLUMNS`] levels and skips cells thinner
    /// than one logical pixel, so the total cell count stays bounded
    /// regardless of tree size.
    ///
    /// Layout is produced in fractional coordinates (`y_start`, `y_end`
    /// in `[0.0, 1.0]`). The Slint side multiplies by its live inner
    /// height to turn fractions into pixels, so a window resize reflows
    /// every cell with no Rust round-trip. This means the layout does
    /// not depend on the window size at all and the previous
    /// `changed content-h` callback is not needed, avoiding the binding
    /// loop it used to trip.
    fn recompute_columns(&mut self) {
        let laid = columns::lay_out(&self.current, self.show_hidden);
        let selected = self.column_selected_path.as_deref();
        let cells: Vec<ColumnCell> = laid
            .iter()
            .map(|c| laid_cell_to_ui(c, selected))
            .collect();
        self.columns_model.set_vec(cells);
        self.column_cells = laid;
    }

    /// Run the squarified treemap layout against the current `filtered`
    /// slice and push the resulting tiles into the Slint `VecModel`.
    /// Cheap: single-level over direct children, with sub-pixel tiles
    /// dropped, so the tile count stays bounded.
    ///
    /// Feeding `filtered` (not a fresh disk walk) into the layout is
    /// what lets selection, hidden-file toggle and search filter all
    /// share one source of truth with the list and grid views. Each
    /// tile carries the same `display_idx` (row in `filtered`) that
    /// `on_item_clicked` uses, so Ctrl-click, double-click and right-
    /// click all route through the already-tested list/grid handlers.
    ///
    /// Layout is produced in fractional coordinates (`x`, `y`, `w`, `h`
    /// in `[0.0, 1.0]`). The Slint side multiplies by its live inner
    /// width and height, so a window resize reflows every tile with no
    /// Rust round-trip. This means the layout does not depend on the
    /// window size at all.
    fn recompute_treemap(&mut self) {
        let inputs: Vec<treemap::TileInput> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(display_idx, &eidx)| {
                let e = &self.entries[eidx];
                // Best-known on-disk bytes. For files this is the
                // direntry len; for dirs, the cached recursive total
                // (zero until the walker settles — which drops the
                // tile out of the layout, same as the cold column-
                // view behavior).
                let size = match e.size_state {
                    SizeState::Known(n) => n,
                    _ => {
                        if e.is_dir {
                            0
                        } else {
                            e.size
                        }
                    }
                };
                let pending = matches!(e.size_state, SizeState::Calculating);
                treemap::TileInput {
                    row_index: display_idx,
                    name: e.name.as_str(),
                    is_dir: e.is_dir,
                    size,
                    pending,
                }
            })
            .collect();
        let laid = treemap::lay_out(&inputs);
        let tiles: Vec<Tile> = laid
            .iter()
            .map(|c| laid_tile_to_ui(c, &self.selection, &self.filtered))
            .collect();
        self.treemap_model.set_vec(tiles);
        self.treemap_tiles = laid;
    }

    /// Schedule directory-size computation for every directory path
    /// currently referenced by the columns-view cells, plus the root
    /// itself. This is a superset of the normal `spawn_size_jobs`
    /// visible-paths scope because column mode needs sizes for deep
    /// descendants, not just direct children of `current`.
    fn spawn_columns_size_jobs(&self) {
        if self.view_mode != 2 {
            return;
        }
        // Kick off a walk of the columns root; the shared cache will
        // populate entries for every descendant directory encountered,
        // which is exactly what the layout needs.
        //
        // The batched update callback routes through
        // `apply_size_updates` as usual. We reuse the same scope-filter
        // infrastructure but widen the set to include every directory
        // currently referenced by a cell.
        let generation = self.generation;
        let ui = self.ui.clone();
        let engine = self.size_engine.clone();

        let mut visible: FxHashSet<PathBuf> = FxHashSet::default();
        // The root itself settles at the top of the walk; pick it up.
        visible.insert(self.current.clone());
        if let Ok(canon) = std::fs::canonicalize(&self.current) {
            visible.insert(canon);
        }
        // Every directory path referenced by a currently-laid cell.
        for i in 0..self.columns_model.row_count() {
            if let Some(cell) = self.columns_model.row_data(i) {
                if cell.is_dir {
                    let p = PathBuf::from(cell.path.as_str());
                    visible.insert(p.clone());
                    if let Ok(canon) = std::fs::canonicalize(&p) {
                        visible.insert(canon);
                    }
                }
            }
        }
        let visible = Arc::new(visible);

        let pending: Arc<Mutex<Vec<(PathBuf, SizeState, Option<SystemTime>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let has_pending = Arc::new(AtomicBool::new(false));

        let ui_for_cb = ui.clone();
        let visible_cb = visible.clone();
        let pending_cb = pending.clone();
        let has_pending_cb = has_pending.clone();
        let on_progress: crate::dir_size::ProgressFn = Box::new(
            move |path: &Path, state: SizeState, recursive_mtime: Option<SystemTime>| {
                if !visible_cb.contains(path) {
                    return;
                }
                {
                    let mut buf = match pending_cb.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    buf.push((path.to_path_buf(), state, recursive_mtime));
                }
                if has_pending_cb
                    .compare_exchange(
                        false,
                        true,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    let pending_cb2 = pending_cb.clone();
                    let has_pending_cb2 = has_pending_cb.clone();
                    let _ = ui_for_cb.upgrade_in_event_loop(move |_ui| {
                        has_pending_cb2.store(false, Ordering::Release);
                        let drained: Vec<(PathBuf, SizeState, Option<SystemTime>)> = {
                            let mut buf = match pending_cb2.lock() {
                                Ok(g) => g,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            std::mem::take(&mut *buf)
                        };
                        APP_TLS.with(|slot| {
                            let Some(weak) = slot.borrow().clone() else {
                                return;
                            };
                            let Some(app) = weak.upgrade() else { return };
                            if let Ok(mut app) = app.try_borrow_mut() {
                                app.apply_size_updates(generation, drained);
                            }
                        });
                    });
                }
            },
        );
        engine.compute(self.current.clone(), generation, on_progress);
    }

    /// Queue a recursive-size computation for every directory in the current
    /// listing. Cache hits complete synchronously inside `SizeEngine::compute`;
    /// misses spawn onto the shared size thread pool.
    ///
    /// To keep the UI responsive while walking huge trees (HOME has ~630k
    /// subdirs), callbacks are filtered and batched before reaching the
    /// event loop:
    ///
    /// 1. **Scope filter.** The walker emits progress for every subdirectory
    ///    it settles, warming the shared cache. Only paths that are direct
    ///    children of the currently-viewed dir can actually move a visible
    ///    row, so non-matches are dropped on the worker thread.
    /// 2. **Coalescing.** Matches are pushed into a shared `Vec` under a
    ///    mutex. The first push since the last drain posts a single
    ///    `invoke_from_event_loop`; further pushes just append. When the
    ///    callback fires on the main thread it drains the buffer in one go
    ///    and applies all pending `set_row_data` updates.
    ///
    /// Together these turn a `~631k` event-loop dispatch storm into a small
    /// handful per second.
    fn spawn_size_jobs(&self) {
        let generation = self.generation;
        let ui = self.ui.clone();
        let engine = self.size_engine.clone();

        // Build the set of paths that could actually update a visible row.
        // Non-matching progress events (deep descendants, unrelated subtrees)
        // are dropped on the worker thread before ever reaching the event
        // loop. The shared cache in `dir_size` is still populated for every
        // subdir regardless.
        let mut visible: FxHashSet<PathBuf> = FxHashSet::default();
        for entry in &self.entries {
            if entry.is_dir {
                visible.insert(entry.path.clone());
                // Include the canonicalized form as well so worker-side
                // comparison works even when the walker reports via the
                // resolved symlink path.
                if let Ok(canon) = std::fs::canonicalize(&entry.path) {
                    visible.insert(canon);
                }
            }
        }
        let visible = Arc::new(visible);

        // Shared, batched update buffer. `has_pending` gates the posting of
        // exactly one event-loop dispatch per drain cycle.
        let pending: Arc<Mutex<Vec<(PathBuf, SizeState, Option<SystemTime>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let has_pending = Arc::new(AtomicBool::new(false));

        for entry in &self.entries {
            if !entry.is_dir {
                continue;
            }
            // Already settled by the synchronous cache backfill in
            // `refresh()`; re-running the engine would just round-trip
            // the same value through the event loop.
            if matches!(entry.size_state, SizeState::Known(_)) {
                continue;
            }
            let dir = entry.path.clone();
            let ui_for_cb = ui.clone();
            let visible = visible.clone();
            let pending = pending.clone();
            let has_pending = has_pending.clone();
            let on_progress: crate::dir_size::ProgressFn = Box::new(
                move |path: &Path,
                      state: SizeState,
                      recursive_mtime: Option<SystemTime>| {
                    // Scope filter: only paths directly visible in the
                    // current listing can affect a row. Everything else goes
                    // straight to the shared cache and is dropped here.
                    if !visible.contains(path) {
                        return;
                    }
                    // Append to the batch buffer. If we are the first
                    // producer since the last drain, post a single dispatch.
                    {
                        let mut buf = match pending.lock() {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        buf.push((path.to_path_buf(), state, recursive_mtime));
                    }
                    if has_pending
                        .compare_exchange(
                            false,
                            true,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        let pending_cb = pending.clone();
                        let has_pending_cb = has_pending.clone();
                        let _ = ui_for_cb.upgrade_in_event_loop(move |_ui| {
                            // Flip the gate before draining so any new
                            // worker-side pushes that race in will post a
                            // fresh dispatch for the next frame.
                            has_pending_cb.store(false, Ordering::Release);
                            let drained: Vec<(PathBuf, SizeState, Option<SystemTime>)> = {
                                let mut buf = match pending_cb.lock() {
                                    Ok(g) => g,
                                    Err(poisoned) => poisoned.into_inner(),
                                };
                                std::mem::take(&mut *buf)
                            };
                            APP_TLS.with(|slot| {
                                let Some(weak) = slot.borrow().clone() else {
                                    return;
                                };
                                let Some(app) = weak.upgrade() else {
                                    return;
                                };
                                if let Ok(mut app) = app.try_borrow_mut() {
                                    app.apply_size_updates(generation, drained);
                                } else {
                                    log::trace!(
                                        "size update skipped: app already borrowed"
                                    );
                                }
                            });
                        });
                    }
                },
            );
            engine.compute(dir, generation, on_progress);
        }
    }

    /// Apply a batched set of size-state updates from a background worker.
    /// Dropped silently (as a group) if the generation does not match (the
    /// user has navigated away). Individual entries that are no longer in
    /// the current listing are ignored.
    fn apply_size_updates(
        &mut self,
        generation: u64,
        updates: Vec<(PathBuf, SizeState, Option<SystemTime>)>,
    ) {
        if generation != self.generation {
            return;
        }
        for (path, state, recursive_mtime) in updates {
            self.on_size_update_inner(&path, state, recursive_mtime);
        }
        // When sorting by size, the per-row `set_row_data` calls above only
        // update the size *text* on existing rows; they do not reorder the
        // list, so the view stays pinned to the pre-settle (all-zero) order.
        // Re-run the filter+sort pass now that authoritative sizes are in,
        // then rebuild just the items model from the new `filtered` ordering.
        //
        // This is cheap: the size-update callbacks are already coalesced into
        // batches throttled to a handful per second by `spawn_size_jobs`, so
        // at worst we re-sort a few-hundred-entry list a few times a second.
        // For non-size sort columns the order cannot have changed, so we
        // skip this path and only refresh the status text below.
        if matches!(self.sort_col, SortCol::Size) {
            self.rebuild_view();
            self.push_items_model();
        }
        // The batched drain path bypasses `push_ui_state`, so the status bar
        // would otherwise stay stuck on whatever value was computed at
        // navigation time (typically a lower bound with a trailing `+`).
        // Refresh just the status string here so totals converge once all
        // pending sizes arrive, without rebuilding crumbs/tiles/sidebar.
        self.refresh_status_text();
        if self.view_mode == 2 {
            // Any size update can change a cell's area (or materialize
            // a previously-pending cell). Re-run the layout.
            self.recompute_columns();
        } else if self.view_mode == 3 {
            // Same rationale as columns: a size update can change a
            // tile's area or materialize a pending tile.
            self.recompute_treemap();
        }
    }

    /// Slim variant of [`App::push_ui_state`] that only replaces the items
    /// model contents. Used by the size-update drain when re-sorting after a
    /// batch: the crumbs, sidebar, view-mode, and sort indicators are all
    /// unchanged, so rebuilding them would be wasted work and would defeat
    /// the responsiveness batching.
    fn push_items_model(&self) {
        let mut items: Vec<FileItem> = Vec::with_capacity(self.filtered.len());
        for &eidx in self.filtered.iter() {
            let e = &self.entries[eidx];
            items.push(FileItem {
                name: e.name.clone().into(),
                icon: self.icons.for_path(&e.path, e.is_dir),
                is_dir: e.is_dir,
                size_text: e.size_text().into(),
                modified_text: e.modified_text().into(),
                selected: self.selection.contains(&eidx),
                cut: self.cut_paths.contains(&e.path),
                hidden: e.hidden,
            });
        }
        self.items_model.set_vec(items);
    }

    fn on_size_update_inner(
        &mut self,
        path: &Path,
        state: SizeState,
        recursive_mtime: Option<SystemTime>,
    ) {
        // Match by path. Descendants reported by the walker (warming the
        // cache for eventual navigation) won't match, and that's fine.
        let Some(eidx) = self.entries.iter().position(|e| e.path == path) else {
            return;
        };
        let entry = &mut self.entries[eidx];
        if !entry.is_dir {
            return;
        }
        entry.size_state = state;
        // Replace the directory's own mtime with the recursive maximum once
        // the walker knows it. The own-mtime only reflects direct child
        // add/remove, so edits inside descendants would otherwise leave
        // the row showing a misleadingly old time.
        if let Some(t) = recursive_mtime {
            entry.modified = t;
        }

        // Update the corresponding row in the live model, if visible.
        let Some(display_idx) = self.filtered.iter().position(|&i| i == eidx) else {
            return;
        };
        if let Some(mut item) = self.items_model.row_data(display_idx) {
            item.size_text = entry.size_text().into();
            item.modified_text = entry.modified_text().into();
            self.items_model.set_row_data(display_idx, item);
        }
    }

    fn rebuild_view(&mut self) {
        // Apply filter.
        let search_lc = self.search.to_lowercase();
        let show_hidden = self.show_hidden;
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| show_hidden || !e.hidden)
            .filter(|(_, e)| {
                search_lc.is_empty() || e.name.to_lowercase().contains(&search_lc)
            })
            .map(|(i, _)| i)
            .collect();

        // Sort. Delegated to the pure `sort_indices` helper so the ordering
        // rule is unit-testable without a live window.
        self.filtered = sort_indices(
            &self.entries,
            &self.filtered,
            self.sort_col,
            self.sort_asc,
            self.folders_first,
        );

        // Drop selection entries that are no longer visible. Selection
        // is keyed on `entries` indices, so a sort change preserves it
        // verbatim; this only matters when the filter (search /
        // hidden-files toggle) hides a previously-selected file.
        let visible: FxHashSet<usize> = self.filtered.iter().copied().collect();
        self.selection.retain(|eidx| visible.contains(eidx));
        if let Some(e) = self.last_clicked {
            if !visible.contains(&e) {
                self.last_clicked = None;
            }
        }
    }

    fn push_ui_state(&self) {
        let Some(ui) = self.ui.upgrade() else { return };

        let mut items: Vec<FileItem> = Vec::with_capacity(self.filtered.len());
        for &eidx in self.filtered.iter() {
            let e = &self.entries[eidx];
            items.push(FileItem {
                name: e.name.clone().into(),
                icon: self.icons.for_path(&e.path, e.is_dir),
                is_dir: e.is_dir,
                size_text: e.size_text().into(),
                modified_text: e.modified_text().into(),
                selected: self.selection.contains(&eidx),
                cut: self.cut_paths.contains(&e.path),
                hidden: e.hidden,
            });
        }
        // Replace the contents of the existing model in one go. Keeping the
        // same `VecModel` instance means the views' bindings, scroll offset
        // and selection don't churn, and background size workers can
        // continue to mutate individual rows via `set_row_data`.
        self.items_model.set_vec(items);

        // Grid column count is now derived reactively in Slint from the
        // FileGridView's own width, so no tile-rows push is needed here.

        // Crumbs + current path.
        ui.set_crumbs(ModelRc::from(Rc::new(VecModel::from(self.build_crumbs()))));
        ui.set_current_path(self.current.to_string_lossy().to_string().into());

        ui.set_can_back(self.history_i > 0);
        ui.set_can_forward(self.history_i + 1 < self.history.len());
        ui.set_can_up(self.current.parent().is_some());
        // When viewing the trash dir, surface the Trash sidebar entry as
        // active by reporting its sentinel path rather than the real one.
        let active = if self.is_viewing_trash() {
            TRASH_TAG.to_string()
        } else {
            self.current.to_string_lossy().to_string()
        };
        ui.set_sidebar_active_path(active.into());
        ui.set_sort_col(match self.sort_col {
            SortCol::Name => 0,
            SortCol::Modified => 1,
            SortCol::Size => 2,
        });
        ui.set_sort_asc(self.sort_asc);

        ui.set_status_text(self.compute_status_text().into());
        ui.set_device_text(self.compute_device_text().into());
    }

    /// Rebuild just the status-bar string from current state and push it to
    /// the UI. Unlike [`App::push_ui_state`] this touches no other UI models
    /// (no crumbs, tile grid, sidebar path, etc.), so it is safe and cheap to
    /// call from the coalesced size-update drain where rebuilding everything
    /// would defeat the responsiveness batching.
    fn refresh_status_text(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        ui.set_status_text(self.compute_status_text().into());
        ui.set_device_text(self.compute_device_text().into());
    }

    /// Build the right-aligned "N free of M" segment for the status bar.
    ///
    /// Calls `statvfs` synchronously. Single syscall, sub-millisecond on
    /// local mounts. We accept that this can block on an unresponsive NFS
    /// mount as a known tradeoff, since navigation already blocks on scan()
    /// anyway. On any failure (statvfs error, weird path) we return an empty
    /// string; the UI simply shows nothing on the right.
    fn compute_device_text(&self) -> String {
        let Some((avail, total)) = disk::free_and_total(&self.current) else {
            return String::new();
        };
        let avail_s = format_size(avail, BINARY);
        let total_s = format_size(total, BINARY);
        tr_fmt("{} free of {}", &[&avail_s, &total_s])
    }

    /// Compute the status-bar string from the current filtered entries and
    /// selection. Shared between [`App::push_ui_state`] and
    /// [`App::refresh_status_text`].
    fn compute_status_text(&self) -> String {
        let total = self.filtered.len();
        let sel = self.selection.len();

        // Sum the sizes of all visible entries (folder total).
        let visible_entries: Vec<&Entry> =
            self.filtered.iter().map(|&i| &self.entries[i]).collect();
        let (folder_bytes, folder_pending) = fs_scan::total_known_sizes(&visible_entries);
        let folder_size_text = format_total(folder_bytes, folder_pending);

        if sel == 0 {
            tr_n_fmt(
                "{} item, {}",
                "{} items, {}",
                total,
                &[&total, &folder_size_text],
            )
        } else {
            let selected_entries: Vec<&Entry> = self
                .selection
                .iter()
                .filter_map(|&eidx| self.entries.get(eidx))
                .collect();
            let (sel_bytes, sel_pending) = fs_scan::total_known_sizes(&selected_entries);
            let sel_size_text = format_total(sel_bytes, sel_pending);
            tr_fmt(
                "{} of {} selected, {} of {}",
                &[&sel, &total, &sel_size_text, &folder_size_text],
            )
        }
    }

    fn build_crumbs(&self) -> Vec<Crumb> {
        let mut crumbs: Vec<Crumb> = Vec::new();
        let mut parts: Vec<PathBuf> = Vec::new();
        let mut p: Option<&Path> = Some(&self.current);
        while let Some(path) = p {
            parts.push(path.to_path_buf());
            p = path.parent();
        }
        parts.reverse();
        for path in parts {
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| {
                    // Root or empty: show "/" as label on Unix.
                    path.to_string_lossy().to_string()
                });
            crumbs.push(Crumb {
                label: label.into(),
                path: path.to_string_lossy().to_string().into(),
            });
        }
        crumbs
    }

    fn navigate(&mut self, target: PathBuf) {
        if target == self.current {
            return;
        }
        // Truncate forward history.
        self.history.truncate(self.history_i + 1);
        self.history.push(target.clone());
        self.history_i = self.history.len() - 1;
        self.current = target;
        self.search.clear();
        self.selection.clear();
        self.last_clicked = None;
        self.column_selected_path = None;
        self.refresh();
        // `refresh` handles `recompute_columns`, but column-mode also
        // needs size jobs for the new root so deep descendants settle.
        // The treemap is single-level, so the direct-child jobs from
        // `spawn_size_jobs` are enough for it.
        if self.view_mode == 2 {
            self.spawn_columns_size_jobs();
        }
        self.persist();
    }

    fn go_back(&mut self) {
        if self.history_i == 0 {
            return;
        }
        self.history_i -= 1;
        self.current = self.history[self.history_i].clone();
        self.selection.clear();
        self.column_selected_path = None;
        self.refresh();
        if self.view_mode == 2 {
            self.spawn_columns_size_jobs();
        }
        self.persist();
    }

    fn go_forward(&mut self) {
        if self.history_i + 1 >= self.history.len() {
            return;
        }
        self.history_i += 1;
        self.current = self.history[self.history_i].clone();
        self.selection.clear();
        self.column_selected_path = None;
        self.refresh();
        if self.view_mode == 2 {
            self.spawn_columns_size_jobs();
        }
        self.persist();
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.current.parent() {
            // Capture the directory name we are leaving so `refresh`
            // can highlight it in the parent listing once loaded.
            self.select_after_navigate = self
                .current
                .file_name()
                .map(|n| n.to_string_lossy().to_string());
            // `navigate` already calls `persist`, no need to double-save.
            self.navigate(parent.to_path_buf());
        }
    }

    fn select_only(&mut self, display_idx: usize) {
        let Some(&eidx) = self.filtered.get(display_idx) else { return };
        self.selection.clear();
        self.selection.insert(eidx);
        self.last_clicked = Some(eidx);
    }

    fn toggle_selection(&mut self, display_idx: usize) {
        let Some(&eidx) = self.filtered.get(display_idx) else { return };
        if !self.selection.insert(eidx) {
            self.selection.remove(&eidx);
        }
        self.last_clicked = Some(eidx);
    }

    fn range_select(&mut self, display_idx: usize) {
        // Anchor in display-row space: arrow / shift-click pick a
        // contiguous range of *visible* rows, regardless of how those
        // map back to `entries`. Translate the entry-keyed
        // `last_clicked` to a display position for that purpose.
        let anchor_display = self
            .last_clicked
            .and_then(|e| self.filtered.iter().position(|&f| f == e))
            .unwrap_or(display_idx);
        let (lo, hi) = if anchor_display <= display_idx {
            (anchor_display, display_idx)
        } else {
            (display_idx, anchor_display)
        };
        self.selection.clear();
        for d in lo..=hi {
            if let Some(&eidx) = self.filtered.get(d) {
                self.selection.insert(eidx);
            }
        }
    }

    fn click(&mut self, idx: usize, ctrl: bool, shift: bool) {
        if idx >= self.filtered.len() {
            return;
        }
        let prev = self.selection.clone();
        if shift {
            self.range_select(idx);
        } else if ctrl {
            self.toggle_selection(idx);
        } else {
            self.select_only(idx);
        }
        self.refresh_selection_display(&prev);
    }

    /// Update only the rows whose `selected` flag flipped between
    /// `prev` and the current `self.selection`, plus the status-bar
    /// text. Avoids the full `push_ui_state` rebuild so arrow-key
    /// selection changes stay snappy on 500+ entry directories.
    ///
    /// The crumbs, sidebar-active-path, sort indicators, view mode and
    /// clipboard-cut markers are all stable across a pure selection
    /// change, so they are not touched here. If any of those shift
    /// (navigation, sort change, paste, etc.) call `push_ui_state`.
    ///
    /// Writes hit the list/grid's `items_model` and, when the treemap
    /// is showing the same `filtered` rows, its `treemap_model` too —
    /// both views share `self.selection` so both need a refresh. The
    /// treemap's row count may be smaller than `items_model` (zero-
    /// size entries are dropped from the layout), which is why we
    /// iterate the tile model and check its `row_index` rather than
    /// indexing by position.
    fn refresh_selection_display(&self, prev: &FxHashSet<usize>) {
        // Symmetric difference in entry space: which `entries` rows
        // changed selected/unselected status.
        let affected_eidx: FxHashSet<usize> = prev
            .symmetric_difference(&self.selection)
            .copied()
            .collect();
        // Translate to display positions for the items model. Walk
        // `filtered` once instead of doing a position-search per
        // affected entry.
        for (display_idx, &eidx) in self.filtered.iter().enumerate() {
            if !affected_eidx.contains(&eidx) {
                continue;
            }
            if let Some(mut item) = self.items_model.row_data(display_idx) {
                item.selected = self.selection.contains(&eidx);
                self.items_model.set_row_data(display_idx, item);
            }
        }
        // Treemap uses the same `selection` set but its own VecModel.
        // Tile.index carries the display position; map back to eidx
        // through `filtered` to test membership in the entry-keyed set.
        let tile_count = self.treemap_model.row_count();
        for t in 0..tile_count {
            let Some(mut tile) = self.treemap_model.row_data(t) else {
                continue;
            };
            let display_idx = tile.index as usize;
            let Some(&eidx) = self.filtered.get(display_idx) else {
                continue;
            };
            if !affected_eidx.contains(&eidx) {
                continue;
            }
            let now_selected = self.selection.contains(&eidx);
            if tile.selected != now_selected {
                tile.selected = now_selected;
                self.treemap_model.set_row_data(t, tile);
            }
        }
        self.refresh_status_text();
    }

    /// Column-view equivalent of [`App::refresh_selection_display`]:
    /// flip the `selected` flag on only the rows whose path matches
    /// the previous or new selection. Mirrors the list/grid pattern so
    /// the Slint `TouchArea` on the clicked cell survives between the
    /// two presses of a double-click gesture. A full
    /// `recompute_columns` rebuild would tear down and re-instantiate
    /// the `TouchArea` under the pointer and drop the pending double-
    /// click state.
    ///
    /// Uses `set_row_data` against the persistent `columns_model`
    /// established in [`App::new`]. Status text is not refreshed here
    /// because `compute_status_text` does not depend on
    /// `column_selected_path`.
    fn refresh_column_selection_display(&self, prev: Option<&Path>) {
        let new = self.column_selected_path.as_deref();
        if prev == new {
            return;
        }
        let row_count = self.columns_model.row_count();
        for i in 0..row_count {
            let Some(mut cell) = self.columns_model.row_data(i) else {
                continue;
            };
            let matches_prev = prev
                .map(|p| cell.path.as_str() == p.to_string_lossy())
                .unwrap_or(false);
            let matches_new = new
                .map(|p| cell.path.as_str() == p.to_string_lossy())
                .unwrap_or(false);
            if matches_prev && !matches_new && cell.selected {
                cell.selected = false;
                self.columns_model.set_row_data(i, cell);
            } else if matches_new && !cell.selected {
                cell.selected = true;
                self.columns_model.set_row_data(i, cell);
            }
        }
    }

    fn double_click(&mut self, idx: usize) {
        if idx >= self.filtered.len() {
            return;
        }
        let eidx = self.filtered[idx];
        let entry = self.entries[eidx].clone();
        if entry.is_dir {
            self.navigate(entry.path);
        } else {
            if let Err(e) = open::that_detached(&entry.path) {
                log::warn!("open failed: {}", e);
            }
        }
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        // In column mode, the "selection" is the single path the user
        // last clicked in the icicle view — context-menu actions
        // target that one path. Every other view shares the top-level
        // `selection` set (indices into `entries`).
        if self.view_mode == 2 {
            return self.column_selected_path.iter().cloned().collect();
        }
        self.selection
            .iter()
            .filter_map(|&eidx| self.entries.get(eidx))
            .map(|e| e.path.clone())
            .collect()
    }

    // === Context menu actions ===

    fn ctx_open(&mut self) {
        if self.view_mode == 2 {
            if let Some(path) = self.column_selected_path.clone() {
                let is_dir = path.is_dir();
                // Root detection: column-selected path equals the
                // current directory. In that case Open behaves as
                // zoom-out just like a double-click on col-0.
                let is_root = path == self.current;
                self.on_column_cell_double_click(path, is_dir, is_root);
            }
            return;
        }
        // List, grid and treemap all share `last_clicked`. Translate
        // back to a display position for `double_click`, which still
        // takes display indices since that's what the click callbacks
        // hand it.
        if let Some(eidx) = self.last_clicked {
            if let Some(display_idx) = self.filtered.iter().position(|&e| e == eidx) {
                self.double_click(display_idx);
            }
        }
    }

    fn ctx_copy_path(&self) {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let paths = self.selected_paths();
            let joined = paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let _ = cb.set_text(joined);
        }
    }

    fn ctx_copy(&mut self) {
        self.cut_paths.clear();
        self.clipboard_op = Some(ClipOp::Copy);
        self.push_clipboard_to_system();
        self.push_ui_state();
    }

    fn ctx_cut(&mut self) {
        self.cut_paths = self.selected_paths().into_iter().collect();
        self.clipboard_op = Some(ClipOp::Cut);
        self.push_clipboard_to_system();
        self.push_ui_state();
    }

    fn push_clipboard_to_system(&self) {
        // Write a text fallback. Full x-special/gnome-copied-files clipboard
        // support requires raw wayland/X11 mime handling.
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let paths = self.selected_paths();
            let joined = paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let _ = cb.set_text(joined);
        }
    }

    fn ctx_paste(&mut self) {
        let Some(op) = self.clipboard_op else { return };
        let dest = self.current.clone();
        let sources: Vec<PathBuf> = self.cut_paths.iter().cloned().collect();
        let sources = if matches!(op, ClipOp::Copy) {
            self.selected_paths() // fallback: copies current selection
        } else {
            sources
        };
        for src in sources {
            let Some(fname) = src.file_name() else { continue };
            let target = unique_name(&dest, fname.to_string_lossy().as_ref());
            let r = match op {
                ClipOp::Copy => copy_recursive(&src, &target),
                ClipOp::Cut => std::fs::rename(&src, &target),
            };
            if let Err(e) = r {
                log::warn!("paste {:?} → {:?} failed: {}", src, target, e);
            }
        }
        if matches!(op, ClipOp::Cut) {
            self.cut_paths.clear();
            self.clipboard_op = None;
        }
        self.refresh();
    }

    fn ctx_rename(&mut self) {
        let Some(path) = self.selected_paths().into_iter().next() else { return };
        let Some(ui) = self.ui.upgrade() else { return };
        self.pending_rename = Some(path.clone());
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        ui.set_rename_initial(name.into());
        ui.set_rename_shown(true);
    }

    /// True when the user is currently browsing the trash directory itself.
    /// Actions that would move selected items into the trash don't make sense
    /// here (the `trash` crate would just rename them with a `.N` suffix).
    fn is_viewing_trash(&self) -> bool {
        dirs::home_dir()
            .map(|h| self.current == h.join(".local/share/Trash/files"))
            .unwrap_or(false)
    }

    fn ctx_delete_to_trash(&mut self) {
        // Triggered by both the context menu and the Delete key. The menu
        // entry is disabled when viewing trash, but the shortcut still fires;
        // guard here so both paths no-op.
        if self.is_viewing_trash() {
            return;
        }
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        self.pending_confirm = Some(ConfirmAction::DeleteToTrash(paths.clone()));
        let n = paths.len();
        ui.set_confirm_title(tr("Move to trash?").into());
        ui.set_confirm_body(
            tr_n_fmt(
                "Move {} item to the trash?",
                "Move {} items to the trash?",
                n,
                &[&n],
            )
            .into(),
        );
        ui.set_confirm_label(tr("Move to trash").into());
        ui.set_confirm_danger(false);
        ui.set_confirm_shown(true);
    }

    fn ctx_permanent_delete(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        self.pending_confirm = Some(ConfirmAction::PermanentDelete(paths.clone()));
        let n = paths.len();
        ui.set_confirm_title(tr("Permanently delete?").into());
        ui.set_confirm_body(
            tr_n_fmt(
                "This will permanently delete {} item. This cannot be undone.",
                "This will permanently delete {} items. This cannot be undone.",
                n,
                &[&n],
            )
            .into(),
        );
        ui.set_confirm_label(tr("Delete").into());
        ui.set_confirm_danger(true);
        ui.set_confirm_shown(true);
    }

    /// Column view: single left-click on a cell selects it (highlights
    /// only, no navigation).
    ///
    /// Uses per-row `set_row_data` instead of rebuilding the whole
    /// columns model. A full rebuild tears down the Slint `TouchArea`
    /// on the clicked cell between the first and second press of a
    /// double-click, so Slint's double-click state machine never fires.
    /// Keeping the existing `VecModel` rows alive preserves the
    /// `TouchArea` instances and lets double-clicks register.
    fn on_column_cell_click(&mut self, path: PathBuf) {
        let prev = self.column_selected_path.take();
        self.column_selected_path = Some(path);
        self.refresh_column_selection_display(prev.as_deref());
    }

    /// Column view: double left-click. Directories zoom in (navigate
    /// into the subtree); the root cell zooms out (navigate to the
    /// current directory's parent). Files fall through to the OS
    /// default-handler, matching list/grid semantics.
    fn on_column_cell_double_click(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        is_root: bool,
    ) {
        if is_root {
            if let Some(parent) = self.current.parent() {
                self.navigate(parent.to_path_buf());
            }
            return;
        }
        if is_dir {
            self.navigate(path);
        } else if let Err(e) = open::that_detached(&path) {
            log::warn!("open failed: {}", e);
        }
    }

    /// Column view: right-click on a cell opens the same item-context
    /// menu as list/grid. The clicked path becomes the column
    /// selection; the menu's actions will operate on that path via
    /// [`App::selected_paths`] in column mode.
    fn on_column_cell_right_click(&mut self, path: PathBuf, x: f32, y: f32) {
        let prev = self.column_selected_path.take();
        self.column_selected_path = Some(path);
        self.refresh_column_selection_display(prev.as_deref());
        // Reuse the list/grid menu-build path. No duplication.
        let Some(ui) = self.ui.upgrade() else { return };
        let entries = item_menu(self);
        ui.set_context_entries(ModelRc::from(Rc::new(VecModel::from(entries))));
        ui.set_context_x(x);
        ui.set_context_y(y);
        ui.set_context_visible(true);
    }

    fn apply_confirm(&mut self) {
        let action = self.pending_confirm.take();
        match action {
            Some(ConfirmAction::DeleteToTrash(paths)) => {
                for p in &paths {
                    if let Err(e) = trash::delete(p) {
                        log::warn!("trash {:?}: {}", p, e);
                    }
                }
                self.refresh();
            }
            Some(ConfirmAction::PermanentDelete(paths)) => {
                for p in &paths {
                    let r = if p.is_dir() {
                        std::fs::remove_dir_all(p)
                    } else {
                        std::fs::remove_file(p)
                    };
                    if let Err(e) = r {
                        log::warn!("delete {:?}: {}", p, e);
                    }
                }
                self.refresh();
            }
            None => {}
        }
    }

    fn submit_rename_dialog(&mut self, new_name: String) {
        let Some(old) = self.pending_rename.take() else { return };
        let Some(parent) = old.parent() else { return };
        let new_path = parent.join(&new_name);
        if new_path == old {
            return;
        }
        if let Err(e) = std::fs::rename(&old, &new_path) {
            log::warn!("rename {:?} → {:?}: {}", old, new_path, e);
        }
        self.refresh();
    }

    fn new_folder(&mut self, name: String) {
        let target = unique_name(&self.current, &name);
        if let Err(e) = std::fs::create_dir(&target) {
            log::warn!("mkdir {:?}: {}", target, e);
        }
        self.refresh();
    }

    fn open_new_folder_dialog(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        ui.set_new_folder_shown(true);
    }

    fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.rebuild_view();
        // Column and treemap views do not consume `filtered`; they
        // read the disk directly. Recompute their layout so the toggle
        // is reflected live without needing to leave and re-enter.
        if self.view_mode == 2 {
            self.recompute_columns();
        } else if self.view_mode == 3 {
            self.recompute_treemap();
        }
        self.push_ui_state();
        self.persist();
    }

    fn set_sort(&mut self, col: i32) {
        let new_col = SortCol::from_int(col);
        if matches!(
            (self.sort_col, new_col),
            (SortCol::Name, SortCol::Name)
                | (SortCol::Modified, SortCol::Modified)
                | (SortCol::Size, SortCol::Size)
        ) {
            self.sort_asc = !self.sort_asc;
        } else {
            self.sort_col = new_col;
            self.sort_asc = true;
        }
        self.rebuild_view();
        self.push_ui_state();
        // Treemap tiles carry `row_index` = display_idx into `filtered`;
        // resorting permutes `filtered`, so the mapping changes even
        // though the underlying set does not. Re-lay so tile indices
        // stay in sync with the list/grid's `selection`.
        if self.view_mode == 3 {
            self.recompute_treemap();
        }
        self.persist();
    }

    fn set_view_mode(&mut self, mode: i32) {
        self.view_mode = mode;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_view_mode(mode);
            // re-push to recompute grid rows for new mode width assumptions
        }
        if mode == 2 {
            // Column view is rooted at `self.current`, same as list/grid.
            // No separate root to seed; just lay out and warm sizes.
            self.recompute_columns();
            self.spawn_columns_size_jobs();
        } else if mode == 3 {
            // Treemap is rooted at `self.current` too. The direct-child
            // size jobs spawned by `refresh`/`navigate` already cover
            // what the single-level layout needs; just lay out.
            self.recompute_treemap();
        }
        self.persist();
    }

    fn handle_key(&mut self, text: SharedString, shift: bool) -> bool {
        let t = text.as_str();
        // Non-navigation shortcuts first: these are independent of the
        // current focus index.
        if t == slint::SharedString::from(slint::platform::Key::Backspace).as_str() {
            self.go_up();
            return true;
        }
        if t == slint::SharedString::from(slint::platform::Key::Delete).as_str() {
            self.ctx_delete_to_trash();
            return true;
        }
        if t == slint::SharedString::from(slint::platform::Key::F2).as_str() {
            self.ctx_rename();
            return true;
        }
        if t == slint::SharedString::from(slint::platform::Key::F5).as_str() {
            self.refresh();
            return true;
        }
        if t == slint::SharedString::from(slint::platform::Key::Return).as_str() {
            self.ctx_open();
            return true;
        }

        // Column view: separate navigation. Uses the `(col, y_start,
        // y_end)` geometry from the cached layout rather than flat row
        // indices.
        if self.view_mode == 2 {
            return self.handle_key_columns(t);
        }
        // Treemap view: separate 2D nearest-neighbor navigation.
        if self.view_mode == 3 {
            return self.handle_key_treemap(t);
        }
        // Arrow / Home / End / PageUp / PageDown move the focus row inside
        // the list or grid. Not handled in column/treemap views.
        if self.view_mode != 0 && self.view_mode != 1 {
            return false;
        }
        let n = self.filtered.len();
        if n == 0 {
            return false;
        }
        // Current focus, in display-row space: prefer the explicit
        // anchor, fall back to the topmost selected row, else to 0.
        // Both `last_clicked` and `selection` are entry-keyed, so
        // translate through `filtered`.
        let cur = self
            .last_clicked
            .and_then(|e| self.filtered.iter().position(|&f| f == e))
            .or_else(|| {
                self.selection
                    .iter()
                    .filter_map(|&e| self.filtered.iter().position(|&f| f == e))
                    .min()
            })
            .unwrap_or(0)
            .min(n - 1);

        // Grid column count: 1 in list view, computed from the window size
        // in grid view. We used to forward a reactive `cols` from the Slint
        // side via an `out property` and a `changed cols` callback, but
        // that wiring (FileGridView.self.width -> cols -> MainWindow.grid-cols)
        // formed a binding cycle that Slint flagged at runtime with
        // "Recursion detected" on first layout when view-mode is Grid.
        // Instead compute the same formula used in file_grid.slint locally:
        // cols = max(1, floor((grid_width - 2 * h_padding) / tile_w)).
        let cols = if self.view_mode == 1 {
            self.ui
                .upgrade()
                .map(|ui| {
                    let tile_w = 110.0_f32;
                    let h_padding = 8.0_f32;
                    let sidebar_w = 224.0_f32;
                    let divider_w = 1.0_f32;
                    let scale = ui.window().scale_factor();
                    let window_w = ui.window().size().width as f32 / scale.max(1e-3);
                    let grid_w = (window_w - sidebar_w - divider_w).max(tile_w);
                    let c = ((grid_w - 2.0 * h_padding) / tile_w).floor() as i32;
                    c.max(1) as usize
                })
                .unwrap_or(1)
        } else {
            1
        };

        let new_idx: Option<usize> =
            if t == slint::SharedString::from(slint::platform::Key::UpArrow).as_str() {
                let step = cols;
                if cur >= step { Some(cur - step) } else { Some(0) }
            } else if t == slint::SharedString::from(slint::platform::Key::DownArrow).as_str() {
                Some((cur + cols).min(n - 1))
            } else if t == slint::SharedString::from(slint::platform::Key::LeftArrow).as_str() {
                // In list view left/right are no-ops.
                if self.view_mode == 1 {
                    Some(cur.saturating_sub(1))
                } else {
                    return false;
                }
            } else if t == slint::SharedString::from(slint::platform::Key::RightArrow).as_str() {
                if self.view_mode == 1 {
                    Some((cur + 1).min(n - 1))
                } else {
                    return false;
                }
            } else if t == slint::SharedString::from(slint::platform::Key::Home).as_str() {
                Some(0)
            } else if t == slint::SharedString::from(slint::platform::Key::End).as_str() {
                Some(n - 1)
            } else if t == slint::SharedString::from(slint::platform::Key::PageUp).as_str() {
                Some(cur.saturating_sub(10))
            } else if t == slint::SharedString::from(slint::platform::Key::PageDown).as_str() {
                Some((cur + 10).min(n - 1))
            } else {
                None
            };

        match new_idx {
            Some(i) => {
                // Reuse the click pipeline so shift-range, anchor update and
                // UI push all go through the already-tested path.
                self.click(i, false, shift);
                true
            }
            None => false,
        }
    }

    /// Arrow-key navigation inside the column (icicle) view. Uses the
    /// cached `column_cells` layout: each cell carries a `col` and a
    /// `[y_start, y_end]` fractional range, and moves are phrased in
    /// those coordinates rather than flat row indices.
    ///
    /// If there is no current selection, the first arrow press selects
    /// the root (col 0) cell. If the selected path no longer resolves
    /// to a visible cell (e.g. after a re-layout), we also fall back to
    /// the root.
    fn handle_key_columns(&mut self, t: &str) -> bool {
        let is_up = t == slint::SharedString::from(slint::platform::Key::UpArrow).as_str();
        let is_down =
            t == slint::SharedString::from(slint::platform::Key::DownArrow).as_str();
        let is_left =
            t == slint::SharedString::from(slint::platform::Key::LeftArrow).as_str();
        let is_right =
            t == slint::SharedString::from(slint::platform::Key::RightArrow).as_str();
        if !(is_up || is_down || is_left || is_right) {
            return false;
        }
        if self.column_cells.is_empty() {
            return false;
        }

        // Locate the currently-selected cell. Missing selection or
        // stale path both fall back to the root (col 0).
        let cur_idx: Option<usize> = self.column_selected_path.as_ref().and_then(|p| {
            self.column_cells
                .iter()
                .position(|c| c.path.as_path() == p.as_path())
        });
        let cur_idx = match cur_idx {
            Some(i) => i,
            None => {
                // Seed to root and stop; one press = "start here".
                let root = &self.column_cells[0];
                let prev = self.column_selected_path.take();
                self.column_selected_path = Some(root.path.clone());
                self.refresh_column_selection_display(prev.as_deref());
                return true;
            }
        };

        let cur = &self.column_cells[cur_idx];
        let cur_col = cur.col;
        let cur_y_start = cur.y_start;
        let cur_y_end = cur.y_end;
        let cur_mid = (cur_y_start + cur_y_end) * 0.5;

        // Small epsilon to tolerate float equality on boundary matches.
        const EPS: f32 = 1e-4;

        let target: Option<usize> = if is_down {
            // Smallest y_start in the same column that is >= cur_y_end.
            self.column_cells
                .iter()
                .enumerate()
                .filter(|(_, c)| c.col == cur_col && c.y_start + EPS >= cur_y_end)
                .min_by(|(_, a), (_, b)| {
                    a.y_start.partial_cmp(&b.y_start).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _)| i)
        } else if is_up {
            // Largest y_end in the same column that is <= cur_y_start.
            self.column_cells
                .iter()
                .enumerate()
                .filter(|(_, c)| c.col == cur_col && c.y_end <= cur_y_start + EPS)
                .max_by(|(_, a), (_, b)| {
                    a.y_end.partial_cmp(&b.y_end).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _)| i)
        } else if is_left {
            if cur_col == 0 {
                None
            } else {
                let parent_col = cur_col - 1;
                self.column_cells
                    .iter()
                    .enumerate()
                    .find(|(_, c)| {
                        c.col == parent_col
                            && c.y_start - EPS <= cur_mid
                            && cur_mid <= c.y_end + EPS
                    })
                    .map(|(i, _)| i)
            }
        } else {
            // is_right: first cell in `cur_col + 1` whose y_start >=
            // cur_y_start and which falls inside `[cur_y_start,
            // cur_y_end]`.
            let next_col = cur_col + 1;
            self.column_cells
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.col == next_col
                        && c.y_start + EPS >= cur_y_start
                        && c.y_start <= cur_y_end + EPS
                })
                .min_by(|(_, a), (_, b)| {
                    a.y_start.partial_cmp(&b.y_start).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _)| i)
        };

        if let Some(i) = target {
            let new_path = self.column_cells[i].path.clone();
            if self.column_selected_path.as_deref() != Some(new_path.as_path()) {
                let prev = self.column_selected_path.take();
                self.column_selected_path = Some(new_path);
                self.refresh_column_selection_display(prev.as_deref());
            }
        }
        // Consume the key either way so the list/grid fallthrough does
        // not interpret it.
        true
    }

    /// Arrow-key navigation inside the treemap view. Picks the nearest
    /// neighbor tile in the pressed direction, using tile centers and
    /// a directional filter so moves feel spatial rather than
    /// list-linear. The chosen tile becomes the sole selection (same
    /// as a plain click), going through [`App::click`] so the list,
    /// grid and treemap all stay in sync on `selection` and
    /// `last_clicked`.
    ///
    /// If there is no current selection, the first arrow press selects
    /// the largest visible tile. If `last_clicked` points at a row that
    /// no longer has a tile (its size went to zero), we also fall back
    /// to the largest.
    fn handle_key_treemap(&mut self, t: &str) -> bool {
        let is_up = t == slint::SharedString::from(slint::platform::Key::UpArrow).as_str();
        let is_down =
            t == slint::SharedString::from(slint::platform::Key::DownArrow).as_str();
        let is_left =
            t == slint::SharedString::from(slint::platform::Key::LeftArrow).as_str();
        let is_right =
            t == slint::SharedString::from(slint::platform::Key::RightArrow).as_str();
        if !(is_up || is_down || is_left || is_right) {
            return false;
        }
        if self.treemap_tiles.is_empty() {
            return false;
        }

        // Find the tile corresponding to the anchor (`last_clicked`),
        // or fall through to the largest tile if there is no anchor
        // or the anchor's row has no tile (e.g. zero-size dir). Tiles
        // carry a display position (`row_index`) but `last_clicked` is
        // entry-keyed, so translate first.
        let anchor_display = self
            .last_clicked
            .and_then(|e| self.filtered.iter().position(|&f| f == e));
        let cur_tile_idx: Option<usize> = anchor_display
            .and_then(|a| self.treemap_tiles.iter().position(|c| c.row_index == a));
        let cur_tile_idx = match cur_tile_idx {
            Some(i) => i,
            None => {
                // Seed to the largest tile's row and stop: "one press
                // lands here, the next one actually moves".
                let first_row = self.treemap_tiles[0].row_index;
                self.click(first_row, /* ctrl */ false, /* shift */ false);
                return true;
            }
        };

        let cur = &self.treemap_tiles[cur_tile_idx];
        let cx = cur.x + cur.w * 0.5;
        let cy = cur.y + cur.h * 0.5;

        // Directional nearest-neighbor: keep candidates whose center
        // lies in the correct half-plane, then pick by Euclidean
        // distance to the current tile's center.
        let target: Option<usize> = self
            .treemap_tiles
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != cur_tile_idx)
            .filter(|(_, c)| {
                let nx = c.x + c.w * 0.5;
                let ny = c.y + c.h * 0.5;
                if is_up {
                    ny < cy
                } else if is_down {
                    ny > cy
                } else if is_left {
                    nx < cx
                } else {
                    nx > cx
                }
            })
            .min_by(|(_, a), (_, b)| {
                let ax = a.x + a.w * 0.5;
                let ay = a.y + a.h * 0.5;
                let bx = b.x + b.w * 0.5;
                let by = b.y + b.h * 0.5;
                let da = (ax - cx).powi(2) + (ay - cy).powi(2);
                let db = (bx - cx).powi(2) + (by - cy).powi(2);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i);

        if let Some(i) = target {
            let new_row = self.treemap_tiles[i].row_index;
            let new_eidx = self.filtered.get(new_row).copied();
            if self.last_clicked != new_eidx {
                self.click(new_row, /* ctrl */ false, /* shift */ false);
            }
        }
        true
    }
}

fn lexical(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// Pure sort step used by `App::rebuild_view`. Takes a slice of `Entry` and a
/// pre-filtered set of indices into it, returns those indices reordered
/// according to the requested column and direction.
///
/// Split out of the method body so the ordering rule can be unit-tested
/// without a live Slint window. The method just wires up its own state and
/// calls this.
///
/// # Size sort and unknown sizes
///
/// The tricky case is `SortCol::Size` on a cold directory: background size
/// walkers populate `Entry::size_state` asynchronously, so at first paint
/// every directory is `Calculating` with `effective_size() == 0`. Comparing
/// those zeros gives read-directory order, which looks unsorted to the user.
/// We treat `Calculating` and `Unknown` as "smaller than anything known" so
/// unknowns consistently cluster at one end (bottom under descending, top
/// under ascending), and fall back to alphabetical on ties so the pre-settle
/// order at least reads top-to-bottom alphabetically rather than randomly.
pub fn sort_indices(
    entries: &[Entry],
    filtered: &[usize],
    sort_col: SortCol,
    sort_asc: bool,
    folders_first: bool,
) -> Vec<usize> {
    let mut out: Vec<usize> = filtered.to_vec();
    // Folders-first grouping only makes sense when sorting alphabetically by
    // Name; for Modified/Size the user expects interleaved rows in pure
    // column order.
    let group_dirs = folders_first && matches!(sort_col, SortCol::Name);
    out.sort_by(|a, b| {
        let ea = &entries[*a];
        let eb = &entries[*b];
        if group_dirs && ea.is_dir != eb.is_dir {
            return if ea.is_dir {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        let ord = match sort_col {
            SortCol::Name => lexical(&ea.name, &eb.name),
            SortCol::Modified => ea.modified.cmp(&eb.modified),
            SortCol::Size => size_cmp(ea, eb),
        };
        if sort_asc { ord } else { ord.reverse() }
    });
    out
}

/// Compare two entries by size for `SortCol::Size`. Known sizes compare by
/// their numeric value. `Calculating` and `Unknown` are treated as strictly
/// smaller than any `Known` value, and equal to each other; ties (including
/// two unknowns) fall back to lexical name order.
///
/// The same rule is applied before the caller's optional `reverse()` so the
/// alphabetical fallback is consistent regardless of `sort_asc`. The direction
/// flip is applied by the caller.
fn size_cmp(ea: &Entry, eb: &Entry) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let a_known = matches!(ea.size_state, SizeState::Known(_));
    let b_known = matches!(eb.size_state, SizeState::Known(_));
    let ord = match (a_known, b_known) {
        (true, true) => ea.effective_size().cmp(&eb.effective_size()),
        (true, false) => Ordering::Greater, // known > unknown
        (false, true) => Ordering::Less,    // unknown < known
        (false, false) => Ordering::Equal,
    };
    if ord == Ordering::Equal {
        lexical(&ea.name, &eb.name)
    } else {
        ord
    }
}

/// Map the on-disk `config::SortCol` into the runtime `fs_scan::SortCol`.
fn sort_col_from_config(c: config::SortCol) -> SortCol {
    match c {
        config::SortCol::Name => SortCol::Name,
        config::SortCol::Modified => SortCol::Modified,
        config::SortCol::Size => SortCol::Size,
    }
}

/// Map the runtime `fs_scan::SortCol` back to the on-disk `config::SortCol`.
fn sort_col_to_config(c: SortCol) -> config::SortCol {
    match c {
        SortCol::Name => config::SortCol::Name,
        SortCol::Modified => config::SortCol::Modified,
        SortCol::Size => config::SortCol::Size,
    }
}

/// Map the on-disk `config::ViewMode` into the UI integer (0 = list,
/// 1 = grid, 2 = columns, 3 = treemap).
fn view_mode_from_config(v: config::ViewMode) -> i32 {
    match v {
        config::ViewMode::List => 0,
        config::ViewMode::Grid => 1,
        config::ViewMode::Columns => 2,
        config::ViewMode::Treemap => 3,
    }
}

/// Map the UI integer back to `config::ViewMode`. Unknown integers fall
/// back to `List` so a corrupt/partial state never surfaces as a panic.
fn view_mode_to_config(m: i32) -> config::ViewMode {
    match m {
        1 => config::ViewMode::Grid,
        2 => config::ViewMode::Columns,
        3 => config::ViewMode::Treemap,
        _ => config::ViewMode::List,
    }
}

/// Format a byte total for the status bar. When `pending` is true, some
/// directory sizes are still being computed (or were unreadable), so append a
/// `+` to signal that the displayed number is a lower bound and may grow.
fn format_total(bytes: u64, pending: bool) -> String {
    let base = format_size(bytes, BINARY);
    if pending { format!("{}+", base) } else { base }
}

/// Convert a `columns::LaidCell` into the Slint-facing `ColumnCell`
/// struct. `y_start`/`y_end` are fractions in `[0.0, 1.0]`; the Slint
/// side multiplies by the live column height to produce pixels.
/// `selected` is set when `c.path` matches the controller's currently
/// column-selected path.
fn laid_cell_to_ui(c: &LaidCell, selected: Option<&Path>) -> ColumnCell {
    let is_selected = selected.map(|p| p == c.path.as_path()).unwrap_or(false);
    ColumnCell {
        name: c.name.clone().into(),
        size_text: c.size_text.clone().into(),
        col: c.col as i32,
        y_start: c.y_start,
        y_end: c.y_end,
        is_dir: c.is_dir,
        pending: c.pending,
        path: c.path.to_string_lossy().to_string().into(),
        is_root: c.is_root,
        selected: is_selected,
    }
}

/// Convert a `treemap::Tile` into the Slint-facing `Tile` struct.
/// `x`/`y`/`w`/`h` are fractions in `[0.0, 1.0]`; the Slint side
/// multiplies by the live treemap width/height to produce pixels.
/// `selected` is set when the tile's underlying `entries` row is in
/// the shared `selection` set, so ticking a tile in list/grid
/// highlights the matching tile in the treemap (and vice-versa).
fn laid_tile_to_ui(c: &LaidTile, selection: &FxHashSet<usize>, filtered: &[usize]) -> Tile {
    let eidx = filtered.get(c.row_index).copied();
    let selected = eidx.map(|e| selection.contains(&e)).unwrap_or(false);
    Tile {
        index: c.row_index as i32,
        name: c.name.clone().into(),
        size_text: c.size_text.clone().into(),
        x: c.x,
        y: c.y,
        w: c.w,
        h: c.h,
        is_dir: c.is_dir,
        pending: c.pending,
        selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_col_round_trip() {
        for c in [
            config::SortCol::Name,
            config::SortCol::Modified,
            config::SortCol::Size,
        ] {
            assert_eq!(sort_col_to_config(sort_col_from_config(c)), c);
        }
    }

    #[test]
    fn view_mode_round_trip() {
        for v in [
            config::ViewMode::List,
            config::ViewMode::Grid,
            config::ViewMode::Columns,
            config::ViewMode::Treemap,
        ] {
            assert_eq!(view_mode_to_config(view_mode_from_config(v)), v);
        }
        // Integer side: 0 is List, 1 is Grid, 2 is Columns, 3 is
        // Treemap; anything outside that range clamps to List. We
        // match `ui/main.slint` where `view-mode` is an int with
        // 0 = list, 1 = grid, 2 = columns, 3 = treemap.
        assert_eq!(view_mode_to_config(0), config::ViewMode::List);
        assert_eq!(view_mode_to_config(1), config::ViewMode::Grid);
        assert_eq!(view_mode_to_config(2), config::ViewMode::Columns);
        assert_eq!(view_mode_to_config(3), config::ViewMode::Treemap);
        assert_eq!(view_mode_to_config(-1), config::ViewMode::List);
        assert_eq!(view_mode_to_config(99), config::ViewMode::List);
    }

    #[test]
    fn config_defaults_map_to_expected_runtime_values() {
        let cfg = Config::default();
        assert!(matches!(sort_col_from_config(cfg.sort_col), SortCol::Name));
        assert_eq!(view_mode_from_config(cfg.view_mode), 0);
        assert!(cfg.sort_asc);
        assert!(!cfg.show_hidden);
        assert!(cfg.folders_first);
    }

    /// Build a synthetic entry for sort tests. Directories get a
    /// `size_state` argument (since that is what the sort comparator actually
    /// inspects for dirs), files are always `Known(size)`.
    fn make_entry(name: &str, is_dir: bool, size: u64, state: SizeState) -> Entry {
        Entry {
            name: name.to_string(),
            path: PathBuf::from(name),
            is_dir,
            size: if is_dir { 0 } else { size },
            size_state: if is_dir { state } else { SizeState::Known(size) },
            modified: std::time::SystemTime::UNIX_EPOCH,
            hidden: false,
        }
    }

    fn names(entries: &[Entry], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| entries[i].name.clone()).collect()
    }

    /// Sort by Size, mixture of files and dirs with known and unknown sizes.
    /// Even with `folders_first = true`, sorting by Size interleaves files
    /// and directories in pure size order; the folders-first grouping is a
    /// Name-column-only rule. Unknown sizes sink below known ones in
    /// descending order.
    #[test]
    fn sort_indices_by_size_mixed_known_and_unknown() {
        let entries = vec![
            make_entry("zdir-unknown", true, 0, SizeState::Calculating),
            make_entry("adir-big", true, 500, SizeState::Known(500)),
            make_entry("bfile-small", false, 10, SizeState::Known(10)),
            make_entry("cfile-big", false, 100, SizeState::Known(100)),
            make_entry("adir-small", true, 50, SizeState::Known(50)),
        ];
        let filtered: Vec<usize> = (0..entries.len()).collect();

        // Descending size, folders_first = true.
        let out = sort_indices(&entries, &filtered, SortCol::Size, false, true);
        let got = names(&entries, &out);
        assert_eq!(
            got,
            vec![
                "adir-big",      // 500, known, dir
                "cfile-big",     // 100, known, file
                "adir-small",    // 50, known, dir
                "bfile-small",   // 10, known, file
                "zdir-unknown",  // unknown (sinks below known in desc)
            ],
        );
    }

    /// Folders-first grouping still applies when sorting by Name. With
    /// `folders_first = true` and ascending Name, directories come before
    /// files even when that conflicts with pure alphabetical order.
    #[test]
    fn sort_indices_by_name_groups_folders_first() {
        let entries = vec![
            make_entry("banana", false, 10, SizeState::Known(10)),
            make_entry("cherry", true, 0, SizeState::Known(0)),
            make_entry("apple", false, 20, SizeState::Known(20)),
            make_entry("date", true, 0, SizeState::Known(0)),
        ];
        let filtered: Vec<usize> = (0..entries.len()).collect();

        let out = sort_indices(&entries, &filtered, SortCol::Name, true, true);
        assert_eq!(
            names(&entries, &out),
            vec!["cherry", "date", "apple", "banana"],
        );
    }

    /// When sorting by Modified, files and directories interleave by the
    /// modified timestamp regardless of `folders_first`.
    #[test]
    fn sort_indices_by_modified_interleaves_dirs_and_files() {
        use std::time::{Duration, SystemTime};
        let base = SystemTime::UNIX_EPOCH;
        let mut mk = |name: &str, is_dir: bool, secs: u64| {
            let mut e = make_entry(name, is_dir, 0, SizeState::Known(0));
            e.modified = base + Duration::from_secs(secs);
            e
        };
        let entries = vec![
            mk("dir-old", true, 10),
            mk("file-new", false, 40),
            mk("dir-new", true, 30),
            mk("file-old", false, 20),
        ];
        let filtered: Vec<usize> = (0..entries.len()).collect();

        // Ascending, folders_first = true: pure chronological order.
        let asc = sort_indices(&entries, &filtered, SortCol::Modified, true, true);
        assert_eq!(
            names(&entries, &asc),
            vec!["dir-old", "file-old", "dir-new", "file-new"],
        );
    }

    /// When two directories are both `Calculating`, they should fall back to
    /// alphabetical order, regardless of `sort_asc`. This is the "readable
    /// pre-settle order" the fix aims to provide.
    #[test]
    fn sort_indices_by_size_ties_on_unknown_are_alphabetical() {
        let entries = vec![
            make_entry("charlie", true, 0, SizeState::Calculating),
            make_entry("alpha", true, 0, SizeState::Calculating),
            make_entry("bravo", true, 0, SizeState::Calculating),
        ];
        let filtered: Vec<usize> = (0..entries.len()).collect();

        // Ascending.
        let asc = sort_indices(&entries, &filtered, SortCol::Size, true, true);
        assert_eq!(names(&entries, &asc), vec!["alpha", "bravo", "charlie"]);

        // Descending: alphabetical fallback applies before the reverse flip,
        // so the on-screen order flips to z..a. This is expected and still
        // readable.
        let desc = sort_indices(&entries, &filtered, SortCol::Size, false, true);
        assert_eq!(names(&entries, &desc), vec!["charlie", "bravo", "alpha"]);
    }
}

fn unique_name(dir: &Path, name: &str) -> PathBuf {
    let base = dir.join(name);
    if !base.exists() {
        return base;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for i in 2..1000 {
        let candidate = dir.join(format!("{} ({}){}", stem, i, ext));
        if !candidate.exists() {
            return candidate;
        }
    }
    base
}

fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = src.metadata()?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let child_dst = dst.join(entry.file_name());
            copy_recursive(&entry.path(), &child_dst)?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

fn wire_callbacks(ui: &MainWindow, app: Rc<RefCell<App>>) {
    {
        // Persist on window close. Slint 1.16 does not expose a per-frame
        // resize callback on `Window`, so we opportunistically capture the
        // current size here. The debounced `persist()` calls from navigation
        // and sort/view changes also call `refresh_window_size`, so mid-
        // session resizes get flushed as a side effect of normal use; this
        // final hook just guarantees a clean exit also writes the latest
        // dimensions.
        let app = app.clone();
        ui.window().on_close_requested(move || {
            if let Ok(mut a) = app.try_borrow_mut() {
                a.persist_now();
            }
            slint::CloseRequestResponse::HideWindow
        });
    }
    {
        let app = app.clone();
        ui.on_back_clicked(move || {
            app.borrow_mut().go_back();
        });
    }
    {
        let app = app.clone();
        ui.on_forward_clicked(move || {
            app.borrow_mut().go_forward();
        });
    }
    {
        let app = app.clone();
        ui.on_up_clicked(move || {
            app.borrow_mut().go_up();
        });
    }
    {
        let app = app.clone();
        ui.on_crumb_clicked(move |path| {
            let p = PathBuf::from(path.as_str());
            app.borrow_mut().navigate(p);
        });
    }
    {
        let app = app.clone();
        ui.on_path_submitted(move |path| {
            let p = PathBuf::from(path.as_str());
            if p.exists() {
                app.borrow_mut().navigate(p);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_search_changed(move |s| {
            let mut a = app.borrow_mut();
            a.search = s.to_string();
            a.rebuild_view();
            a.push_ui_state();
            if a.view_mode == 3 {
                // Search narrows `filtered`; re-lay so tile indices
                // stay aligned with the new display positions.
                a.recompute_treemap();
            }
        });
    }
    {
        let app = app.clone();
        ui.on_view_mode_changed(move |m| {
            app.borrow_mut().set_view_mode(m);
        });
    }
    {
        let app = app.clone();
        ui.on_sidebar_clicked(move |path| {
            let s = path.as_str();
            if s == TRASH_TAG {
                // Trash view not implemented yet. Stub: open system trash dir.
                if let Some(home) = dirs::home_dir() {
                    let trash = home.join(".local/share/Trash/files");
                    if trash.exists() {
                        app.borrow_mut().navigate(trash);
                        return;
                    }
                }
                log::info!("trash view not available");
            } else {
                app.borrow_mut().navigate(PathBuf::from(s));
            }
        });
    }
    {
        let app = app.clone();
        ui.on_item_clicked(move |idx, ctrl, shift| {
            app.borrow_mut().click(idx as usize, ctrl, shift);
        });
    }
    {
        let app = app.clone();
        ui.on_item_double_clicked(move |idx| {
            app.borrow_mut().double_click(idx as usize);
        });
    }
    {
        let app = app.clone();
        ui.on_item_right_clicked(move |idx, x, y| {
            let mut a = app.borrow_mut();
            let display_idx = idx as usize;
            // Right-click on an already-selected row keeps the rest of
            // the selection; otherwise reset to just this row so the
            // menu acts on what the user clicked.
            let already_selected = a
                .filtered
                .get(display_idx)
                .map(|eidx| a.selection.contains(eidx))
                .unwrap_or(false);
            if !already_selected {
                let prev = a.selection.clone();
                a.select_only(display_idx);
                a.refresh_selection_display(&prev);
            }
            drop(a);
            show_context_menu(&app, x, y, /* on_item */ true);
        });
    }
    {
        let app = app.clone();
        ui.on_background_right_clicked(move |x, y| {
            show_context_menu(&app, x, y, /* on_item */ false);
        });
    }
    {
        let app = app.clone();
        ui.on_background_clicked(move || {
            let mut a = app.borrow_mut();
            let prev = a.selection.clone();
            a.selection.clear();
            a.last_clicked = None;
            a.refresh_selection_display(&prev);
        });
    }
    {
        let app = app.clone();
        ui.on_sort_changed(move |col| {
            app.borrow_mut().set_sort(col);
        });
    }
    {
        let app = app.clone();
        ui.on_rename_submitted(move |idx, name| {
            let mut a = app.borrow_mut();
            let idx = idx as usize;
            let Some(&eidx) = a.filtered.get(idx) else { return };
            let old = a.entries[eidx].path.clone();
            let Some(parent) = old.parent() else { return };
            let new_path = parent.join(name.as_str());
            if new_path == old {
                return;
            }
            if let Err(e) = std::fs::rename(&old, &new_path) {
                log::warn!("rename {:?} → {:?}: {}", old, new_path, e);
            }
            a.refresh();
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_rename_cancelled(move || {
            if let Some(ui) = ui_weak.upgrade() {
                FileListState::get(&ui).set_editing_index(-1);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_context_action(move |action| {
            let action = action.to_string();
            let mut a = app.borrow_mut();
            match action.as_str() {
                "open" => a.ctx_open(),
                "copy" => a.ctx_copy(),
                "cut" => a.ctx_cut(),
                "paste" => a.ctx_paste(),
                "copy-path" => a.ctx_copy_path(),
                "rename" => a.ctx_rename(),
                "trash" => a.ctx_delete_to_trash(),
                "delete" => a.ctx_permanent_delete(),
                "new-folder" => a.open_new_folder_dialog(),
                "toggle-hidden" => a.toggle_hidden(),
                "refresh" => a.refresh(),
                _ => {}
            }
        });
    }
    {
        let app = app.clone();
        ui.on_new_folder_accepted(move |name| {
            app.borrow_mut().new_folder(name.to_string());
        });
    }
    {
        let app = app.clone();
        ui.on_rename_dialog_accepted(move |name| {
            app.borrow_mut().submit_rename_dialog(name.to_string());
        });
    }
    {
        let app = app.clone();
        ui.on_confirm_accepted(move || {
            app.borrow_mut().apply_confirm();
        });
    }
    {
        let app = app.clone();
        ui.on_menu_clicked(move || {
            // Show a context-style menu at a fixed position (top-right area).
            let Some(ui) = app.borrow().ui.upgrade() else { return };
            ui.set_context_entries(ModelRc::from(Rc::new(VecModel::from(main_menu(
                &app.borrow(),
            )))));
            let size = ui.window().size();
            let scale = ui.window().scale_factor();
            let win_w = (size.width as f32) / scale;
            ui.set_context_x(win_w - 260.0);
            ui.set_context_y(44.0);
            ui.set_context_visible(true);
        });
    }
    {
        let app = app.clone();
        ui.on_key_pressed(move |text, shift| app.borrow_mut().handle_key(text, shift));
    }
    {
        let app = app.clone();
        ui.on_columns_cell_clicked(move |path| {
            let path = PathBuf::from(path.as_str());
            app.borrow_mut().on_column_cell_click(path);
        });
    }
    {
        let app = app.clone();
        ui.on_columns_cell_double_clicked(move |path, is_dir, is_root| {
            let path = PathBuf::from(path.as_str());
            app.borrow_mut().on_column_cell_double_click(path, is_dir, is_root);
        });
    }
    {
        // Columns is the only view that reaches Rust via a path-keyed
        // right-click: its deep-descendant tiles may have no row in
        // `items`, so there's nothing to index. List, grid and treemap
        // all share the index-keyed `item-right-clicked` callback
        // below.
        let app = app.clone();
        ui.on_item_path_right_clicked(move |path, x, y| {
            let path = PathBuf::from(path.as_str());
            app.borrow_mut().on_column_cell_right_click(path, x, y);
        });
    }
}

fn show_context_menu(app: &Rc<RefCell<App>>, x: f32, y: f32, on_item: bool) {
    let a = app.borrow();
    let Some(ui) = a.ui.upgrade() else { return };
    let entries = if on_item {
        item_menu(&a)
    } else {
        empty_menu(&a)
    };
    drop(a);
    ui.set_context_entries(ModelRc::from(Rc::new(VecModel::from(entries))));
    ui.set_context_x(x);
    ui.set_context_y(y);
    ui.set_context_visible(true);
}

fn menu_entry(label: &str, action: &str, shortcut: &str) -> MenuEntry {
    MenuEntry {
        label: label.into(),
        action: action.into(),
        shortcut: shortcut.into(),
        separator: false,
        enabled: true,
    }
}

fn menu_separator() -> MenuEntry {
    MenuEntry {
        label: SharedString::default(),
        action: SharedString::default(),
        shortcut: SharedString::default(),
        separator: true,
        enabled: true,
    }
}

fn item_menu(a: &App) -> Vec<MenuEntry> {
    let can_paste = a.clipboard_op.is_some();
    let can_trash = !a.is_viewing_trash();
    vec![
        menu_entry(&tr("Open"), "open", "Enter"),
        menu_separator(),
        menu_entry(&tr("Cut"), "cut", "Ctrl+X"),
        menu_entry(&tr("Copy"), "copy", "Ctrl+C"),
        MenuEntry {
            label: tr("Paste").into(),
            action: "paste".into(),
            shortcut: "Ctrl+V".into(),
            separator: false,
            enabled: can_paste,
        },
        menu_entry(&tr("Copy path"), "copy-path", ""),
        menu_separator(),
        menu_entry(&tr("Rename"), "rename", "F2"),
        MenuEntry {
            label: tr("Move to trash").into(),
            action: "trash".into(),
            shortcut: "Delete".into(),
            separator: false,
            enabled: can_trash,
        },
        menu_entry(&tr("Delete permanently"), "delete", "Shift+Delete"),
    ]
}

fn empty_menu(a: &App) -> Vec<MenuEntry> {
    let can_paste = a.clipboard_op.is_some();
    vec![
        menu_entry(&tr("New folder"), "new-folder", ""),
        menu_separator(),
        MenuEntry {
            label: tr("Paste").into(),
            action: "paste".into(),
            shortcut: "Ctrl+V".into(),
            separator: false,
            enabled: can_paste,
        },
        menu_separator(),
        menu_entry(
            &tr(if a.show_hidden { "Hide hidden files" } else { "Show hidden files" }),
            "toggle-hidden",
            "Ctrl+H",
        ),
        menu_entry(&tr("Refresh"), "refresh", "F5"),
    ]
}

fn main_menu(a: &App) -> Vec<MenuEntry> {
    vec![
        menu_entry(&tr("New folder"), "new-folder", "Ctrl+Shift+N"),
        menu_entry(
            &tr(if a.show_hidden { "Hide hidden files" } else { "Show hidden files" }),
            "toggle-hidden",
            "Ctrl+H",
        ),
        menu_separator(),
        menu_entry(&tr("Refresh"), "refresh", "F5"),
    ]
}
