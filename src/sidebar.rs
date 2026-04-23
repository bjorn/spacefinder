use crate::dir_size::SizeEngine;
use crate::disk;
use crate::fs_scan::SizeState;
use crate::i18n::tr;
use crate::icons::Icons;
use crate::{MainWindow, SidebarItem};
use humansize::{format_size, BINARY};
use rustc_hash::FxHashMap;
use slint::{Model, SharedString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const TRASH_TAG: &str = "__trash__";

/// Placeholder shown while the recursive size of a Places entry is being
/// computed in the background.
const PENDING_SIZE: &str = "…";

/// Result of [`build`]: the sidebar item list plus the (index, path) pairs
/// of Places entries whose used-space totals should be filled in
/// asynchronously.
pub struct Built {
    pub items: Vec<SidebarItem>,
    pub places_to_size: Vec<(usize, PathBuf)>,
}

pub fn build(icons: &Icons) -> Built {
    let mut items: Vec<SidebarItem> = Vec::new();
    let mut places_to_size: Vec<(usize, PathBuf)> = Vec::new();

    let home = dirs::home_dir();
    push_header(&mut items, &tr("Places"));
    if let Some(home) = &home {
        push_place(&mut items, &mut places_to_size, &tr("Home"), icons.home(), home);
    }
    if let Some(p) = dirs::desktop_dir() {
        push_place(&mut items, &mut places_to_size, &tr("Desktop"), icons.folder(), &p);
    }
    if let Some(p) = dirs::document_dir() {
        push_place(&mut items, &mut places_to_size, &tr("Documents"), icons.folder(), &p);
    }
    if let Some(p) = dirs::download_dir() {
        push_place(&mut items, &mut places_to_size, &tr("Downloads"), icons.folder(), &p);
    }
    if let Some(p) = dirs::audio_dir() {
        push_place(&mut items, &mut places_to_size, &tr("Music"), icons.folder(), &p);
    }
    if let Some(p) = dirs::picture_dir() {
        push_place(&mut items, &mut places_to_size, &tr("Pictures"), icons.folder(), &p);
    }
    if let Some(p) = dirs::video_dir() {
        push_place(&mut items, &mut places_to_size, &tr("Videos"), icons.folder(), &p);
    }
    items.push(separator());
    let trash_idx = items.len();
    let trash_files = home.as_ref().map(|h| h.join(".local/share/Trash/files"));
    items.push(SidebarItem {
        label: tr("Trash").into(),
        icon: icons.trash(),
        path: TRASH_TAG.into(),
        size: trash_files
            .as_ref()
            .filter(|p| p.exists())
            .map(|_| PENDING_SIZE.into())
            .unwrap_or_default(),
        is_separator: false,
        is_header: false,
    });
    if let Some(p) = trash_files.filter(|p| p.exists()) {
        places_to_size.push((trash_idx, p));
    }

    push_header(&mut items, &tr("Drives"));
    items.push(drive(&tr("Root"), icons.drive(), &PathBuf::from("/")));

    Built { items, places_to_size }
}

fn push_header(items: &mut Vec<SidebarItem>, label: &str) {
    items.push(SidebarItem {
        label: label.into(),
        icon: slint::Image::default(),
        path: SharedString::default(),
        size: SharedString::default(),
        is_separator: false,
        is_header: true,
    });
}

fn separator() -> SidebarItem {
    SidebarItem {
        label: SharedString::default(),
        icon: slint::Image::default(),
        path: SharedString::default(),
        size: SharedString::default(),
        is_separator: true,
        is_header: false,
    }
}

fn drive(label: &str, icon: slint::Image, path: &Path) -> SidebarItem {
    let size = disk::free_and_total(path)
        .map(|(avail, _total)| format!("{} free", format_size(avail, BINARY)))
        .unwrap_or_default();
    SidebarItem {
        label: label.into(),
        icon,
        path: path.to_string_lossy().to_string().into(),
        size: size.into(),
        is_separator: false,
        is_header: false,
    }
}

fn push_place(
    items: &mut Vec<SidebarItem>,
    to_size: &mut Vec<(usize, PathBuf)>,
    label: &str,
    icon: slint::Image,
    path: &Path,
) {
    let idx = items.len();
    items.push(SidebarItem {
        label: label.into(),
        icon,
        path: path.to_string_lossy().to_string().into(),
        size: PENDING_SIZE.into(),
        is_separator: false,
        is_header: false,
    });
    to_size.push((idx, path.to_path_buf()));
}

/// Schedule a recursive-size computation for every Places entry through the
/// shared [`SizeEngine`]. Cache hits flow back synchronously; misses spawn
/// onto the engine's thread pool. The walker emits progress for every
/// directory it visits, but the per-target callback only forwards the one
/// path it cares about, so deep descendants just warm the shared cache.
pub fn spawn_places_size_jobs(
    engine: Arc<SizeEngine>,
    ui: slint::Weak<MainWindow>,
    targets: Vec<(usize, PathBuf)>,
) {
    for (idx, path) in targets {
        // Pre-resolve canonical form so the worker-side comparison matches
        // whatever the walker reports (the engine canonicalizes its root).
        let mut keys: FxHashMap<PathBuf, ()> = FxHashMap::default();
        keys.insert(path.clone(), ());
        if let Ok(canon) = std::fs::canonicalize(&path) {
            keys.insert(canon, ());
        }
        let keys = Arc::new(keys);

        let ui_for_cb = ui.clone();
        let on_progress: crate::dir_size::ProgressFn =
            Box::new(move |reported: &Path, state: SizeState, _recursive_mtime| {
                if !keys.contains_key(reported) {
                    return;
                }
                let formatted = match state {
                    SizeState::Known(n) => format_size(n, BINARY),
                    SizeState::Unknown => String::new(),
                    SizeState::Calculating => return,
                };
                let _ = ui_for_cb.upgrade_in_event_loop(move |ui| {
                    let model = ui.get_sidebar_items();
                    if let Some(mut row) = model.row_data(idx) {
                        row.size = formatted.into();
                        model.set_row_data(idx, row);
                    }
                });
            });
        engine.compute(path, 0, on_progress);
    }
}
