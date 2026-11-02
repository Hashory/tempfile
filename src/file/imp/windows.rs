use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;
use std::{io, iter};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, ReOpenFile, SetFileAttributesW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TEMPORARY,
    FILE_FLAG_DELETE_ON_CLOSE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING,
};

use crate::util;

/// Encodes `path` as a NUL-terminated UTF-16 string for Win32 APIs.
///
/// Long paths are converted to the `\\?\` (verbatim) form, mirroring what the standard library
/// does for its own file system calls. Unless the application manifest declares `longPathAware`,
/// Win32 APIs like `MoveFileExW` reject paths longer than `MAX_PATH` even when long paths are
/// enabled system-wide, but they always accept verbatim paths.
fn to_utf16(path: &Path) -> io::Result<Vec<u16>> {
    // Same threshold as the standard library: directories are limited to MAX_PATH - 12 (248).
    const LEGACY_MAX_PATH: usize = 248;
    const SEP: u16 = b'\\' as u16;
    const VERBATIM_PREFIX: [u16; 4] = [SEP, SEP, b'?' as u16, SEP];
    const DEVICE_PREFIX: [u16; 4] = [SEP, SEP, b'.' as u16, SEP];
    const UNC_PREFIX: [u16; 8] = [
        SEP,
        SEP,
        b'?' as u16,
        SEP,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        SEP,
    ];

    let encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
    let encoded = if encoded.len() < LEGACY_MAX_PATH
        || encoded.starts_with(&VERBATIM_PREFIX)
        || encoded.starts_with(&DEVICE_PREFIX)
    {
        encoded
    } else {
        // Verbatim paths aren't normalized by Windows, so normalize first (this also resolves
        // `.`, `..` and `/`).
        let absolute: Vec<u16> = std::path::absolute(path)?
            .as_os_str()
            .encode_wide()
            .collect();
        if let Some(unc) = absolute.strip_prefix(&[SEP, SEP]) {
            // \\server\share\... => \\?\UNC\server\share\...
            UNC_PREFIX.iter().chain(unc).copied().collect()
        } else {
            // C:\... => \\?\C:\...
            VERBATIM_PREFIX.iter().chain(&absolute).copied().collect()
        }
    };
    Ok(encoded.into_iter().chain(iter::once(0)).collect())
}

fn not_supported<T>(msg: &str) -> io::Result<T> {
    Err(io::Error::other(msg))
}

pub fn create_named(
    path: &Path,
    open_options: &mut OpenOptions,
    permissions: Option<&std::fs::Permissions>,
) -> io::Result<File> {
    if permissions.is_some_and(|p| p.readonly()) {
        return not_supported("changing permissions is not supported on this platform");
    }
    open_options
        .create_new(true)
        .read(true)
        .write(true)
        .custom_flags(FILE_ATTRIBUTE_TEMPORARY)
        .open(path)
}

pub fn create(dir: &Path) -> io::Result<File> {
    util::create_helper(
        dir,
        OsStr::new(".tmp"),
        OsStr::new(""),
        crate::NUM_RAND_CHARS,
        |path| {
            let f = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .share_mode(0)
                .custom_flags(FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_DELETE_ON_CLOSE)
                .open(path)?;
            // NOTE: in theory, we could delete the file immediately (we open the file in "unix
            // semantics" mode) but this seemed to corrupt something in Windows at scale (see #339).
            // So we just rely on `FILE_FLAG_DELETE_ON_CLOSE`.
            Ok(f)
        },
    )
}

pub fn reopen(file: &File, _path: &Path) -> io::Result<File> {
    let handle = file.as_raw_handle();
    unsafe {
        let handle = ReOpenFile(
            handle as HANDLE,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE,
            0,
        );
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(FromRawHandle::from_raw_handle(handle as RawHandle))
        }
    }
}

pub fn keep(path: &Path) -> io::Result<()> {
    unsafe {
        let path_w = to_utf16(path)?;
        if SetFileAttributesW(path_w.as_ptr(), FILE_ATTRIBUTE_NORMAL) == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub fn persist(old_path: &Path, new_path: &Path, overwrite: bool) -> io::Result<()> {
    unsafe {
        let old_path_w = to_utf16(old_path)?;
        let new_path_w = to_utf16(new_path)?;

        // Don't succeed if this fails. We don't want to claim to have successfully persisted a file
        // still marked as temporary because this file won't have the same consistency guarantees.
        if SetFileAttributesW(old_path_w.as_ptr(), FILE_ATTRIBUTE_NORMAL) == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut flags = 0;

        if overwrite {
            flags |= MOVEFILE_REPLACE_EXISTING;
        }

        if MoveFileExW(old_path_w.as_ptr(), new_path_w.as_ptr(), flags) == 0 {
            let e = io::Error::last_os_error();
            // If this fails, the temporary file is now un-hidden and no longer marked temporary
            // (slightly less efficient) but it will still work.
            let _ = SetFileAttributesW(old_path_w.as_ptr(), FILE_ATTRIBUTE_TEMPORARY);
            Err(e)
        } else {
            Ok(())
        }
    }
}
