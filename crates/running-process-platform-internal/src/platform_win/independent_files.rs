//! Atomic owner-only creation for independent helper request artifacts.
//! Existing paths are never adopted or repaired. Reading another process's
//! artifact additionally requires handle-based identity/ACL verification.

use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{LocalFree, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT};

struct OwnerDescriptor(PSECURITY_DESCRIPTOR);

impl OwnerDescriptor {
    fn new() -> io::Result<Self> {
        let sid = super::host::current_user_sid_text()?;
        // Protected DACL: only this token's user, no inherited grants. Child
        // objects inherit the same owner-only access from a new directory.
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;OICI;FA;;;{sid})").encode_utf16().chain([0]).collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: terminated SDDL and valid output; success is LocalFree-owned.
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1,
            &mut descriptor, std::ptr::null_mut()) } == 0 { return Err(io::Error::last_os_error()); }
        Ok(Self(descriptor))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0, bInheritHandle: 0 }
    }
}

impl Drop for OwnerDescriptor {
    fn drop(&mut self) {
        // SAFETY: this value owns the allocation returned by the converter.
        unsafe { LocalFree(self.0); }
    }
}

fn absolute_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::path::{Component, Prefix};
    if !path.is_absolute() { return Err(io::Error::new(io::ErrorKind::InvalidInput, "private artifact path must be absolute")); }
    let ordinary_prefix = matches!(path.components().next(), Some(Component::Prefix(prefix))
        if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_) | Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _)));
    if !ordinary_prefix || path.components().any(|component| match component {
        Component::ParentDir => true,
        Component::Normal(name) => name.encode_wide().any(|unit| unit == u16::from(b':')),
        _ => false,
    }) {
        // CREATE_NEW against an alternate data stream can modify an existing
        // file rather than create the standalone private artifact we require.
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "device, traversal, or stream artifact path is forbidden"));
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) || wide.len() >= 32_767 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid private artifact path"));
    }
    wide.push(0);
    Ok(wide)
}

/// Create one new directory with a protected owner-only DACL in the creation
/// call, so there is no permissive create-then-chmod window.
pub fn create_private_launch_directory(path: &Path) -> io::Result<()> {
    let wide = absolute_path(path)?;
    let descriptor = OwnerDescriptor::new()?;
    let attributes = descriptor.attributes();
    // SAFETY: both path and security descriptor remain alive for this call.
    if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Confirm that the filesystem actually enforced the supplied ACL.
    let _directory = open_private_launch_directory(path)?;
    Ok(())
}

/// Hold an owner-only directory without permitting its deletion or rename.
/// A final junction/reparse point is rejected instead of traversed.
pub fn open_private_launch_directory(path: &Path) -> io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{OPEN_EXISTING, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};
    let wide = absolute_path(path)?;
    let handle = unsafe { CreateFileW(wide.as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE,
        std::ptr::null(), OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
        std::ptr::null_mut()) };
    if handle == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
    let file = unsafe { File::from_raw_handle(handle) };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "private launch directory is not a regular directory"));
    }
    verify_owner_only(&file)?;
    Ok(file)
}

/// Create a new private regular artifact, exclusively held until the writer
/// drops it. The helper must not open it until writes and sync have finished.
pub fn create_private_launch_file(path: &Path) -> io::Result<File> {
    let wide = absolute_path(path)?;
    let descriptor = OwnerDescriptor::new()?;
    let attributes = descriptor.attributes();
    // SAFETY: valid terminated path/security attributes; CREATE_NEW never
    // overwrites an existing file or follows an existing final reparse point.
    let handle = unsafe { CreateFileW(wide.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0,
        &attributes, CREATE_NEW, FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
        std::ptr::null_mut()) };
    if handle == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
    // SAFETY: the new handle is uniquely owned and transferred to File.
    let file = unsafe { File::from_raw_handle(handle) };
    // Some filesystems cannot enforce Windows ACLs. Refuse to hand a writer
    // back until the actual created handle proves the requested protection.
    // On failure the artifact remains empty; no launch secrets were written.
    verify_owner_only(&file)?;
    Ok(file)
}

/// Open a private regular artifact without following its final reparse point.
/// The retained handle denies concurrent writers and deletion while consumed.
pub fn open_private_launch_file(path: &Path) -> io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{OPEN_EXISTING, FILE_SHARE_READ,
        GetFileType, FILE_TYPE_DISK, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};
    let wide = absolute_path(path)?;
    // SAFETY: terminated path, no inherited handle, no security change.
    let handle = unsafe { CreateFileW(wide.as_ptr(), GENERIC_READ, FILE_SHARE_READ,
        std::ptr::null(), OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut()) };
    if handle == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
    let file = unsafe { File::from_raw_handle(handle) };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: held handle and valid output structure.
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK
        || unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "request is not an inspectable disk file"));
    }
    if information.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || information.nNumberOfLinks != 1 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "request is linked or not regular"));
    }
    verify_owner_only(&file)?;
    Ok(file)
}

fn verify_owner_only(file: &File) -> io::Result<()> {
    use windows_sys::Win32::Security::{EqualSid, GetAce, GetSecurityDescriptorControl,
        GetSecurityDescriptorOwner, IsValidAcl, IsValidSid, ACCESS_ALLOWED_ACE,
        OWNER_SECURITY_INFORMATION, DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        INHERIT_ONLY_ACE};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    let denied = || io::Error::new(io::ErrorKind::PermissionDenied, "request does not have the caller-only protected ACL");
    let expected = OwnerDescriptor::new()?;
    let mut expected_owner = std::ptr::null_mut();
    let mut defaulted = 0;
    let mut owner = std::ptr::null_mut();
    let mut acl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: owned descriptor and live file handle; every out-pointer lives
    // through this call. GetSecurityInfo's allocation owns owner/ACL pointers.
    unsafe {
        if GetSecurityDescriptorOwner(expected.0, &mut expected_owner, &mut defaulted) == 0 {
            return Err(io::Error::last_os_error());
        }
        let error = GetSecurityInfo(file.as_raw_handle(), SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION, &mut owner,
            std::ptr::null_mut(), &mut acl, std::ptr::null_mut(), &mut descriptor);
        if error != 0 { return Err(io::Error::from_raw_os_error(error as i32)); }
        let descriptor = OwnerDescriptor(descriptor);
        let mut control = 0;
        let mut revision = 0;
        if owner.is_null() || expected_owner.is_null() || acl.is_null()
            || IsValidSid(owner) == 0 || IsValidSid(expected_owner) == 0
            || EqualSid(owner, expected_owner) == 0 || IsValidAcl(acl) == 0
            || (*acl).AceCount != 1 { return Err(denied()); }
        if GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) == 0 {
            return Err(io::Error::last_os_error());
        }
        if control & SE_DACL_PROTECTED == 0 { return Err(denied()); }
        let mut ace = std::ptr::null_mut();
        if GetAce(acl, 0, &mut ace) == 0 || ace.is_null() { return Err(denied()); }
        let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
        // Type 0 is the basic ACCESS_ALLOWED_ACE. Other ACE layouts must not
        // be interpreted as this structure. Valid ACL bounds the ACE payload.
        if (*ace).Header.AceType != 0 || ((*ace).Header.AceSize as usize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
            || u32::from((*ace).Header.AceFlags) & INHERIT_ONLY_ACE != 0 || (*ace).Mask != FILE_ALL_ACCESS {
            return Err(denied());
        }
        let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        let sid_bytes = usize::from((*ace).Header.AceSize).saturating_sub(sid_offset);
        if sid_bytes < 8 { return Err(denied()); }
        let sid_pointer = std::ptr::addr_of!((*ace).SidStart).cast::<u8>();
        let required_sid_bytes = 8 + usize::from(*sid_pointer.add(1)) * 4;
        if required_sid_bytes > sid_bytes { return Err(denied()); }
        let sid = sid_pointer.cast_mut().cast();
        if IsValidSid(sid) == 0 || EqualSid(sid, expected_owner) == 0 { return Err(denied()); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_and_nul_paths_are_rejected_before_creation() {
        assert!(absolute_path(Path::new("request")).is_err());
        assert!(absolute_path(Path::new("C:\\private\0request")).is_err());
        assert!(absolute_path(Path::new(r"C:\private\request")).is_ok());
        assert!(absolute_path(Path::new(r"\\?\C:\private\request")).is_ok());
        assert!(absolute_path(Path::new(r"C:\private\request:secret")).is_err());
        assert!(absolute_path(Path::new(r"C:\private\..\request")).is_err());
        assert!(absolute_path(Path::new(r"\\.\pipe\request")).is_err());
    }
}
