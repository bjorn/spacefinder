//! Icicle columns view layout.
//!
//! Computes a flat `Vec<LaidCell>` of positioned cells for a depth-capped
//! tree rooted at a single path. Column 0 is always exactly one cell (the
//! root) spanning the full view height. Column N+1 places each directory
//! in column N's range into its own `[y_start, y_end]` slice, proportional
//! to its size share of the parent.
//!
//! Sizes come from two places:
//! - Files: `std::fs::metadata(path).len()`, read synchronously during the
//!   shallow scan of each directory.
//! - Directories: [`SizeEngine`]'s process-wide cache, which is populated
//!   by the background walker. A cached hit is used directly; a miss
//!   renders as `pending` and the cell's size falls back to the sum of
//!   any files we can read cheaply at this level (zero if none). A
//!   recomputation is triggered by the controller on every batched size
//!   update, so pending cells settle as the walker reports.
//!
//! The algorithm is strictly capped at [`VISIBLE_COLUMNS`] depth and
//! skips cells smaller than [`MIN_RENDERABLE_PX`] so very large trees
//! (`~$HOME`) never blow out allocations or swamp the renderer.
//!
//! The controller is the only caller; this module is pure data massaging.

use crate::dir_size::lookup_cached_size;
use humansize::{format_size, BINARY};
use std::fs;
use std::path::{Path, PathBuf};

/// How many columns the view renders at once. Anything deeper than this
/// is cut off; the user zooms into a subtree to see its descendants.
pub const VISIBLE_COLUMNS: usize = 5;

/// Any cell shorter than this in logical pixels is skipped. Both keeps
/// rendering cheap and avoids pushing invisible cells into the Slint
/// model when a subtree has thousands of tiny entries.
const MIN_RENDERABLE_PX: f32 = 1.0;

/// A positioned cell in the icicle layout. Mirrors the Slint
/// `ColumnCell` struct (see `ui/columns_view.slint`), but carries an
/// owned `PathBuf` rather than the Slint `string` form so callers can
/// pass it back through path APIs without reparsing.
#[derive(Debug, Clone)]
pub struct LaidCell {
    pub col: usize,
    pub name: String,
    pub size_text: String,
    pub is_dir: bool,
    pub y_start: f32,
    pub y_end: f32,
    pub path: PathBuf,
    pub pending: bool,
    pub is_root: bool,
}

/// Quick snapshot of a directory entry for layout. We only need the
/// basics; the fuller `fs_scan::Entry` is not reused because this code
/// path also runs at deeper levels where we never scanned the parent
/// into the controller's `entries` list.
struct RawChild {
    name: String,
    path: PathBuf,
    is_dir: bool,
    /// Best-known size (files: direntry len; dirs: cache hit or 0).
    size: u64,
    /// Directory whose size is not yet in the cache.
    pending: bool,
}

fn read_children(parent: &Path) -> Vec<RawChild> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir(parent) else {
        return out;
    };
    for entry in dir.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let is_dir = meta.is_dir();
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let (size, pending) = if is_dir {
            match lookup_cached_size(&path) {
                Some(n) => (n, false),
                None => (0, true),
            }
        } else {
            (meta.len(), false)
        };
        out.push(RawChild {
            name,
            path,
            is_dir,
            size,
            pending,
        });
    }
    out
}

/// Build the complete flat cell list for `root` within a view of
/// `view_height` logical pixels. The output always contains exactly
/// one col-0 cell for the root; deeper columns may be sparse.
pub fn lay_out(root: &Path, view_height: f32) -> Vec<LaidCell> {
    let mut cells = Vec::new();
    if view_height <= 0.0 {
        return cells;
    }

    // Root cell: fill the whole column. Its size is the cached total (if
    // any); a miss renders as pending with size 0, because we have no
    // way to know the subtree size without the walker.
    let (root_size, root_pending) = match lookup_cached_size(root) {
        Some(n) => (n, false),
        None => (0, true),
    };
    let root_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    cells.push(LaidCell {
        col: 0,
        name: root_name,
        size_text: format_cell_size(root_size, root_pending),
        is_dir: true,
        y_start: 0.0,
        y_end: view_height,
        path: root.to_path_buf(),
        pending: root_pending,
        is_root: true,
    });

    lay_out_children(root, 0.0, view_height, 1, &mut cells);
    cells
}

/// Recursive helper. `depth` is the target column for the children of
/// `parent_path`. Bails out at `VISIBLE_COLUMNS`.
fn lay_out_children(
    parent_path: &Path,
    y_start: f32,
    y_end: f32,
    depth: usize,
    cells: &mut Vec<LaidCell>,
) {
    if depth >= VISIBLE_COLUMNS {
        return;
    }
    let span = y_end - y_start;
    if span < MIN_RENDERABLE_PX {
        return;
    }

    let mut entries = read_children(parent_path);
    // Sort largest-first for visual clarity and deterministic ordering.
    entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    // Skip empties: entries with size==0 take no space and would
    // otherwise inflate the total zero-divisor guard.
    entries.retain(|e| e.size > 0 || e.pending);

    // Pending dirs still need a visual slot, so use the max of their
    // best-known size (0 for cold misses) and 1 so they at least appear
    // proportionally... actually no: if we upscale pending rows we
    // distort warm rows. Keep them at 0 and let them materialize once
    // the walker settles.
    let total: u64 = entries.iter().map(|e| e.size).sum();
    if total == 0 {
        return;
    }

    let mut cursor = y_start;
    for e in entries {
        if e.size == 0 {
            // Pending directory with no cached size yet. Do not
            // allocate space; it will take a slot on a later layout.
            continue;
        }
        let frac = e.size as f32 / total as f32;
        let h = span * frac;
        let cell_y_end = (cursor + h).min(y_end);
        if h < MIN_RENDERABLE_PX {
            // Too small to render. Advance the cursor so siblings
            // remain proportional, but push no cell and do not recurse.
            cursor = cell_y_end;
            continue;
        }
        cells.push(LaidCell {
            col: depth,
            name: e.name.clone(),
            size_text: format_cell_size(e.size, e.pending),
            is_dir: e.is_dir,
            y_start: cursor,
            y_end: cell_y_end,
            path: e.path.clone(),
            pending: e.pending,
            is_root: false,
        });
        if e.is_dir {
            lay_out_children(&e.path, cursor, cell_y_end, depth + 1, cells);
        }
        cursor = cell_y_end;
    }
}

fn format_cell_size(size: u64, pending: bool) -> String {
    if pending && size == 0 {
        "…".to_string()
    } else if pending {
        format!("{}+", format_size(size, BINARY))
    } else {
        format_size(size, BINARY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_cell_always_spans_full_view() {
        // Use a directory that definitely exists but whose size we do
        // not pre-cache: the test temp dir. The cache miss means the
        // root will be flagged pending, but it must still be emitted
        // with col=0 and the full vertical span.
        let dir = std::env::temp_dir();
        let cells = lay_out(&dir, 800.0);
        assert!(!cells.is_empty(), "lay_out must always emit the root cell");
        let root = &cells[0];
        assert_eq!(root.col, 0);
        assert_eq!(root.y_start, 0.0);
        assert!((root.y_end - 800.0).abs() < 0.01);
        assert!(root.is_root);
        assert!(root.is_dir);
    }

    #[test]
    fn zero_view_height_emits_nothing() {
        let dir = std::env::temp_dir();
        let cells = lay_out(&dir, 0.0);
        assert!(cells.is_empty());
    }
}
