//! Squarified treemap layout.
//!
//! Computes a flat `Vec<Tile>` of positioned rectangles for a caller-
//! supplied set of children. Each tile's size is proportional to the
//! child's on-disk bytes, and tiles are packed with the classic
//! squarified algorithm (Bruls, Huijsen, van Wijk 2000) so their
//! aspect ratios stay close to 1.
//!
//! # Input
//!
//! [`lay_out`] takes a slice of [`TileInput`] supplied by the
//! controller. This keeps the module independent of where the entries
//! come from: the controller reuses its `filtered` slice (indices into
//! `entries`) so the treemap shares the same filter/search/sort
//! pipeline — and critically the same `row_index` values — as the
//! list and grid views. Each output [`Tile`] echoes the input's
//! `row_index` verbatim so clicks can be routed through the same
//! `click(display_idx, ctrl, shift)` handler used by list/grid.
//!
//! # Skipping zero-area entries
//!
//! A child with `size == 0` (either a truly empty file or a directory
//! whose background size walk has not yet settled) cannot contribute
//! visible area. Such entries are dropped from the output and re-join
//! the layout once the walker reports a non-zero size. The controller
//! recomputes the layout on every batched size update so pending
//! directories materialize smoothly.
//!
//! # Coordinates
//!
//! Layout is produced in fractional coordinates: every tile carries
//! `x`, `y`, `w`, `h` in `[0.0, 1.0]` relative to the usable area. The
//! Slint side multiplies by its live width and height so a window
//! resize reflows entirely in the render layer, no Rust round-trip.
//!
//! The treemap is strictly single-level: only one directory's children
//! are laid out per call. Drill-down is implemented by the controller
//! by navigating into a tile and re-running `lay_out` against the new
//! directory, mirroring how the list and grid views consume
//! `navigate`.

use humansize::{format_size, BINARY};

/// Any tile with area smaller than this fraction of the usable area is
/// dropped from the output. At a 1000×700 view that is roughly a single
/// logical pixel, which keeps the total tile count bounded on huge
/// directories without losing any visually meaningful rows.
const MIN_RENDERABLE_FRAC_AREA: f32 = 1.0 / 700_000.0;

/// Shallow per-child snapshot fed to [`lay_out`]. The `row_index` is
/// the caller's identifier for this child (usually the index into the
/// controller's `filtered` slice); `lay_out` echoes it back in each
/// resulting [`Tile`] so callbacks can route through the same indexed
/// click handlers as the list and grid views.
#[derive(Debug, Clone)]
pub struct TileInput<'a> {
    pub row_index: usize,
    pub name: &'a str,
    pub is_dir: bool,
    /// Best-known on-disk bytes. Zero means either an empty entry or a
    /// directory still being sized; either way the entry is dropped
    /// from the output.
    pub size: u64,
    /// Directory whose size is not yet in the shared cache. Included
    /// here so the UI can render partially-known rows with the
    /// "pending" styling (dimmed text, trailing `+` on totals) once
    /// we add back that treatment.
    pub pending: bool,
}

/// A positioned tile in the squarified layout. Mirrors the Slint
/// `Tile` struct (see `ui/treemap.slint`). `x`, `y`, `w`, `h` are
/// fractions in `[0.0, 1.0]` of the view's usable area.
///
/// `row_index` threads the controller's `filtered` index through the
/// layout so a click on a tile can be turned back into a
/// `click(display_idx, ctrl, shift)` call without a path-to-index
/// lookup.
#[derive(Debug, Clone)]
pub struct Tile {
    pub row_index: usize,
    pub name: String,
    pub size_text: String,
    pub is_dir: bool,
    pub pending: bool,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Build the tile list for the supplied children in fractional
/// coordinates. Children with `size == 0` are dropped because they
/// cannot occupy any visible area; they reappear on the next call
/// once the background walker fills in their size.
pub fn lay_out(inputs: &[TileInput<'_>]) -> Vec<Tile> {
    // Keep only entries that can actually contribute area. Clone the
    // pointers into a local vec so the sort below doesn't need to mutate
    // the caller's slice.
    let mut usable: Vec<&TileInput> = inputs.iter().filter(|e| e.size > 0).collect();
    if usable.is_empty() {
        return Vec::new();
    }

    // Largest first, for visual clarity and so the squarified algorithm
    // places the big rocks before the pebbles. The user's chosen sort
    // column (Name/Modified/Size) is intentionally ignored here — a
    // treemap sorted by name gives pathological aspect ratios.
    usable.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(b.name)));

    let total: u64 = usable.iter().map(|e| e.size).sum();
    if total == 0 {
        return Vec::new();
    }

    // Normalize sizes into the unit square. Each normalized size is the
    // target area (w·h) of that tile inside a 1×1 rectangle.
    let sizes: Vec<f32> = usable
        .iter()
        .map(|e| e.size as f32 / total as f32)
        .collect();
    let rects = squarify(&sizes, Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 });

    let mut tiles = Vec::with_capacity(usable.len());
    for (e, r) in usable.into_iter().zip(rects.into_iter()) {
        if r.w * r.h < MIN_RENDERABLE_FRAC_AREA {
            continue;
        }
        tiles.push(Tile {
            row_index: e.row_index,
            name: e.name.to_string(),
            size_text: format_size(e.size, BINARY),
            is_dir: e.is_dir,
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

    /// Entries with `size == 0` are dropped; the remaining entries'
    /// `row_index` values survive the layout so the caller can still
    /// route clicks back through the same `filtered` index.
    #[test]
    fn lay_out_drops_zero_size_and_preserves_row_index() {
        let inputs = vec![
            TileInput {
                row_index: 7,
                name: "small",
                is_dir: false,
                size: 100,
                pending: false,
            },
            TileInput {
                row_index: 9,
                name: "pending",
                is_dir: true,
                size: 0,
                pending: true,
            },
            TileInput {
                row_index: 11,
                name: "large",
                is_dir: false,
                size: 900,
                pending: false,
            },
        ];
        let tiles = lay_out(&inputs);
        // The pending entry is dropped; the two non-zero entries remain.
        assert_eq!(tiles.len(), 2);
        let row_indices: Vec<usize> = tiles.iter().map(|t| t.row_index).collect();
        assert!(row_indices.contains(&7));
        assert!(row_indices.contains(&11));
        assert!(!row_indices.contains(&9));
    }
}
