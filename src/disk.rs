//! Free/total space lookup for the filesystem hosting a given path.
//!
//! The computation follows the same convention as `df(1)`:
//!
//! - Available bytes for ordinary users: the space the calling process can
//!   actually write, excluding any reserved-for-root slice.
//! - Total bytes: the full capacity of the filesystem.
//!
//! Any error (bad path, syscall failure) returns `None`. The caller renders
//! that as an empty string rather than crashing.

use std::path::Path;

/// Available and total bytes for the filesystem hosting `path`.
///
/// `available` is what `df`'s `Avail` column shows: the bytes a non-root
/// process can actually allocate, with the root-reserved slice held back.
/// `total` is the filesystem capacity.
///
/// Returns `None` on any failure path.
#[cfg(unix)]
pub fn free_and_total(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

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
    // is what we want here too. `f_bavail` excludes the root-reserved blocks
    // (`f_bfree - f_bavail`), matching what users can actually write.
    let frsize = buf.f_frsize as u64;
    let available = (buf.f_bavail as u64).saturating_mul(frsize);
    let total = (buf.f_blocks as u64).saturating_mul(frsize);
    Some((available, total))
}

/// Available and total bytes for the volume hosting `path`, via
/// `GetDiskFreeSpaceExW`.
///
/// `available` is the free bytes available to the calling user (honoring
/// disk quotas), matching the Unix non-root figure. `total` is the volume
/// capacity. Returns `None` on any failure path.
#[cfg(windows)]
pub fn free_and_total(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    // GetDiskFreeSpaceExW wants a nul-terminated wide string. A trailing
    // separator is not required; a directory path resolves to its volume.
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    let mut free_to_caller: u64 = 0;
    let mut total: u64 = 0;

    // SAFETY: `wide` is a valid nul-terminated UTF-16 buffer that outlives the
    // call. The two out-params point at owned, aligned `u64` stack slots; the
    // third (total free, ignored) is null, which the API explicitly allows.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some((free_to_caller, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_has_positive_space() {
        let (avail, total) = free_and_total(&std::env::temp_dir())
            .expect("free_and_total on temp_dir should succeed");
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
