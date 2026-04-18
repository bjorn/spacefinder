//! Free/total space lookup for the filesystem hosting a given path.
//!
//! Thin wrapper around `libc::statvfs(3)` with a safe Rust surface. The
//! computation follows the same convention as `df(1)`:
//!
//! - Available bytes for ordinary users: `f_bavail * f_frsize`. This excludes
//!   the reserved-for-root blocks (`f_bfree - f_bavail`), which is what users
//!   care about when the question is "how much can I actually write here".
//! - Total bytes: `f_blocks * f_frsize`.
//!
//! Any error (bad path, invalid CString, syscall failure) returns `None`. The
//! caller renders that as an empty string rather than crashing.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Available and total bytes for the filesystem hosting `path`.
///
/// `available` is what `df`'s `Avail` column shows (`f_bavail * f_frsize`):
/// the bytes a non-root process can actually allocate, with the root-reserved
/// slice held back. `total` is `f_blocks * f_frsize`.
///
/// Returns `None` on any failure path, including paths that cannot be
/// converted to a `CString` (interior nul) and any nonzero return from
/// `statvfs`.
pub fn free_and_total(path: &Path) -> Option<(u64, u64)> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;

    // SAFETY: `statvfs` reads the path through the C string pointer (valid
    // for the duration of the call, since `c_path` outlives it) and writes
    // the result into `buf`, a properly-aligned and fully-owned stack slot
    // of the right type. The call has no aliasing or thread-safety
    // requirements beyond those.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }

    // `f_frsize` is the fundamental block size; `f_bsize` is a preferred I/O
    // size that can differ. `df` uses `f_frsize` for both columns, and that
    // is what we want here too.
    let frsize = buf.f_frsize as u64;
    let available = (buf.f_bavail as u64).saturating_mul(frsize);
    let total = (buf.f_blocks as u64).saturating_mul(frsize);
    Some((available, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_has_positive_space() {
        let (avail, total) = free_and_total(&std::env::temp_dir())
            .expect("statvfs on temp_dir should succeed");
        assert!(avail > 0, "available bytes should be > 0, got {}", avail);
        assert!(total > 0, "total bytes should be > 0, got {}", total);
        assert!(
            avail <= total,
            "available ({}) should not exceed total ({})",
            avail,
            total,
        );
    }
}
