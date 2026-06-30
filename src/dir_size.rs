//! Recursive directory size engine.
//!
//! Computes the total on-disk size of a directory subtree in the background
//! using `jwalk` for parallel traversal. Results are memoised in a process-wide
//! cache keyed by `(canonical_path, mtime)` so re-visits are instant and each
//! subtree is walked at most once.
//!
//! # Caching
//!
//! The cache key includes the directory's own `mtime`. If the directory's
//! mtime changes (i.e. children were added/removed), the cached total is
//! considered stale.
//!
//! Note: a file's *content* can change without touching its parent directory's
//! mtime, so our cache will not reflect that kind of change until the entry
//! is manually refreshed. For this tool (disk cleanup) that is acceptable:
//! files always carry their size directly from `direntry.metadata()`, and
//! stale directory totals are only off by however much a handful of files
//! grew or shrunk since last scan.
//!
//! # Cancellation
//!
//! The engine itself does not cancel running walks. Instead, each result is
//! emitted to an `on_progress` callback; the caller is expected to compare a
//! generation counter and drop stale results. The walk runs to completion so
//! the cache is populated either way.
//!
//! # Parallelism
//!
//! A single shared `rayon::ThreadPool` sized to the number of logical CPUs is
//! used both to dispatch compute jobs and as the jwalk backing pool.

use jwalk::rayon::{ThreadPool, ThreadPoolBuilder};
use jwalk::{Parallelism, WalkDirGeneric};
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use crate::fs_scan::{SizeState, on_disk_bytes};

/// Returns `true` if `meta` is a hardlinked file already counted in this walk.
///
/// On Unix we dedupe by `(dev, ino)` so each inode is summed once, matching
/// `du`'s default. Std metadata does not expose link counts or inode ids on
/// other platforms, so the dedup is a no-op there.
#[cfg(unix)]
fn already_counted_hardlink(
    meta: &std::fs::Metadata,
    seen_inodes: &mut FxHashSet<(u64, u64)>,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1 && !seen_inodes.insert((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn already_counted_hardlink(
    _meta: &std::fs::Metadata,
    _seen_inodes: &mut FxHashSet<(u64, u64)>,
) -> bool {
    false
}

/// Cached per-directory aggregate: total on-disk bytes plus the most recent
/// mtime seen anywhere in the subtree (the dir's own mtime reflects only
/// direct child add/remove, which is misleading for "last modified"). The
/// dir's own mtime at walk time is stored too as the validity stamp: a
/// lookup compares it against the live mtime and treats a mismatch as a
/// cache miss.
#[derive(Copy, Clone, Debug)]
struct CachedAgg {
    own_mtime: SystemTime,
    size: u64,
    recursive_mtime: Option<SystemTime>,
}

static CACHE: LazyLock<Mutex<FxHashMap<PathBuf, CachedAgg>>> =
    LazyLock::new(|| Mutex::new(FxHashMap::default()));

/// Callback invoked for every directory whose size is settled during a walk.
/// Receives the directory path, either the computed size or `Unknown`, and —
/// when known — the maximum mtime across the subtree (so dir rows can show
/// "last modified anywhere inside", not just "last direct-child change").
pub type ProgressFn = Box<dyn Fn(&Path, SizeState, Option<SystemTime>) + Send + Sync>;

/// Parallel size computer with a shared cache.
pub struct SizeEngine {
    pool: Arc<ThreadPool>,
}

impl SizeEngine {
    pub fn new() -> Self {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let pool = ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("space-size-{}", i))
            .build()
            .expect("build rayon pool");
        Self {
            pool: Arc::new(pool),
        }
    }

    /// Schedule the size of `dir` (and all its descendant directories) to be
    /// computed in the background. For each directory settled by the walk,
    /// including every subdirectory visited, not only `dir`, `on_progress`
    /// is invoked with the resolved `SizeState`.
    ///
    /// Cache hits short-circuit: if `dir`'s `(path, mtime)` is already cached,
    /// `on_progress` is invoked synchronously on the calling thread with
    /// `SizeState::Known(size)` and no thread is spawned. The `generation`
    /// argument is accepted only for caller convenience (it is not inspected
    /// by the engine); callers capture it in `on_progress` to discard stale
    /// results.
    pub fn compute(&self, dir: PathBuf, _generation: u64, on_progress: ProgressFn) {
        // Fast path: cache hit for the top-level dir. This is the common case
        // when re-navigating into a previously-walked tree.
        if let Some((canon, agg)) = lookup_cached(&dir) {
            on_progress(&canon, SizeState::Known(agg.size), agg.recursive_mtime);
            return;
        }

        let pool = self.pool.clone();
        let pool_for_walk = pool.clone();
        pool.spawn(move || {
            walk_and_aggregate(&dir, pool_for_walk, &on_progress);
        });
    }
}

/// Look up a dir's cached aggregate. Returns `(canonical_path, agg)` on a
/// fresh hit, or `None` if there is no entry or the dir's mtime has moved
/// since the entry was written.
fn lookup_cached(dir: &Path) -> Option<(PathBuf, CachedAgg)> {
    let canon = std::fs::canonicalize(dir).ok()?;
    let current_mtime = std::fs::metadata(&canon).ok()?.modified().ok()?;
    let cache = CACHE.lock().ok()?;
    let agg = cache.get(&canon).copied()?;
    (agg.own_mtime == current_mtime).then_some((canon, agg))
}

/// Synchronous cache probe for callers that need just the size. Returns
/// `None` on a cache miss or any I/O error reading the directory's
/// mtime. Does not schedule a walk; use [`SizeEngine::compute`] if a
/// miss should trigger background work.
pub fn lookup_cached_size(dir: &Path) -> Option<u64> {
    lookup_cached(dir).map(|(_, agg)| agg.size)
}

/// Synchronous cache probe returning `(size, recursive_mtime)`. Mirrors
/// the payload that [`SizeEngine::compute`] would deliver via
/// `on_progress` on a cache hit, minus the event-loop hop — callers that
/// own the target row synchronously can use this to avoid a first-frame
/// flash of "pending" state for already-walked directories.
pub fn lookup_cached_total(dir: &Path) -> Option<(u64, Option<SystemTime>)> {
    lookup_cached(dir).map(|(_, agg)| (agg.size, agg.recursive_mtime))
}

/// Drop cache entries for every path in `paths` and for all of their
/// ancestor directories. Call after a filesystem mutation under those
/// paths so ancestors keep showing the correct total. A directory's own
/// mtime changes only when its direct children do; when contents change
/// deeper in the subtree, the mtime up the chain stays the same and the
/// cache key still matches, serving the pre-mutation size on the next
/// lookup. Removing those entries forces a fresh walk.
pub fn invalidate_ancestors_of_paths<I, P>(paths: I)
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut targets: FxHashSet<PathBuf> = FxHashSet::default();
    for p in paths {
        let mut cur: Option<PathBuf> = Some(p.as_ref().to_path_buf());
        while let Some(x) = cur {
            if let Ok(c) = std::fs::canonicalize(&x) {
                targets.insert(c);
            }
            targets.insert(x.clone());
            cur = x.parent().map(|q| q.to_path_buf());
        }
    }
    if let Ok(mut cache) = CACHE.lock() {
        cache.retain(|k, _| !targets.contains(k));
    }
}

/// Perform the full walk for `root`, populate the cache, and emit
/// `on_progress` for every directory encountered.
fn walk_and_aggregate(root: &Path, pool: Arc<ThreadPool>, on_progress: &ProgressFn) {
    // Canonicalize once so cache entries share a normalized key even if the
    // user reached the dir via a symlink.
    let canon_root = match std::fs::canonicalize(root) {
        Ok(p) => p,
        Err(e) => {
            log::debug!("size: canonicalize {:?} failed: {}", root, e);
            on_progress(root, SizeState::Unknown, None);
            return;
        }
    };

    // Per-directory accumulator. Tuple is
    // `(own_mtime, direct_bytes, running_max_mtime)`: own_mtime keys the
    // cache; direct_bytes is summed at file-encounter time and rolled up
    // bottom-up; running_max_mtime starts at own_mtime and is bumped by
    // every file and (in the rollup pass) every descendant's final max.
    let mut dir_info: FxHashMap<PathBuf, (SystemTime, u64, SystemTime)> = FxHashMap::default();
    // Parent-dir path → child-dir paths, for the aggregation pass.
    let mut dir_children: FxHashMap<PathBuf, Vec<PathBuf>> = FxHashMap::default();
    // Dirs we failed to read completely. Their size is Unknown.
    let mut dir_errors: FxHashMap<PathBuf, ()> = FxHashMap::default();
    // Hardlinked files (nlink > 1) are counted the first time we see their
    // inode in this walk — matching `du`'s default. Without this, container
    // overlay storage inflates several-fold: podman layers share files across
    // `diff/` trees via hardlinks, so the same inode gets summed many times.
    let mut seen_inodes: FxHashSet<(u64, u64)> = FxHashSet::default();

    // Seed the root so it appears in outputs even when empty.
    match std::fs::metadata(&canon_root) {
        Ok(meta) => {
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            dir_info.insert(canon_root.clone(), (mtime, 0, mtime));
        }
        Err(e) => {
            log::debug!("size: stat root {:?} failed: {}", canon_root, e);
            on_progress(&canon_root, SizeState::Unknown, None);
            return;
        }
    }

    // Walk. We use `process_read_dir` to capture per-directory state directly;
    // iterating the resulting `DirEntryIter` is what actually drives the walk.
    let walker = WalkDirGeneric::<((), ())>::new(&canon_root)
        .follow_links(false)
        .skip_hidden(false)
        .parallelism(Parallelism::RayonExistingPool {
            pool,
            busy_timeout: None,
        });

    for dir_entry_result in walker {
        match dir_entry_result {
            Ok(entry) => {
                let parent = entry.parent_path().to_path_buf();
                let path = entry.path();

                if entry.file_type.is_dir() {
                    // Record this dir's own mtime for cache key.
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    dir_info.entry(path.clone()).or_insert((mtime, 0, mtime));
                    // Remember this dir under its parent (skip the root itself;
                    // its parent is outside the walk).
                    if path != canon_root {
                        dir_children
                            .entry(parent.clone())
                            .or_default()
                            .push(path.clone());
                    }
                    // Did we fail to read its children?
                    if entry.read_children_error.is_some() {
                        dir_errors.insert(path, ());
                    }
                } else if entry.file_type.is_file() {
                    // Add the file's on-disk size to its parent's direct-file
                    // sum. Symlinks (file_type.is_symlink()) are intentionally
                    // skipped: we do not follow them and we do not count the
                    // symlink's own size.
                    if let Ok(meta) = entry.metadata() {
                        // Dedupe hardlinks so each inode is counted once per walk.
                        if already_counted_hardlink(&meta, &mut seen_inodes) {
                            continue;
                        }
                        let bytes = on_disk_bytes(&meta);
                        let file_mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                        if let Some(info) = dir_info.get_mut(&parent) {
                            info.1 = info.1.saturating_add(bytes);
                            if file_mtime > info.2 {
                                info.2 = file_mtime;
                            }
                        } else {
                            // Parent not yet registered (root's direct files):
                            // ensure the parent exists.
                            let parent_mtime = std::fs::metadata(&parent)
                                .ok()
                                .and_then(|m| m.modified().ok())
                                .unwrap_or(SystemTime::UNIX_EPOCH);
                            let max = parent_mtime.max(file_mtime);
                            dir_info.insert(parent.clone(), (parent_mtime, bytes, max));
                        }
                    }
                }
            }
            Err(e) => {
                // A read_dir somewhere failed. The path isn't directly
                // exposed on the error here for all variants, but we already
                // flag dirs via `read_children_error` above when possible.
                log::trace!("size: walk error: {}", e);
            }
        }
    }

    // Bottom-up aggregation. Process dirs by descending depth so each dir's
    // children are already totalled when we get to it.
    let mut dirs: Vec<PathBuf> = dir_info.keys().cloned().collect();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

    // Per-dir final totals: (size_bytes, recursive_max_mtime).
    let mut totals: FxHashMap<PathBuf, (u64, SystemTime)> = FxHashMap::default();
    for dir in dirs {
        let (own_mtime, direct_files, mut max_mtime) = dir_info[&dir];
        let is_error_dir = dir_errors.contains_key(&dir);
        let mut subtotal: u64 = direct_files;
        if let Some(children) = dir_children.get(&dir) {
            for child in children {
                if let Some(&(child_total, child_max)) = totals.get(child) {
                    subtotal = subtotal.saturating_add(child_total);
                    if child_max > max_mtime {
                        max_mtime = child_max;
                    }
                }
            }
        }
        totals.insert(dir.clone(), (subtotal, max_mtime));

        if is_error_dir {
            // The directory itself was unreadable: we have no idea what's
            // inside. Do not cache; a retry later may succeed (e.g. after the
            // user fixes permissions).
            on_progress(&dir, SizeState::Unknown, None);
        } else {
            // Populate the cache and notify the caller. The total is a
            // best-effort sum: any unreadable descendants simply don't
            // contribute their bytes to our tally.
            let agg = CachedAgg {
                own_mtime,
                size: subtotal,
                recursive_mtime: Some(max_mtime),
            };
            if let Ok(mut cache) = CACHE.lock() {
                cache.insert(dir.clone(), agg);
            }
            on_progress(&dir, SizeState::Known(subtotal), agg.recursive_mtime);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Build a small on-disk tree we control end-to-end, so cache warm/cold
    /// behaviour is observable without depending on `$HOME`.
    ///
    /// Layout:
    ///   root/
    ///     a.bin (1000 bytes)
    ///     sub/
    ///       b.bin (2000 bytes)
    ///       inner/
    ///         c.bin (4000 bytes)
    ///
    /// The expected total is summed from the files' actual on-disk block
    /// counts after creation: the engine reports `blocks*512`, which depends
    /// on cluster size and filesystem compression, so a hard-coded logical
    /// total would flake across filesystems.
    fn make_fixture() -> (PathBuf, u64) {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("space-dir-size-{}-{}", pid, nanos));
        std::fs::create_dir_all(root.join("sub/inner")).unwrap();
        let files = [
            ("a.bin", 1000usize),
            ("sub/b.bin", 2000),
            ("sub/inner/c.bin", 4000),
        ];
        let mut expected: u64 = 0;
        for (rel, len) in files {
            let p = root.join(rel);
            std::fs::write(&p, vec![0u8; len]).unwrap();
            expected = expected.saturating_add(on_disk_bytes(&std::fs::metadata(&p).unwrap()));
        }
        (root, expected)
    }

    /// Cold run walks the tree, warm run must hit the cache and still report
    /// `SizeState::Known(size)` for the root. Guards the specific regression
    /// tracked in the prior WIP: warm observed `Unknown` or nothing.
    #[test]
    fn cold_then_warm_hits_cache_with_known_size() {
        let (root, expected) = make_fixture();
        let engine = SizeEngine::new();

        let cold = run_once(&engine, &root, 1);
        assert_eq!(
            cold.root_total,
            Some(expected),
            "cold walk should report the full {} bytes",
            expected
        );

        let warm = run_once(&engine, &root, 2);
        assert_eq!(
            warm.root_total,
            Some(expected),
            "warm walk must still emit SizeState::Known({})",
            expected
        );

        // Cold touched many dirs; warm on a cache hit should emit exactly
        // one progress event (for the root only).
        assert_eq!(
            warm.dirs_seen, 1,
            "warm should short-circuit to a single cache-hit emission"
        );
        // And it should be virtually instantaneous. Give generous slack to
        // keep the check robust on slow CI / loaded machines.
        assert!(
            warm.elapsed < std::time::Duration::from_millis(50),
            "warm completed in {:?}, expected <50ms cache hit",
            warm.elapsed
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression guard for the UI-responsiveness fix: the walker emits a
    /// callback for every directory in the subtree, but the controller wraps
    /// that callback in a visibility-scope filter so only direct children of
    /// the currently-viewed dir ever make it through. This test builds a
    /// multi-level tree, runs the walker with a filter matching only the
    /// top-level children, and asserts that the filter fired for those
    /// direct children and nothing deeper.
    ///
    /// Layout (identical to `make_fixture`):
    ///   root/
    ///     a.bin (file, never filtered in: not a dir)
    ///     sub/            <- direct child, filtered in
    ///       b.bin
    ///       inner/        <- grandchild, MUST be filtered out
    ///         c.bin
    #[test]
    fn scope_filter_only_fires_for_direct_children() {
        let (root, _) = make_fixture();
        let canon_root = std::fs::canonicalize(&root).unwrap();
        let engine = SizeEngine::new();

        // Build the "visible" set: direct-child dirs of `root`. The walker
        // will settle and emit for every directory, including the root
        // itself, `sub`, and `sub/inner`. Only `sub` is a direct child, so
        // only that path should pass the filter.
        let mut visible: FxHashMap<PathBuf, ()> = FxHashMap::default();
        for e in std::fs::read_dir(&canon_root).unwrap() {
            let e = e.unwrap();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                visible.insert(e.path(), ());
                if let Ok(c) = std::fs::canonicalize(e.path()) {
                    visible.insert(c, ());
                }
            }
        }

        // Counters: how many raw callbacks did the walker emit, and how
        // many survived the scope filter.
        let raw = Arc::new(AtomicU64::new(0));
        let filtered_hits: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let root_done = Arc::new(Mutex::new(false));
        let cv = Arc::new(std::sync::Condvar::new());

        let raw_cb = raw.clone();
        let filtered_cb = filtered_hits.clone();
        let root_done_cb = root_done.clone();
        let cv_cb = cv.clone();
        let target = canon_root.clone();
        let visible_for_cb = visible.clone();

        engine.compute(
            root.clone(),
            1,
            Box::new(move |p, _s, _m| {
                raw_cb.fetch_add(1, Ordering::Relaxed);
                if visible_for_cb.contains_key(p) {
                    filtered_cb.lock().unwrap().push(p.to_path_buf());
                }
                // Use the root completion to synchronize the test.
                if p == target {
                    *root_done_cb.lock().unwrap() = true;
                    cv_cb.notify_all();
                }
            }),
        );
        let mut g = root_done.lock().unwrap();
        while !*g {
            g = cv.wait(g).unwrap();
        }

        // Walker should see every directory in the tree: root, sub, and
        // sub/inner (at minimum).
        assert!(
            raw.load(Ordering::Relaxed) >= 3,
            "walker should emit for every dir; saw {}",
            raw.load(Ordering::Relaxed)
        );

        // But the scope filter should only admit direct children of `root`,
        // i.e. `sub`. Neither `root` itself nor `sub/inner` are direct
        // children of the currently-viewed dir in this setup.
        let hits = filtered_hits.lock().unwrap();
        assert_eq!(
            hits.len(),
            1,
            "scope filter should admit exactly one direct child, got {:?}",
            *hits
        );
        let sub_path = canon_root.join("sub");
        let sub_alt = root.join("sub");
        assert!(
            hits.iter().any(|h| h == &sub_path || h == &sub_alt),
            "the one admitted hit should be `sub`; got {:?}",
            *hits
        );

        // And crucially: the grandchild `sub/inner` must never have been
        // admitted, even though the walker settled it.
        let inner_path = canon_root.join("sub").join("inner");
        let inner_alt = root.join("sub").join("inner");
        assert!(
            !hits.iter().any(|h| h == &inner_path || h == &inner_alt),
            "scope filter leaked a grandchild into the admitted set: {:?}",
            *hits
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Hardlinked files must be counted once per walk, matching `du`'s default.
    /// Container overlay storage hardlinks shared files across layer dirs; without
    /// this, reported sizes balloon (the motivating case: a 480 MiB podman layer
    /// that reported 2.65 GiB).
    #[cfg(unix)]
    #[test]
    fn hardlinks_counted_once_per_walk() {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "space-dir-size-hardlink-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(root.join("layer_a")).unwrap();
        std::fs::create_dir_all(root.join("layer_b")).unwrap();
        let original = root.join("layer_a/big.bin");
        std::fs::write(&original, vec![0u8; 10_000]).unwrap();
        std::fs::hard_link(&original, root.join("layer_b/big.bin")).unwrap();

        let expected = on_disk_bytes(&std::fs::metadata(&original).unwrap());
        let engine = SizeEngine::new();
        let run = run_once(&engine, &root, 1);
        assert_eq!(
            run.root_total,
            Some(expected),
            "hardlinked inode must be counted once, not once per link"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Directories should report the max mtime across their subtree, not
    /// their own `st_mtime` (which only reflects direct child add/remove).
    /// This guards the case the user hit: `root/` unchanged since long ago,
    /// but `root/sub/file.txt` written recently — the row should show the
    /// recent time, not the stale dir time.
    #[test]
    fn recursive_mtime_reflects_deepest_change() {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "space-dir-size-rmtime-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        // Record root's own mtime *before* touching anything deeper. On
        // Linux, creating a grandchild file bumps `sub`'s mtime but not
        // `root`'s, which is the whole point — root's stat mtime stays
        // older than the new file's.
        let root_own_mtime = std::fs::metadata(&root).unwrap().modified().unwrap();
        // Let wall clock advance enough that filesystem-granularity mtimes
        // can distinguish the two events. ext4/btrfs are nanosecond, but
        // tmpfs on older kernels falls back to seconds — pause a touch
        // more than one second to be safe.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let deep = root.join("sub/file.txt");
        std::fs::write(&deep, b"hi").unwrap();
        let deep_mtime = std::fs::metadata(&deep).unwrap().modified().unwrap();
        assert!(
            deep_mtime > root_own_mtime,
            "test setup: deep file must be newer than root"
        );

        let engine = SizeEngine::new();
        let run = run_once(&engine, &root, 1);
        let reported = run
            .root_recursive_mtime
            .expect("recursive mtime should be delivered");
        assert!(
            reported >= deep_mtime,
            "recursive mtime {:?} should cover deep file {:?}",
            reported,
            deep_mtime
        );
        assert!(
            reported > root_own_mtime,
            "recursive mtime {:?} should exceed root's own mtime {:?}",
            reported,
            root_own_mtime
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Ad-hoc probe against a real path:
    /// `SPACE_PROBE=/some/dir cargo test --release -- --nocapture --ignored size_probe`
    #[test]
    #[ignore]
    fn size_probe() {
        let Ok(path) = std::env::var("SPACE_PROBE") else {
            eprintln!("set SPACE_PROBE=<dir> to run this probe");
            return;
        };
        let engine = SizeEngine::new();
        let run = run_once(&engine, Path::new(&path), 1);
        eprintln!(
            "PROBE {}: total={} bytes, dirs_seen={}, elapsed={:?}",
            path,
            run.root_total
                .map(|n| n.to_string())
                .unwrap_or_else(|| "UNKNOWN".into()),
            run.dirs_seen,
            run.elapsed,
        );
    }

    /// `cargo test --release -- --nocapture --ignored bench_home_cold_then_warm`
    /// to measure wall-clock time of the engine against `$HOME`.
    #[test]
    #[ignore]
    fn bench_home_cold_then_warm() {
        let home = dirs::home_dir().expect("HOME");
        let engine = SizeEngine::new();

        let cold = run_once(&engine, &home, 1);
        eprintln!(
            "COLD: {:?}, total={}, dirs_seen={}",
            cold.elapsed,
            cold.root_total
                .map(|n| format!("{} bytes", n))
                .unwrap_or_else(|| "UNKNOWN".into()),
            cold.dirs_seen,
        );

        let warm = run_once(&engine, &home, 2);
        eprintln!(
            "WARM: {:?}, total={}, dirs_seen={}",
            warm.elapsed,
            warm.root_total
                .map(|n| format!("{} bytes", n))
                .unwrap_or_else(|| "UNKNOWN".into()),
            warm.dirs_seen,
        );
    }

    struct BenchRun {
        elapsed: std::time::Duration,
        root_total: Option<u64>,
        root_recursive_mtime: Option<SystemTime>,
        dirs_seen: u64,
    }

    fn run_once(engine: &SizeEngine, dir: &Path, generation: u64) -> BenchRun {
        let hits = Arc::new(AtomicU64::new(0));
        let total = Arc::new(std::sync::Mutex::new(None::<u64>));
        let rec_mtime: Arc<std::sync::Mutex<Option<SystemTime>>> =
            Arc::new(std::sync::Mutex::new(None));
        let done = Arc::new(std::sync::Mutex::new(false));
        let cv = Arc::new(std::sync::Condvar::new());

        let hits_cb = hits.clone();
        let total_cb = total.clone();
        let rec_mtime_cb = rec_mtime.clone();
        let done_cb = done.clone();
        let cv_cb = cv.clone();
        // Canonicalize so we match the engine's reported path.
        let target = std::fs::canonicalize(dir).unwrap();

        let start = std::time::Instant::now();
        engine.compute(
            dir.to_path_buf(),
            generation,
            Box::new(move |p, s, m| {
                hits_cb.fetch_add(1, Ordering::Relaxed);
                if p == target {
                    if let SizeState::Known(n) = s {
                        *total_cb.lock().unwrap() = Some(n);
                    }
                    *rec_mtime_cb.lock().unwrap() = m;
                    let mut g = done_cb.lock().unwrap();
                    *g = true;
                    cv_cb.notify_all();
                }
            }),
        );
        // Wait for the root's own result to come in. If the compute hit the
        // cache, this fired synchronously before `compute` returned.
        let mut g = done.lock().unwrap();
        while !*g {
            g = cv.wait(g).unwrap();
        }
        BenchRun {
            elapsed: start.elapsed(),
            root_total: *total.lock().unwrap(),
            root_recursive_mtime: *rec_mtime.lock().unwrap(),
            dirs_seen: hits.load(Ordering::Relaxed),
        }
    }
}
