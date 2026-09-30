//! Windows per-user directory placement for product runtime artifacts.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Directory for `product`'s ephemeral runtime artifacts (pid files, run data).
///
/// `LOCALAPPDATA` is per-user and non-roaming, which is what machine-local
/// artifacts want: a roaming profile would carry another machine's state here.
/// Sockets are not placed by this — Windows named pipes live in a kernel
/// namespace with no directory at all.
pub fn user_runtime_dir(product: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join(product)
}

/// Directory for `product`'s persistent state (databases that outlive a boot).
///
/// Windows draws no line between runtime and state locations; both are
/// per-user under `LOCALAPPDATA`.
pub fn user_state_dir(product: &str) -> PathBuf {
    user_runtime_dir(product)
}

/// Directory for `product`'s persistent state, derived from the environment
/// alone: `LOCALAPPDATA`, then `C:\ProgramData`.
///
/// Unlike [`user_state_dir`] this never asks the known-folder API, so the
/// location is exactly what the environment says. That is the documented
/// contract of the probe worker's symbol cache (#974), and a caller that
/// publishes such a contract needs a primitive that keeps it.
pub fn user_state_dir_from_environment(product: &str) -> PathBuf {
    state_dir_from_environment_in(crate::env_vars::LOCALAPPDATA.os(), product)
}

/// The base directory for per-user persistent state, derived from the
/// environment alone: `LOCALAPPDATA`.
///
/// `None` when it is unset; the caller picks its own fallback and its own leaf
/// beneath the base.
pub fn state_home_from_environment() -> Option<PathBuf> {
    crate::env_vars::LOCALAPPDATA.os().map(PathBuf::from)
}

fn state_dir_from_environment_in(
    local_app_data: Option<std::ffi::OsString>,
    product: &str,
) -> PathBuf {
    local_app_data
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join(product)
}

/// Root under which `product` keeps per-run scratch data.
pub fn user_run_data_root(product: &str) -> PathBuf {
    user_runtime_dir(product)
}

/// Stable identity of an open file on this host.
///
/// Two paths that resolve to the same bytes on disk report the same identity,
/// which is what lets a caller notice that the file it opened has since been
/// replaced. The two fields are whatever this host uses to say that: a device
/// and inode, a volume serial and file index, or an equivalent pair. Callers
/// compare them; they do not interpret them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    /// Device, volume, or platform-equivalent file namespace.
    pub device: u64,
    /// Inode, file index, or platform-equivalent file number.
    pub file: u64,
}

/// Identity of an already-open file.
pub fn file_identity(file: &File) -> io::Result<Option<FileIdentity>> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use winapi::um::fileapi::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    use winapi::um::winnt::HANDLE;

    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, info.as_mut_ptr()) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    let info = unsafe { info.assume_init() };
    Ok(Some(FileIdentity {
        device: info.dwVolumeSerialNumber as u64,
        file: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    }))
}

/// Identity of the file a path currently names.
///
/// Windows answers this from an open handle, so the file is opened here. The
/// share mode is permissive on purpose: asking who a file is must not evict a
/// writer that already holds it.
pub fn path_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winnt::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)?;
    file_identity(&file)
}

/// Open `path` for use as an advisory lock file, creating it if absent.
///
/// The share mode is permissive on purpose: exclusion must come from the lock,
/// not from the open, or a second opener fails before it can even ask.
pub fn open_lock_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use winapi::um::winnt::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Never truncate: an existing lock file may be held right now,
        // and its contents are not ours to clear.
        .truncate(false)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
}

/// Take an exclusive advisory lock without waiting.
pub fn try_lock_exclusive(file: &File) -> io::Result<()> {
    use std::mem;
    use std::os::windows::io::AsRawHandle as _;
    use winapi::um::fileapi::LockFileEx;
    use winapi::um::minwinbase::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, OVERLAPPED};
    use winapi::um::winnt::HANDLE;

    let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Release a lock taken by [`try_lock_exclusive`].
pub fn unlock(file: &File) -> io::Result<()> {
    use std::mem;
    use std::os::windows::io::AsRawHandle as _;
    use winapi::um::fileapi::UnlockFileEx;
    use winapi::um::minwinbase::OVERLAPPED;
    use winapi::um::winnt::HANDLE;

    let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
    let result = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as HANDLE,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Whether `error` means "someone else holds it", rather than a real failure.
pub fn is_lock_conflict(error: &io::Error) -> bool {
    use winapi::shared::winerror::ERROR_LOCK_VIOLATION;

    error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32)
}

/// Encode `path` as the bytes this host uses to spell it.
///
/// Faithful, not canonical: this is the encoding a path is carried in so the
/// other end can reconstruct exactly the path that was named. It is
/// deliberately not `ipc::endpoint_scope_bytes`, which folds away differences
/// a host considers meaningless in order to hash two spellings to one identity.
/// Round-tripping through the pair here must return the original path;
/// round-tripping through that one need not.
pub fn encode_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt as _;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Reconstruct a path from [`encode_path_bytes`] output produced on this host.
///
/// Windows paths are UTF-16, so an odd byte count cannot have come from this
/// encoder and is rejected rather than silently truncated.
pub fn decode_path_bytes(bytes: &[u8]) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;

    if !bytes.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows path bytes must be little-endian UTF-16",
        ));
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
}

/// Directory for `product`'s shared application data.
///
/// Distinct from [`user_state_dir`]: state is this machine's private
/// bookkeeping, while this is the data a user expects to follow their account.
/// That is exactly the roaming/local split, so this uses the roaming root
/// while the state and runtime roles use the local one.
pub fn user_data_dir(product: &str) -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join(product)
}

/// The directory this host keeps a product's *configuration* in.
///
/// Distinct from [`user_data_dir`] on this host: Windows separates roaming
/// configuration from local application data, and a setting a user expects to
/// follow them between machines belongs in the former.
pub fn user_config_dir(product: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join(product)
}

/// Move `tmp` onto `target`, replacing it, without a window where neither is
/// readable.
///
/// `ReplaceFileW` is the call that gives that guarantee here; a bare rename
/// onto an existing file fails on Windows. With no file to replace there is
/// nothing for it to do, so a rename is both correct and cheaper.
pub fn replace_file(tmp: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{ReplaceFileW, REPLACEFILE_WRITE_THROUGH};

    if !target.exists() {
        return std::fs::rename(tmp, target);
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let target_w = wide(target);
    let tmp_w = wide(tmp);
    let ok = unsafe {
        ReplaceFileW(
            target_w.as_ptr(),
            tmp_w.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Make a directory entry created by [`replace_file`] durable.
///
/// Nothing to do here: `REPLACEFILE_WRITE_THROUGH` already committed the
/// change, and Windows does not expose a directory handle to flush.
pub fn sync_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}

/// Create a new file that only its owner can read, failing if it exists.
///
/// `create_new` is part of the contract, not a convenience: a private file
/// opened over one that already exists inherits whatever that one allowed.
///
/// Windows carries no mode bits here; the file inherits its directory's ACL.
/// That is the same protection by a different route as long as the caller
/// creates it somewhere already scoped to one user -- a per-user temp
/// directory, or one of the [`user_runtime_dir`] roles -- which is why the
/// directory choice is the caller's and matters.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Open `path` for reading without following a link at its final component.
///
/// `FILE_FLAG_OPEN_REPARSE_POINT` opens a symlink or junction itself rather
/// than its target, so the handle is always the named object. Windows does
/// not refuse the open; check the handle with [`is_link_handle`].
pub fn open_read_no_follow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Whether metadata of an [`open_read_no_follow`] handle names a reparse
/// point (symlink, junction) rather than an ordinary object.
pub fn is_link_handle(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(test)]
mod state_dir_from_environment_tests {
    use std::path::PathBuf;

    /// #975: the state base is `LOCALAPPDATA` as given, with no fallback.
    #[test]
    fn state_home_is_local_app_data_verbatim() {
        assert_eq!(
            crate::env_vars::LOCALAPPDATA.os().map(PathBuf::from),
            super::state_home_from_environment()
        );
    }

    #[test]
    fn state_dir_from_environment_prefers_local_app_data_then_program_data() {
        use super::state_dir_from_environment_in as dir;
        assert_eq!(
            dir(Some(r"D:\Users\u\AppData\Local".into()), "rp"),
            PathBuf::from(r"D:\Users\u\AppData\Local\rp")
        );
        assert_eq!(dir(None, "rp"), PathBuf::from(r"C:\ProgramData\rp"));
    }
}
