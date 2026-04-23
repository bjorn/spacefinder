//! Squarified treemap layout.
//!
//! Computes a flat `Vec<Tile>` of positioned rectangles for the direct
//! children of a single directory. Each tile's size is proportional to
//! the child's on-disk bytes, and tiles are packed with the classic
//! squarified algorithm (Bruls, Huijsen, van Wijk 2000) so their aspect
//! ratios stay close to 1.
//!
//! Sizes come from two places:
//! - Files: `std::fs::metadata(path).len()`, read synchronously during
//!   the shallow scan of the parent.
//! - Directories: [`SizeEngine`]'s process-wide cache, populated by the
//!   background walker. A cached hit is used directly; a miss drops the
//!   child out of this layout entirely because it has no area to occupy
//!   yet. The next recomputation (triggered by every batched size
//!   update in the controller) picks it up.
//!
//! Layout is produced in fractional coordinates: every tile carries
//! `x`, `y`, `w`, `h` in `[0.0, 1.0]` relative to the usable area. The
//! Slint side multiplies by its live width and height so a window
//! resize reflows entirely in the render layer, no Rust round-trip.
//!
//! The treemap is strictly single-level: only direct children of the
//! root are laid out. Drill-down is implemented by the controller by
//! navigating into a tile and re-running `lay_out` against the new
//! directory, mirroring how the list and grid views consume `navigate`.

use crate::dir_size::lookup_cached_size;
use humansize::{format_size, BINARY};
use std::fs;
use std::path::{Path, PathBuf};

/// Any tile with area smaller than this fraction of the usable area is
/// dropped from the output. At a 1000×700 view that is roughly a single
/// logical pixel, which keeps the total tile count bounded on huge
/// directories without losing any visually meaningful rows.
const MIN_RENDERABLE_FRAC_AREA: f32 = 1.0 / 700_000.0;

/// A positioned tile in the squarified layout. Mirrors the Slint `Tile`
/// struct (see `ui/treemap.slint`), but carries an owned `PathBuf`
/// rather than the Slint `string` form so callers can pass it back
/// through path APIs without reparsing.
///
/// `x`, `y`, `w`, `h` are fractions in `[0.0, 1.0]` of the view's usable
/// area.
#[derive(Debug, Clone)]
pub struct Tile {
    pub name: String,
    pub size_text: String,
    pub is_dir: bool,
    pub path: PathBuf,
    pub pending: bool,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Shallow per-child snapshot fed to the layout. Mirrors the fields
/// `lay_out` needs without pulling in the heavier `fs_scan::Entry`, so
/// this module stays usable even at levels where the controller did
/// not scan into `entries`.
struct RawChild {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    /// Directory whose size is not yet in the shared cache.
    pending: bool,
}

fn read_children(parent: &Path, show_hidden: bool) -> Vec<RawChild> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir(parent) else {
        return out;
    };
    for entry in dir.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let is_dir = meta.is_dir();
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let (size, pending) = if is_dir {
            match lookup_cached_size(&path) {
                Some(n) => (n, false),
                None => (0, true),
            }
        } else {
            (crate::fs_scan::on_disk_bytes(&meta), false)
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

/// Build the tile list for the direct children of `root` in fractional
/// coordinates. Children whose size is zero or not yet known are dropped
/// because they have no area to occupy; they re-appear as soon as the
/// background size walker settles them.
pub fn lay_out(root: &Path, show_hidden: bool) -> Vec<Tile> {
    let mut entries = read_children(root, show_hidden);
    // Skip entries that cannot contribute area. Pending dirs rejoin the
    // layout once the walker fills in their size.
    entries.retain(|e| e.size > 0);
    if entries.is_empty() {
        return Vec::new();
    }

    // Largest first, for visual clarity and so the squarified algorithm
    // places the big rocks before the pebbles.
    entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    let total: u64 = entries.iter().map(|e| e.size).sum();
    if total == 0 {
        return Vec::new();
    }

    // Normalize sizes into the unit square. Each normalized size is the
    // target area (w·h) of that tile inside a 1×1 rectangle.
    let sizes: Vec<f32> = entries.iter().map(|e| e.size as f32 / total as f32).collect();
    let rects = squarify(&sizes, Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 });

    let mut tiles = Vec::with_capacity(entries.len());
    for (e, r) in entries.into_iter().zip(rects.into_iter()) {
        if r.w * r.h < MIN_RENDERABLE_FRAC_AREA {
            continue;
        }
        tiles.push(Tile {
            name: e.name,
            size_text: format_size(e.size, BINARY),
            is_dir: e.is_dir,
            path: e.path,
            pending: e.pending,
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        });
    }
    tiles
}

/// Packed rectangle in fractional coordinates.
#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Classic squarified treemap (Bruls, Huijsen, van Wijk 2000).
///
/// Takes a slice of normalized sizes (their sum equals `rect.w * rect.h`
/// for an exact fill) and returns one output rectangle per input in
/// matching order. Expects the caller to pass sizes largest-first;
/// passing unsorted input still produces a valid tiling but with worse
/// aspect ratios.
///
/// The algorithm lays out tiles along the shorter edge of the
/// shrinking remainder, one "row" (strip) at a time. Each new item is
/// tentatively added to the current row; if its inclusion would make
/// the worst aspect ratio in the row worse than committing the row
/// without it, the row is flipped to output and a fresh row starts in
/// the sub-rectangle that the committed row leaves behind.
fn squarify(sizes: &[f32], rect: Rect) -> Vec<Rect> {
    let mut out: Vec<Rect> = vec![
        Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
        sizes.len()
    ];
    let mut remaining = rect;
    let mut row: Vec<usize> = Vec::new();

    let mut i = 0usize;
    while i < sizes.len() {
        if sizes[i] <= 0.0 {
            i += 1;
            continue;
        }
        let shorter = remaining.w.min(remaining.h);
        if shorter <= 0.0 {
            break;
        }
        if row.is_empty() {
            row.push(i);
            i += 1;
            continue;
        }
        let cur_worst = worst_aspect(&row, None, sizes, shorter);
        let next_worst = worst_aspect(&row, Some(sizes[i]), sizes, shorter);
        if next_worst <= cur_worst {
            row.push(i);
            i += 1;
        } else {
            layout_row(&row, sizes, &mut remaining, &mut out);
            row.clear();
            // Retry this item as the first of a fresh row.
        }
    }
    if !row.is_empty() {
        layout_row(&row, sizes, &mut remaining, &mut out);
    }
    out
}

/// Worst (max) aspect ratio inside the row, optionally including one
/// tentative extra size. Uses the closed form from the Bruls paper:
///
/// ```text
/// worst = max( w² · r_max / s² , s² / (w² · r_min) )
/// ```
///
/// where `s` is the row's total size, `w` the shorter side of the
/// remaining area, and `r_max`/`r_min` the largest and smallest entries
/// in the row. Smaller is better; 1.0 is a perfect square.
fn worst_aspect(row: &[usize], extra: Option<f32>, sizes: &[f32], shorter: f32) -> f32 {
    let mut s = 0.0_f32;
    let mut r_max = 0.0_f32;
    let mut r_min = f32::INFINITY;
    for &idx in row {
        let r = sizes[idx];
        s += r;
        if r > r_max {
            r_max = r;
        }
        if r < r_min {
            r_min = r;
        }
    }
    if let Some(e) = extra {
        s += e;
        if e > r_max {
            r_max = e;
        }
        if e < r_min {
            r_min = e;
        }
    }
    if s <= 0.0 || shorter <= 0.0 || r_min <= 0.0 {
        return f32::INFINITY;
    }
    let w2 = shorter * shorter;
    let s2 = s * s;
    let a = w2 * r_max / s2;
    let b = s2 / (w2 * r_min);
    a.max(b)
}

/// Commit `row` into `out`, subdividing the strip of `remaining` whose
/// thickness matches the row's total size. The strip spans the shorter
/// side of the remaining rectangle; `remaining` is then shrunk to the
/// sub-rectangle left over.
fn layout_row(
    row: &[usize],
    sizes: &[f32],
    remaining: &mut Rect,
    out: &mut [Rect],
) {
    let row_sum: f32 = row.iter().map(|&i| sizes[i]).sum();
    if row_sum <= 0.0 {
        return;
    }
    // Horizontal strip when the remaining rect is wider than tall: the
    // strip spans the full width and has height `strip_h`, the tiles
    // inside divide the width.
    let horizontal = remaining.w <= remaining.h;
    if horizontal {
        let strip_h = row_sum / remaining.w;
        let mut cursor = remaining.x;
        for &idx in row {
            let s = sizes[idx];
            let tile_w = s / strip_h;
            out[idx] = Rect {
                x: cursor,
                y: remaining.y,
                w: tile_w,
                h: strip_h,
            };
            cursor += tile_w;
        }
        remaining.y += strip_h;
        remaining.h -= strip_h;
    } else {
        let strip_w = row_sum / remaining.h;
        let mut cursor = remaining.y;
        for &idx in row {
            let s = sizes[idx];
            let tile_h = s / strip_w;
            out[idx] = Rect {
                x: remaining.x,
                y: cursor,
                w: strip_w,
                h: tile_h,
            };
            cursor += tile_h;
        }
        remaining.x += strip_w;
        remaining.w -= strip_w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        }
    }

    /// The output rectangles must tile the input rectangle: total area
    /// equals the input area (within float tolerance), and every tile
    /// stays inside the [0, 1] × [0, 1] unit square.
    #[test]
    fn squarify_tiles_exactly_cover_input() {
        let sizes = vec![6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let total: f32 = sizes.iter().sum();
        // Normalize so sum == 1.0 to match lay_out()'s contract.
        let norm: Vec<f32> = sizes.iter().map(|s| s / total).collect();
        let rects = squarify(&norm, unit_rect());
        assert_eq!(rects.len(), norm.len());

        let mut covered = 0.0f32;
        for r in &rects {
            assert!(r.x >= -1e-4, "x out of unit: {r:?}");
            assert!(r.y >= -1e-4, "y out of unit: {r:?}");
            assert!(r.x + r.w <= 1.0 + 1e-4, "right edge out of unit: {r:?}");
            assert!(r.y + r.h <= 1.0 + 1e-4, "bottom edge out of unit: {r:?}");
            assert!(r.w >= 0.0 && r.h >= 0.0, "negative dim: {r:?}");
            covered += r.w * r.h;
        }
        assert!(
            (covered - 1.0).abs() < 1e-3,
            "total covered area {} != 1.0",
            covered
        );
    }

    /// Output rectangles must appear in the same order as the input
    /// sizes. This matters because the caller zips input `entries` with
    /// output `rects` to build tiles.
    #[test]
    fn squarify_preserves_input_order() {
        let sizes = vec![0.5, 0.3, 0.15, 0.05];
        let rects = squarify(&sizes, unit_rect());
        // Largest size should have the largest area.
        let areas: Vec<f32> = rects.iter().map(|r| r.w * r.h).collect();
        assert!(areas[0] > areas[1]);
        assert!(areas[1] > areas[2]);
        assert!(areas[2] > areas[3]);
        // Each tile's area should approximately equal its normalized
        // size (since the unit square has area 1.0).
        for (s, a) in sizes.iter().zip(areas.iter()) {
            assert!((s - a).abs() < 1e-4, "size {} vs area {}", s, a);
        }
    }

    /// Degenerate inputs do not panic: empty slice, a single size,
    /// and a zero-sized tile at the tail.
    #[test]
    fn squarify_handles_edge_cases() {
        let empty: Vec<Rect> = squarify(&[], unit_rect());
        assert!(empty.is_empty());

        let one = squarify(&[1.0], unit_rect());
        assert_eq!(one.len(), 1);
        assert!((one[0].w - 1.0).abs() < 1e-4);
        assert!((one[0].h - 1.0).abs() < 1e-4);

        // Zero-sized tile at tail should be skipped, not crash.
        let with_zero = squarify(&[0.75, 0.25, 0.0], unit_rect());
        assert_eq!(with_zero.len(), 3);
        // Zero tile got a default Rect (w=h=0).
        assert_eq!(with_zero[2].w, 0.0);
        assert_eq!(with_zero[2].h, 0.0);
    }

    /// Fixture dir with a known set of visible and hidden files: the
    /// `show_hidden` flag must filter dot-entries out of the layout.
    #[test]
    fn hidden_entries_filtered_when_show_hidden_false() {
        let base = std::env::temp_dir().join(format!(
            "space-tm-hidden-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&base).expect("create fixture dir");
        fs::write(base.join("visible.txt"), b"visible content bytes").expect("write visible");
        fs::write(base.join(".hidden.txt"), b"hidden content bytes").expect("write hidden");

        let off = lay_out(&base, false);
        let names_off: Vec<&str> = off.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names_off.iter().any(|n| *n == "visible.txt"),
            "visible.txt must appear with show_hidden=false, got {:?}",
            names_off
        );
        assert!(
            !names_off.iter().any(|n| *n == ".hidden.txt"),
            ".hidden.txt must NOT appear with show_hidden=false, got {:?}",
            names_off
        );

        let on = lay_out(&base, true);
        let names_on: Vec<&str> = on.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names_on.iter().any(|n| *n == ".hidden.txt"),
            ".hidden.txt must appear with show_hidden=true, got {:?}",
            names_on
        );

        let _ = fs::remove_dir_all(&base);
    }
}
