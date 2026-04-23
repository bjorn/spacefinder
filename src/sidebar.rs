use crate::disk;
use crate::i18n::tr;
use crate::icons::Icons;
use crate::{MainWindow, SidebarItem};
use humansize::{format_size, BINARY};
use slint::{Model, SharedString};
use std::path::{Path, PathBuf};

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
    items.push(SidebarItem {
        label: tr("Trash").into(),
        icon: icons.trash(),
        path: TRASH_TAG.into(),
        size: SharedString::default(),
        is_separator: false,
        is_header: false,
    });

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
        .map(|(avail, _total)| format_size(avail, BINARY))
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

/// Kick off a single background worker that walks each Places directory
/// sequentially and posts its total used size back to the UI thread.
///
/// Sequential (rather than one thread per entry) so the OS page cache is
/// warmed in order and we don't oversubscribe the disk on cold starts;
/// each entry is on the order of seconds for a populated `~`. Results are
/// marshalled back via `Weak::upgrade_in_event_loop` and applied with
/// `set_row_data` on the existing sidebar model — no whole-model rebuild,
/// so scroll/selection are preserved.
pub fn spawn_places_size_worker(
    ui: slint::Weak<MainWindow>,
    targets: Vec<(usize, PathBuf)>,
) {
    if targets.is_empty() {
        return;
    }
    std::thread::Builder::new()
        .name("space-places-size".into())
        .spawn(move || {
            for (idx, path) in targets {
                // Stop early if the UI has gone away.
                if ui.upgrade_in_event_loop(|_| {}).is_err() {
                    return;
                }
                let bytes = match dir_used_bytes(&path) {
                    Some(b) => b,
                    None => continue,
                };
                let formatted = format_size(bytes, BINARY);
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    let model = ui.get_sidebar_items();
                    if let Some(mut row) = model.row_data(idx) {
                        row.size = formatted.into();
                        model.set_row_data(idx, row);
                    }
                });
            }
        })
        .expect("spawn places-size worker");
}

/// Recursive sum of file sizes under `root`, equivalent to `du -sb`:
///   - uses `symlink_metadata` so symlinks are not followed
///   - stays on the starting filesystem (`dev` boundary)
///   - silently skips entries we cannot read
fn dir_used_bytes(root: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    let root_meta = std::fs::symlink_metadata(root).ok()?;
    let root_dev = root_meta.dev();
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in reader.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            // Skip anything on a different filesystem (mountpoints, fuse
            // mounts, etc.). `du -xb` style — and matters because $HOME
            // can contain mounted GVfs/FUSE trees that block on stat.
            if meta.dev() != root_dev {
                continue;
            }
            let ft = entry.file_type().ok();
            // Don't follow symlinks; just skip them (du -sb doesn't add
            // the symlink's own size into the total either).
            if ft.map(|t| t.is_symlink()).unwrap_or(false) {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Some(total)
}
