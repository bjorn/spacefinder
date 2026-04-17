use crate::config::{self, Config};
use crate::dir_size::SizeEngine;
use crate::fs_scan::{self, Entry, SizeState, SortCol};
use crate::i18n::{tr, tr_fmt, tr_n_fmt};
use crate::icons::Icons;
use crate::sidebar::{self, TRASH_TAG};
use crate::{Crumb, FileItem, FileListState, GridCell, GridRow, MainWindow, MenuEntry};
use humansize::{format_size, BINARY};
use rustc_hash::FxHashSet;
use slint::{ComponentHandle, Global, Model, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    selection: FxHashSet<usize>, // indices into `filtered`
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
    /// 0 = list, 1 = grid, matching `ui/main.slint`.
    view_mode: i32,
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
        let sidebar_items = sidebar::build(&icons);
        let items_model = Rc::new(VecModel::<FileItem>::default());
        ui.set_items(ModelRc::from(items_model.clone()));

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
            window_size: cfg.window_size,
            last_save: None,
            save_scheduled: Rc::new(Cell::new(false)),
        }));

        // Stash a weak handle for background workers to reach back through.
        APP_TLS.with(|slot| *slot.borrow_mut() = Some(Rc::downgrade(&app)));

        let sidebar_model = Rc::new(VecModel::from(sidebar_items));
        ui.set_sidebar_items(ModelRc::from(sidebar_model));

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

        self.entries = fs_scan::scan(&self.current).unwrap_or_else(|e| {
            log::warn!("scan failed for {}: {}", self.current.display(), e);
            Vec::new()
        });
        log::info!(
            "scanned {} ({} entries)",
            self.current.display(),
            self.entries.len()
        );
        self.rebuild_view();
        self.push_ui_state();
        self.spawn_size_jobs();
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
        let pending: Arc<Mutex<Vec<(PathBuf, SizeState)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let has_pending = Arc::new(AtomicBool::new(false));

        for entry in &self.entries {
            if !entry.is_dir {
                continue;
            }
            let dir = entry.path.clone();
            let ui_for_cb = ui.clone();
            let visible = visible.clone();
            let pending = pending.clone();
            let has_pending = has_pending.clone();
            let on_progress: crate::dir_size::ProgressFn =
                Box::new(move |path: &Path, state: SizeState| {
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
                        buf.push((path.to_path_buf(), state));
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
                            let drained: Vec<(PathBuf, SizeState)> = {
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
                });
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
        updates: Vec<(PathBuf, SizeState)>,
    ) {
        if generation != self.generation {
            return;
        }
        for (path, state) in updates {
            self.on_size_update_inner(&path, state);
        }
    }

    fn on_size_update_inner(&mut self, path: &Path, state: SizeState) {
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

        // Update the corresponding row in the live model, if visible.
        let Some(display_idx) = self.filtered.iter().position(|&i| i == eidx) else {
            return;
        };
        if let Some(mut item) = self.items_model.row_data(display_idx) {
            item.size_text = entry.size_text().into();
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

        // Sort. We sort directly on the indices.
        let sort_col = self.sort_col;
        let sort_asc = self.sort_asc;
        let folders_first = self.folders_first;
        let entries = &self.entries;
        self.filtered.sort_by(|a, b| {
            let ea = &entries[*a];
            let eb = &entries[*b];
            if folders_first && ea.is_dir != eb.is_dir {
                return if ea.is_dir {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            let ord = match sort_col {
                SortCol::Name => lexical(&ea.name, &eb.name),
                SortCol::Modified => ea.modified.cmp(&eb.modified),
                SortCol::Size => ea.effective_size().cmp(&eb.effective_size()),
            };
            if sort_asc { ord } else { ord.reverse() }
        });

        // Clamp selection.
        let max = self.filtered.len();
        self.selection.retain(|i| *i < max);
        if let Some(i) = self.last_clicked {
            if i >= max {
                self.last_clicked = None;
            }
        }
    }

    fn push_ui_state(&self) {
        let Some(ui) = self.ui.upgrade() else { return };

        let mut items: Vec<FileItem> = Vec::with_capacity(self.filtered.len());
        for (display_idx, &eidx) in self.filtered.iter().enumerate() {
            let e = &self.entries[eidx];
            items.push(FileItem {
                name: e.name.clone().into(),
                icon: self.icons.for_path(&e.path, e.is_dir),
                is_dir: e.is_dir,
                size_text: e.size_text().into(),
                modified_text: e.modified_text().into(),
                selected: self.selection.contains(&display_idx),
                cut: self.cut_paths.contains(&e.path),
                hidden: e.hidden,
            });
        }
        // Replace the contents of the existing model in one go. Keeping the
        // same `VecModel` instance means the views' bindings, scroll offset
        // and selection don't churn, and background size workers can
        // continue to mutate individual rows via `set_row_data`.
        self.items_model.set_vec(items);

        // Precompute grid rows based on window width.
        let tile = 110.0_f32;
        let size = ui.window().size();
        let scale = ui.window().scale_factor();
        let win_w = (size.width as f32) / scale;
        let content_w = (win_w - 224.0).max(tile);
        let cols = ((content_w / tile).floor() as usize).max(1);
        let mut rows: Vec<GridRow> = Vec::new();
        let mut i = 0;
        while i < self.filtered.len() {
            let end = (i + cols).min(self.filtered.len());
            let mut row_cells: Vec<GridCell> = Vec::with_capacity(end - i);
            for idx in i..end {
                row_cells.push(GridCell { index: idx as i32 });
            }
            rows.push(GridRow {
                tiles: ModelRc::from(Rc::new(VecModel::from(row_cells))),
            });
            i = end;
        }
        ui.set_tile_rows(ModelRc::from(Rc::new(VecModel::from(rows))));

        // Crumbs + current path.
        ui.set_crumbs(ModelRc::from(Rc::new(VecModel::from(self.build_crumbs()))));
        ui.set_current_path(self.current.to_string_lossy().to_string().into());

        ui.set_can_back(self.history_i > 0);
        ui.set_can_forward(self.history_i + 1 < self.history.len());
        ui.set_can_up(self.current.parent().is_some());
        ui.set_sidebar_active_path(
            self.current.to_string_lossy().to_string().into(),
        );
        ui.set_sort_col(match self.sort_col {
            SortCol::Name => 0,
            SortCol::Modified => 1,
            SortCol::Size => 2,
        });
        ui.set_sort_asc(self.sort_asc);

        let total = self.filtered.len();
        let sel = self.selection.len();

        // Sum the sizes of all visible entries (folder total).
        let visible_entries: Vec<&Entry> =
            self.filtered.iter().map(|&i| &self.entries[i]).collect();
        let (folder_bytes, folder_pending) = fs_scan::total_known_sizes(&visible_entries);
        let folder_size_text = format_total(folder_bytes, folder_pending);

        let status = if sel == 0 {
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
                .filter_map(|&i| self.filtered.get(i))
                .map(|&eidx| &self.entries[eidx])
                .collect();
            let (sel_bytes, sel_pending) = fs_scan::total_known_sizes(&selected_entries);
            let sel_size_text = format_total(sel_bytes, sel_pending);
            tr_fmt(
                "{} of {} selected, {} of {}",
                &[&sel, &total, &sel_size_text, &folder_size_text],
            )
        };
        ui.set_status_text(status.into());
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
        self.refresh();
        self.persist();
    }

    fn go_back(&mut self) {
        if self.history_i == 0 {
            return;
        }
        self.history_i -= 1;
        self.current = self.history[self.history_i].clone();
        self.selection.clear();
        self.refresh();
        self.persist();
    }

    fn go_forward(&mut self) {
        if self.history_i + 1 >= self.history.len() {
            return;
        }
        self.history_i += 1;
        self.current = self.history[self.history_i].clone();
        self.selection.clear();
        self.refresh();
        self.persist();
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.current.parent() {
            // `navigate` already calls `persist`, no need to double-save.
            self.navigate(parent.to_path_buf());
        }
    }

    fn select_only(&mut self, idx: usize) {
        self.selection.clear();
        self.selection.insert(idx);
        self.last_clicked = Some(idx);
    }

    fn toggle_selection(&mut self, idx: usize) {
        if !self.selection.insert(idx) {
            self.selection.remove(&idx);
        }
        self.last_clicked = Some(idx);
    }

    fn range_select(&mut self, idx: usize) {
        let anchor = self.last_clicked.unwrap_or(idx);
        let (lo, hi) = if anchor <= idx {
            (anchor, idx)
        } else {
            (idx, anchor)
        };
        self.selection.clear();
        for i in lo..=hi {
            self.selection.insert(i);
        }
    }

    fn click(&mut self, idx: usize, ctrl: bool, shift: bool) {
        if idx >= self.filtered.len() {
            return;
        }
        if shift {
            self.range_select(idx);
        } else if ctrl {
            self.toggle_selection(idx);
        } else {
            self.select_only(idx);
        }
        self.push_ui_state();
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
        self.selection
            .iter()
            .filter_map(|&idx| self.filtered.get(idx))
            .map(|&eidx| self.entries[eidx].path.clone())
            .collect()
    }

    // === Context menu actions ===

    fn ctx_open(&mut self) {
        if let Some(idx) = self.last_clicked {
            self.double_click(idx);
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

    fn ctx_delete_to_trash(&mut self) {
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
        self.persist();
    }

    fn set_view_mode(&mut self, mode: i32) {
        self.view_mode = mode;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_view_mode(mode);
            // re-push to recompute grid rows for new mode width assumptions
        }
        self.persist();
    }

    fn handle_key(&mut self, text: SharedString) -> bool {
        let t = text.as_str();
        match t {
            k if k == slint::SharedString::from(slint::platform::Key::Backspace).as_str() => {
                self.go_up();
                true
            }
            k if k == slint::SharedString::from(slint::platform::Key::Delete).as_str() => {
                self.ctx_delete_to_trash();
                true
            }
            k if k == slint::SharedString::from(slint::platform::Key::F2).as_str() => {
                self.ctx_rename();
                true
            }
            k if k == slint::SharedString::from(slint::platform::Key::F5).as_str() => {
                self.refresh();
                true
            }
            k if k == slint::SharedString::from(slint::platform::Key::Return).as_str() => {
                self.ctx_open();
                true
            }
            _ => false,
        }
    }
}

fn lexical(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
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

/// Map the on-disk `config::ViewMode` into the UI integer (0 = list, 1 = grid).
fn view_mode_from_config(v: config::ViewMode) -> i32 {
    match v {
        config::ViewMode::List => 0,
        config::ViewMode::Grid => 1,
    }
}

/// Map the UI integer (0 = list, anything else = grid) back to `config::ViewMode`.
fn view_mode_to_config(m: i32) -> config::ViewMode {
    if m == 1 {
        config::ViewMode::Grid
    } else {
        config::ViewMode::List
    }
}

/// Format a byte total for the status bar. When `pending` is true, some
/// directory sizes are still being computed (or were unreadable), so append a
/// `+` to signal that the displayed number is a lower bound and may grow.
fn format_total(bytes: u64, pending: bool) -> String {
    let base = format_size(bytes, BINARY);
    if pending { format!("{}+", base) } else { base }
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
        for v in [config::ViewMode::List, config::ViewMode::Grid] {
            assert_eq!(view_mode_to_config(view_mode_from_config(v)), v);
        }
        // Integer side: 0 is List, 1 is Grid; anything outside that range
        // clamps to List. We match `ui/main.slint` where `view-mode` is an
        // int with 0 = list, 1 = grid.
        assert_eq!(view_mode_to_config(0), config::ViewMode::List);
        assert_eq!(view_mode_to_config(1), config::ViewMode::Grid);
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
            let idx = idx as usize;
            if !a.selection.contains(&idx) {
                a.select_only(idx);
                a.push_ui_state();
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
            a.selection.clear();
            a.last_clicked = None;
            a.push_ui_state();
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
        ui.on_key_pressed(move |text| app.borrow_mut().handle_key(text));
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
        menu_entry(&tr("Move to trash"), "trash", "Delete"),
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
