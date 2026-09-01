use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;

use crate::platform::private_dir::OwnerPrivateDirectoryOutcome;

/// Protected, inheritable owner-and-SYSTEM DACL for private IPC directories.
///
/// OICI is required because applying a protected DACL re-propagates inherited
/// ACEs through existing descendants. The earlier non-inheritable policy could
/// leave descendants with an empty DACL, including files with hardlinks outside
/// the directory. Reapplying this policy repairs that legacy state.
const PRIVATE_DIR_SDDL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)";

pub fn ensure_owner_private_directory(path: &Path) -> io::Result<OwnerPrivateDirectoryOutcome> {
    fs::create_dir_all(path)?;
    // Avoid an expensive recursive DACL propagation on every warm manifest
    // write while still forcing legacy/non-inheritable policies through repair.
    if owner_private_directory(path).unwrap_or(false) {
        return Ok(OwnerPrivateDirectoryOutcome::AlreadyPrivate);
    }
    apply_protected_dacl_sddl(path, PRIVATE_DIR_SDDL)?;
    if owner_private_directory(path)? {
        Ok(OwnerPrivateDirectoryOutcome::Hardened)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "private-directory permissions were not applied to {}",
                path.display()
            ),
        ))
    }
}

pub fn owner_private_directory(path: &Path) -> io::Result<bool> {
    let actual = file_security_descriptor(path)?;
    if !actual.dacl_is_protected()? {
        return Ok(false);
    }
    // Binary equality covers ACL revision, ACE flags/masks/SIDs/order and
    // callback or object payloads that SDDL substring checks can misclassify.
    let expected = LocalSecurityDescriptor::from_sddl(PRIVATE_DIR_SDDL)?;
    Ok(actual.dacl()?.bytes()? == expected.dacl()?.bytes()?)
}

fn apply_protected_dacl_sddl(path: &Path, sddl: &str) -> io::Result<()> {
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;

    apply_dacl_sddl(path, sddl, PROTECTED_DACL_SECURITY_INFORMATION)
}

fn apply_dacl_sddl(
    path: &Path,
    sddl: &str,
    inheritance_control: windows_sys::Win32::Security::OBJECT_SECURITY_INFORMATION,
) -> io::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    let descriptor = LocalSecurityDescriptor::from_sddl(sddl)?;
    let dacl = descriptor.dacl()?;

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: `wide` is NUL-terminated; `dacl` borrows the live descriptor
    // for this call; all unused owner/group/SACL pointers are null as the
    // requested flags update only the DACL.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | inheritance_control,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl.as_ptr(),
            std::ptr::null_mut(),
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

fn file_security_descriptor(path: &Path) -> io::Result<LocalSecurityDescriptor> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and `descriptor` is a writable out
    // pointer. The successful result is checked for null before RAII adoption.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if descriptor.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "GetNamedSecurityInfoW returned a null security descriptor",
        ));
    }
    Ok(LocalSecurityDescriptor(descriptor))
}

struct LocalSecurityDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

impl LocalSecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };

        let wide = sddl
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `wide` is a NUL-terminated SDDL string and `descriptor` is a
        // writable out pointer. A successful non-null allocation is adopted
        // exactly once by `LocalSecurityDescriptor` and freed in `Drop`.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || descriptor.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(descriptor))
        }
    }

    fn dacl_is_protected(&self) -> io::Result<bool> {
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorControl, SECURITY_DESCRIPTOR_CONTROL, SE_DACL_PROTECTED,
        };

        let mut control: SECURITY_DESCRIPTOR_CONTROL = 0;
        let mut revision = 0;
        // SAFETY: `self.0` is a live descriptor owned by `self`; both output
        // pointers refer to initialized writable locals.
        let ok = unsafe { GetSecurityDescriptorControl(self.0, &mut control, &mut revision) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(control & SE_DACL_PROTECTED != 0)
    }

    fn dacl(&self) -> io::Result<SecurityDescriptorDacl<'_>> {
        use windows_sys::Win32::Security::{GetSecurityDescriptorDacl, ACL};

        let mut present = 0;
        let mut defaulted = 0;
        let mut pointer: *mut ACL = std::ptr::null_mut();
        // SAFETY: `self.0` is a live descriptor owned by `self`; all output
        // pointers refer to writable locals. The returned borrow is tied to
        // `self`, so the descriptor outlives every use of its DACL pointer.
        let ok = unsafe {
            GetSecurityDescriptorDacl(self.0, &mut present, &mut pointer, &mut defaulted)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let pointer = std::ptr::NonNull::new(pointer).ok_or_else(|| {
            io::Error::new(io::ErrorKind::PermissionDenied, "directory has no DACL")
        })?;
        Ok(SecurityDescriptorDacl {
            pointer,
            descriptor: std::marker::PhantomData,
        })
    }
}

struct SecurityDescriptorDacl<'descriptor> {
    pointer: std::ptr::NonNull<windows_sys::Win32::Security::ACL>,
    descriptor: std::marker::PhantomData<&'descriptor LocalSecurityDescriptor>,
}

impl SecurityDescriptorDacl<'_> {
    fn as_ptr(&self) -> *mut windows_sys::Win32::Security::ACL {
        self.pointer.as_ptr()
    }

    fn bytes(&self) -> io::Result<Vec<u8>> {
        use windows_sys::Win32::Security::{
            AclSizeInformation, GetAclInformation, ACL_SIZE_INFORMATION,
        };

        let mut information = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        // SAFETY: `self.pointer` is a checked non-null DACL borrowed from a
        // live descriptor; the information buffer and size match the selected
        // ACL information class.
        let ok = unsafe {
            GetAclInformation(
                self.as_ptr(),
                (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let byte_len = information.AclBytesInUse as usize;
        if byte_len < std::mem::size_of::<windows_sys::Win32::Security::ACL>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DACL byte length is smaller than its header",
            ));
        }
        // SAFETY: GetAclInformation validated this live DACL and reported the
        // bytes in use. The slice is copied immediately while its descriptor
        // owner remains alive, so no borrowed native memory escapes.
        Ok(unsafe { std::slice::from_raw_parts(self.as_ptr().cast::<u8>(), byte_len).to_vec() })
    }
}

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the unique non-null allocation returned by
        // ConvertStringSecurityDescriptorToSecurityDescriptorW or
        // GetNamedSecurityInfoW. Both APIs transfer LocalFree ownership, and
        // the allocation is released exactly once here.
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.0.cast());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};

    use super::*;

    #[test]
    fn ensure_private_dir_is_a_noop_for_an_already_private_populated_tree() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        ensure_owner_private_directory(&directory).unwrap();
        for index in 0..1_500 {
            let shard = directory.join(format!("shard-{index:04}"));
            fs::create_dir(&shard).unwrap();
            fs::write(shard.join("artifact.bin"), b"payload").unwrap();
        }

        assert_eq!(
            ensure_owner_private_directory(&directory).unwrap(),
            OwnerPrivateDirectoryOutcome::AlreadyPrivate
        );
        File::open(directory.join("shard-1499/artifact.bin")).unwrap();
    }

    #[test]
    fn nonstandard_two_ace_policy_is_rejected_and_repaired() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        fs::create_dir_all(&directory).unwrap();
        apply_protected_dacl_sddl(&directory, "D:P(A;OICI;FA;;;SY)(OA;OICI;FA;;;OW)")
            .unwrap();

        assert!(!owner_private_directory(&directory).unwrap());
        assert_eq!(
            ensure_owner_private_directory(&directory).unwrap(),
            OwnerPrivateDirectoryOutcome::Hardened
        );
        assert!(owner_private_directory(&directory).unwrap());
    }

    #[test]
    fn unprotected_identical_acl_is_rejected_and_repaired() {
        use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;

        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("parent");
        let directory = parent.join("private");
        fs::create_dir_all(&directory).unwrap();
        apply_protected_dacl_sddl(&parent, "D:P(A;;FA;;;OW)(A;;FA;;;SY)").unwrap();
        apply_protected_dacl_sddl(&directory, PRIVATE_DIR_SDDL).unwrap();
        let protected = file_security_descriptor(&directory).unwrap();
        let protected_bytes = protected.dacl().unwrap().bytes().unwrap();

        apply_dacl_sddl(
            &directory,
            PRIVATE_DIR_SDDL,
            UNPROTECTED_DACL_SECURITY_INFORMATION,
        )
        .unwrap();
        let unprotected = file_security_descriptor(&directory).unwrap();
        assert!(!unprotected.dacl_is_protected().unwrap());
        assert_eq!(unprotected.dacl().unwrap().bytes().unwrap(), protected_bytes);
        assert!(!owner_private_directory(&directory).unwrap());
        assert_eq!(
            ensure_owner_private_directory(&directory).unwrap(),
            OwnerPrivateDirectoryOutcome::Hardened
        );
    }

    #[test]
    fn ensure_private_dir_keeps_existing_children_accessible() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        let child = directory.join("nested");
        fs::create_dir_all(&child).unwrap();
        let file = child.join("service.bin");
        fs::write(&file, b"payload").unwrap();

        ensure_owner_private_directory(&directory).unwrap();

        File::open(&file).unwrap();
        fs::write(&file, b"payload2").unwrap();
        fs::read_dir(&child).unwrap();
    }

    #[test]
    fn ensure_private_dir_does_not_brick_hardlinked_files_outside() {
        let temporary = tempfile::tempdir().unwrap();
        let outside = temporary.path().join("outside.bin");
        fs::write(&outside, b"binary").unwrap();
        let directory = temporary.path().join("private");
        fs::create_dir_all(&directory).unwrap();
        fs::hard_link(&outside, directory.join("inside.bin")).unwrap();

        ensure_owner_private_directory(&directory).unwrap();

        File::open(&outside).unwrap();
        fs::write(&outside, b"binary2").unwrap();
    }

    #[test]
    fn legacy_non_inheritable_dacl_is_rejected_and_healed_by_reapply() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("private");
        let file = directory.join("service.bin");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&file, b"payload").unwrap();

        apply_protected_dacl_sddl(&directory, "D:P(A;;FA;;;OW)").unwrap();
        assert!(!owner_private_directory(&directory).unwrap());
        assert!(File::open(&file).is_err());
        assert_eq!(
            ensure_owner_private_directory(&directory).unwrap(),
            OwnerPrivateDirectoryOutcome::Hardened
        );
        File::open(&file).unwrap();
    }
}
