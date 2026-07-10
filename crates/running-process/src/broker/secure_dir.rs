//! Private-directory helpers for broker-owned disk state.
//!
//! Broker manifests and service definitions both sit in per-user
//! directories that must not be writable by other users.

use std::fs;
use std::io;
use std::path::Path;

/// Create `path` and restrict it to the current user.
pub(crate) fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_private_permissions(path)
}

/// Return true when `path` has current-user-only permissions.
pub(crate) fn private_dir_permissions_are_private(path: &Path) -> io::Result<bool> {
    platform_private_dir_permissions_are_private(path)
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o700);
    fs::set_permissions(path, perms)
}

#[cfg(unix)]
fn platform_private_dir_permissions_are_private(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)?.permissions().mode();
    Ok(mode & 0o077 == 0)
}

/// Protected, owner-only DACL for private broker dirs.
///
/// The ACEs MUST carry `OICI` (container + object inherit). Applying a
/// protected DACL through `SetNamedSecurityInfoW` re-propagates
/// auto-inheritance to every existing descendant: each child's inherited
/// ACEs are recomputed from this DACL. With the non-inheritable
/// `D:P(A;;FA;;;OW)` this crate used before, that recomputation stripped
/// children to an EMPTY DACL — deny-everyone, including the owner — and
/// the damage escaped the tree through NTFS hardlinks, which share one
/// security descriptor per file (a hardlinked binary inside the private
/// dir bricked its sibling link in the caller's install dir; see
/// zackees/soldr#1513). `OICI` makes propagation grant owner + SYSTEM
/// full control down the tree instead, which also self-heals descendants
/// bricked by the old shape when the DACL is re-applied.
///
/// SYSTEM is granted alongside OWNER RIGHTS so AV, search indexing, and
/// backup agents keep working; it does not weaken the other-users
/// exclusion this hardening exists for.
#[cfg(windows)]
const PRIVATE_DIR_SDDL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)";

#[cfg(windows)]
fn set_private_permissions(path: &Path) -> io::Result<()> {
    apply_protected_dacl_sddl(path, PRIVATE_DIR_SDDL)
}

#[cfg(windows)]
fn apply_protected_dacl_sddl(path: &Path, sddl: &str) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };

    let sd = LocalSecurityDescriptor::from_sddl(sddl)?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = std::ptr::null_mut();
    let ok = unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) };
    if ok == 0 || present == 0 || dacl.is_null() {
        return Err(io::Error::last_os_error());
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        Err(io::Error::from_raw_os_error(status as i32))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn platform_private_dir_permissions_are_private(path: &Path) -> io::Result<bool> {
    let sddl = dir_dacl_sddl(path)?;
    // Deliberately rejects the legacy non-inheritable `D:P(A;;FA;;;OW)`
    // shape: callers that probe-and-repair must re-apply the inheritable
    // DACL so descendants stripped by the old shape get healed.
    let ace_count = sddl.matches("(A;").count();
    let owner_full = sddl.contains("(A;OICI;FA;;;OW)") || sddl.contains("(A;OICI;0x1f01ff;;;OW)");
    let system_full = sddl.contains("(A;OICI;FA;;;SY)") || sddl.contains("(A;OICI;0x1f01ff;;;SY)");
    Ok(sddl.starts_with("D:P") && ace_count == 2 && owner_full && system_full)
}

#[cfg(windows)]
fn dir_dacl_sddl(path: &Path) -> io::Result<String> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let sd = LocalSecurityDescriptor(sd);

    let mut sddl = std::ptr::null_mut();
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd.0,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut sddl,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || sddl.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _sddl_guard = LocalWideString(sddl);
    let mut len = 0;
    unsafe {
        while *sddl.add(len) != 0 {
            len += 1;
        }
    }
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(sddl, len)
    }))
}

#[cfg(windows)]
struct LocalSecurityDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl LocalSecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };

        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || sd.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(sd))
        }
    }
}

#[cfg(windows)]
impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.0.cast());
        }
    }
}

#[cfg(windows)]
struct LocalWideString(windows_sys::core::PWSTR);

#[cfg(windows)]
impl Drop for LocalWideString {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.0.cast());
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::fs::{self, File};

    use super::*;

    #[test]
    fn ensure_private_dir_passes_private_check() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        ensure_private_dir(&dir).unwrap();
        assert!(private_dir_permissions_are_private(&dir).unwrap());
    }

    /// Regression for zackees/soldr#1513: applying the protected DACL to
    /// a dir with existing contents must NOT strip the children's access.
    /// The old non-inheritable shape left every descendant with an empty
    /// DACL (deny-everyone).
    #[test]
    fn ensure_private_dir_keeps_existing_children_accessible() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        let sub = dir.join("v1.0.0");
        fs::create_dir_all(&sub).unwrap();
        let file = sub.join("service.bin");
        fs::write(&file, b"payload").unwrap();

        ensure_private_dir(&dir).unwrap();

        File::open(&file).expect("child file must stay readable by the owner");
        fs::write(&file, b"payload2").expect("child file must stay writable by the owner");
        fs::read_dir(&sub).expect("child dir must stay listable by the owner");
    }

    /// Regression for zackees/soldr#1513 (hardlink leak): NTFS hardlinks
    /// share one security descriptor per file, so stripping a link inside
    /// the private dir bricked the sibling link outside it (the
    /// pip-installed soldr.exe). The inheritable owner DACL must leave the
    /// outside link usable by the owner.
    #[test]
    fn ensure_private_dir_does_not_brick_hardlinked_files_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside.bin");
        fs::write(&outside, b"binary").unwrap();
        let dir = tmp.path().join("private");
        fs::create_dir_all(&dir).unwrap();
        fs::hard_link(&outside, dir.join("inside.bin")).unwrap();

        ensure_private_dir(&dir).unwrap();

        File::open(&outside).expect("hardlinked sibling must stay readable by the owner");
        fs::write(&outside, b"binary2").expect("hardlinked sibling must stay writable");
    }

    /// The legacy non-inheritable shape is explicitly NOT considered
    /// private any more, so probe-and-repair callers re-apply the fixed
    /// DACL and heal descendants stripped by the old one.
    #[test]
    fn legacy_non_inheritable_dacl_is_rejected_and_healed_by_reapply() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        let file = dir.join("service.bin");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&file, b"payload").unwrap();

        apply_protected_dacl_sddl(&dir, "D:P(A;;FA;;;OW)").unwrap();
        assert!(
            !private_dir_permissions_are_private(&dir).unwrap(),
            "legacy shape must be treated as not-private"
        );
        // The old shape stripped the child's inherited ACEs to an empty
        // DACL — confirm the failure mode this fix exists for, then heal.
        assert!(
            File::open(&file).is_err(),
            "legacy shape bricks children; if this starts passing, the \
             propagation behavior changed and the fix should be revisited"
        );

        ensure_private_dir(&dir).unwrap();
        assert!(private_dir_permissions_are_private(&dir).unwrap());
        File::open(&file).expect("re-applying the inheritable DACL must heal the child");
    }
}
