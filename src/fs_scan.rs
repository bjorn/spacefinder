use chrono::{DateTime, Local};
use humansize::{BINARY, format_size};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Bytes this file consumes on disk. Matches `du` (counts allocated 512-byte
/// blocks, the `st_blocks` field) rather than `ls -l` (the declared length).
///
/// For a disk-cleanup tool the on-disk figure is what actually matters:
///   * sparse files declare more than they allocate (VM images, some logs);
///   * filesystem compression (BTRFS, ZFS) shrinks allocated bytes;
///   * tiny files still occupy a whole cluster.
pub fn on_disk_bytes(meta: &fs::Metadata) -> u64 {
    meta.blocks().saturating_mul(512)
}

/// State of a directory-size computation for an `Entry`.
///
/// Files always have `SizeState::Known(size)` with the size taken directly
/// from the direntry metadata at scan time. Directories start out as
/// `Calculating` and flip to `Known` (or `Unknown` on permission errors)
/// once the async size walker finishes that subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeState {
    /// Size not yet computed (background walker still working on it).
    Calculating,
    /// Size is known in bytes.
    Known(u64),
    /// Could not be determined (e.g. permission denied walking the subtree).
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// On-disk size in bytes (allocated blocks × 512).
    ///
    /// For directories this stays at 0 and the authoritative value lives in
    /// `size_state`. For files it is `on_disk_bytes(&meta)` — matching `du`,
    /// not `ls -l`.
    pub size: u64,
    /// For directories, this tracks the recursive size computation. For
    /// files it is always `Known(size)`.
    pub size_state: SizeState,
    pub modified: SystemTime,
    pub hidden: bool,
}

impl Entry {
    /// Best-known size in bytes. For directories this is 0 until the walker
    /// settles the subtree, then the computed total.
    pub fn effective_size(&self) -> u64 {
        match self.size_state {
            SizeState::Known(n) => n,
            _ => self.size,
        }
    }

    pub fn size_text(&self) -> String {
        if self.is_dir {
            match self.size_state {
                SizeState::Known(n) => format_size(n, BINARY),
                // Single-char placeholders to keep the column compact.
                SizeState::Calculating => "\u{2026}".to_string(), // "…"
                SizeState::Unknown => "?".to_string(),
            }
        } else {
            format_size(self.size, BINARY)
        }
    }

    pub fn modified_text(&self) -> String {
        let dt: DateTime<Local> = self.modified.into();
        dt.format("%b %-d, %Y %H:%M").to_string()
    }
}

pub fn scan(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().to_string();
        let hidden = name.starts_with('.');
        let Ok(meta) = entry.metadata() else { continue };
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { on_disk_bytes(&meta) };
        let size_state = if is_dir {
            SizeState::Calculating
        } else {
            SizeState::Known(size)
        };
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(Entry {
            name,
            path,
            is_dir,
            size,
            size_state,
            modified,
            hidden,
        });
    }
    Ok(out)
}

/// Sum up the known sizes across a slice of entries, reporting whether any of
/// them still have a pending or failed size lookup. The boolean is true if any
/// entry is still `Calculating` or `Unknown`, which the caller uses to append a
/// trailing `+` to the formatted total.
pub fn total_known_sizes(entries: &[&Entry]) -> (u64, bool) {
    let mut sum: u64 = 0;
    let mut any_unknown = false;
    for e in entries {
        match e.size_state {
            SizeState::Known(n) => sum = sum.saturating_add(n),
            SizeState::Calculating | SizeState::Unknown => any_unknown = true,
        }
    }
    (sum, any_unknown)
}

#[derive(Copy, Clone)]
pub enum SortCol {
    Name,
    Modified,
    Size,
}

impl SortCol {
    pub fn from_int(i: i32) -> Self {
        match i {
            1 => Self::Modified,
            2 => Self::Size,
            _ => Self::Name,
        }
    }
}
