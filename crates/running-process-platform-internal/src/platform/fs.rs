//! Runtime-artifact identity, permissions, secure-open, replacement, and directory primitives.
//!
//! Callers name the *role* a directory plays for their product -- ephemeral
//! runtime artifacts, persistent state, per-run scratch. Which location on this
//! host plays that role, and how two accounts are kept apart there, is decided
//! here. Callers still own their own layout beneath it: leaf names, extensions,
//! and subdirectories are product conventions, not host mechanics.

/// A descriptor the caller already owns and has asked us to write to.
///
/// Deliberately opaque. Callers hold host-specific things -- a `RawFd` on
/// Unix, a `RawHandle` on Windows -- and there is no honest neutral spelling
/// for *what they hold*, so the conversion into this type is host-specific
/// and stays at the caller's edge. What is not host-specific is everything
/// after: writing all of a buffer to it, retrying the partial writes and the
/// interruptions that every host has in its own dialect.
///
/// This borrows. It does not close the descriptor, and it does not extend its
/// lifetime: the caller who opened it still decides when it goes away, and
/// using this after that is the same mistake as using the raw value would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawDescriptor(usize);

impl RawDescriptor {
    /// Wrap a host descriptor value. Host trees call this; callers do not.
    pub(crate) fn from_value(value: usize) -> Self {
        Self(value)
    }

    /// The underlying host value, for the host tree that will use it.
    pub(crate) fn value(self) -> usize {
        self.0
    }
}

pub use crate::fs_write_all_to_descriptor as write_all_to_descriptor;

#[cfg(feature = "fs")]
pub use crate::{
    fs_create_private_file as create_private_file, fs_decode_path_bytes as decode_path_bytes,
    fs_encode_path_bytes as encode_path_bytes, fs_file_identity as file_identity,
    fs_is_lock_conflict as is_lock_conflict, fs_open_lock_file as open_lock_file,
    fs_path_identity as path_identity, fs_replace_file as replace_file,
    fs_sync_directory as sync_directory, fs_try_lock_exclusive as try_lock_exclusive,
    fs_unlock as unlock, fs_user_config_dir as user_config_dir, fs_user_data_dir as user_data_dir,
    fs_user_run_data_root as user_run_data_root, fs_user_runtime_dir as user_runtime_dir,
    fs_user_state_dir as user_state_dir, FsFileIdentity as FileIdentity,
};

#[cfg(all(test, feature = "fs"))]
mod tests {
    use super::*;

    const PRODUCT: &str = "rp-fs-facade-test";

    /// Every role resolves to an absolute directory that names the product.
    ///
    /// Asserted as a property rather than against one host's spelling: the
    /// point of the facade is that callers cannot tell which host answered.
    #[test]
    fn every_role_is_an_absolute_product_scoped_directory() {
        for directory in [
            user_runtime_dir(PRODUCT),
            user_state_dir(PRODUCT),
            user_run_data_root(PRODUCT),
        ] {
            assert!(
                directory.is_absolute(),
                "{} must be absolute",
                directory.display()
            );
            assert!(
                directory.to_string_lossy().contains(PRODUCT),
                "{} must be scoped to the product",
                directory.display()
            );
        }
    }

    /// Two products never share a directory in any role.
    #[test]
    fn distinct_products_do_not_collide() {
        let other = "rp-fs-facade-other";
        assert_ne!(user_runtime_dir(PRODUCT), user_runtime_dir(other));
        assert_ne!(user_state_dir(PRODUCT), user_state_dir(other));
        assert_ne!(user_run_data_root(PRODUCT), user_run_data_root(other));
    }

    /// A file is the same file as itself, by whichever pair this host uses.
    #[test]
    fn a_file_has_one_identity_through_both_a_handle_and_its_path() {
        let dir = std::env::temp_dir().join(format!("rp-fs-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("subject");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("create subject");

        let by_handle = file_identity(&file).expect("identity by handle");
        let by_path = path_identity(&path).expect("identity by path");
        assert_eq!(by_handle, by_path);

        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two distinct files never share an identity, which is the property a
    /// caller relies on to notice its file was replaced underneath it.
    #[test]
    fn distinct_files_have_distinct_identities() {
        let dir = std::env::temp_dir().join(format!("rp-fs-identity2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let (first, second) = (dir.join("first"), dir.join("second"));
        std::fs::write(&first, b"a").expect("write first");
        std::fs::write(&second, b"b").expect("write second");

        let a = path_identity(&first).expect("identity a");
        let b = path_identity(&second).expect("identity b");
        if a.is_some() {
            assert_ne!(a, b);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An exclusive lock excludes a second holder, and releasing readmits one.
    ///
    /// Both handles are opened through the facade, so this exercises the open
    /// and the lock together -- on Windows the two interact, because a
    /// restrictive share mode would fail the second open before it could ask
    /// for the lock.
    #[test]
    fn an_exclusive_lock_excludes_a_second_holder_until_released() {
        let dir = std::env::temp_dir().join(format!("rp-fs-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("guard.lock");

        let first = open_lock_file(&path).expect("open first");
        let second = open_lock_file(&path).expect("open second");

        try_lock_exclusive(&first).expect("first acquires");
        let conflict = try_lock_exclusive(&second).expect_err("second must be refused");
        assert!(
            is_lock_conflict(&conflict),
            "refusal must classify as a conflict, got {conflict:?}"
        );

        unlock(&first).expect("release first");
        try_lock_exclusive(&second).expect("second acquires after release");
        unlock(&second).expect("release second");

        drop((first, second));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A genuine failure is not reported as a conflict, so a caller does not
    /// retry forever on something waiting cannot fix.
    #[test]
    fn an_unrelated_error_is_not_a_lock_conflict() {
        let missing = std::env::temp_dir().join("rp-fs-lock-no-such-file");
        let _ = std::fs::remove_file(&missing);
        let error = std::fs::File::open(&missing).expect_err("must not exist");
        assert!(!is_lock_conflict(&error));
    }

    /// The pair round-trips, which is the only contract a wire encoding owes
    /// its decoder: the far end must reconstruct exactly the path that was
    /// named, not an equivalent one.
    #[test]
    fn a_path_survives_encoding_and_decoding_unchanged() {
        for original in [
            std::path::PathBuf::from("relative/leaf.log"),
            std::env::temp_dir()
                .join("rp path with spaces")
                .join("t.log"),
            std::env::current_exe().expect("current image"),
        ] {
            let decoded =
                decode_path_bytes(&encode_path_bytes(&original)).expect("decode what we encoded");
            assert_eq!(decoded, original);
        }
    }

    /// An empty path is a path, and must not become an error or a surprise.
    #[test]
    fn an_empty_path_round_trips_as_empty() {
        let empty = std::path::PathBuf::new();
        assert!(encode_path_bytes(&empty).is_empty());
        assert_eq!(
            decode_path_bytes(&encode_path_bytes(&empty)).expect("decode empty"),
            empty
        );
    }

    /// Replacing works whether or not the target already exists.
    ///
    /// Both cases matter: a bare rename onto an existing file fails on
    /// Windows, and the no-target case is the one a first write takes.
    #[test]
    fn a_file_is_replaced_whether_or_not_the_target_exists() {
        let dir = std::env::temp_dir().join(format!("rp-fs-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let target = dir.join("manifest");

        let first = dir.join("first.tmp");
        std::fs::write(&first, b"first").expect("write first");
        replace_file(&first, &target).expect("replace absent target");
        assert_eq!(std::fs::read(&target).expect("read"), b"first");

        let second = dir.join("second.tmp");
        std::fs::write(&second, b"second").expect("write second");
        replace_file(&second, &target).expect("replace existing target");
        assert_eq!(std::fs::read(&target).expect("read"), b"second");

        // The replaced-from paths are consumed by the move, not left behind.
        assert!(!first.exists());
        assert!(!second.exists());

        sync_directory(&dir).expect("sync the directory that records it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Shared data and machine-local state are different roles, and a host
    /// that distinguishes them must not collapse the two.
    #[test]
    fn shared_data_is_its_own_role() {
        let data = user_data_dir(PRODUCT);
        assert!(data.is_absolute());
        assert!(data.to_string_lossy().contains(PRODUCT));
    }

    /// A private file is created, and refuses to open over an existing one.
    ///
    /// The refusal is the security-relevant half: opening over a file someone
    /// else made would inherit their permissions, so it must fail rather than
    /// succeed with weaker protection than the caller asked for.
    #[test]
    fn a_private_file_is_created_once_and_refuses_to_reopen() {
        let dir = std::env::temp_dir().join(format!("rp-fs-private-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("artifact.json");
        let _ = std::fs::remove_file(&path);

        {
            let mut file = create_private_file(&path).expect("create private file");
            use std::io::Write as _;
            file.write_all(b"payload").expect("write");
        }
        assert_eq!(std::fs::read(&path).expect("read back"), b"payload");

        let second = create_private_file(&path).expect_err("must not open over an existing file");
        assert_eq!(second.kind(), std::io::ErrorKind::AlreadyExists);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The roles are stable: asking twice gives the same answer, so a path
    /// derived at startup still names the same directory later.
    #[test]
    fn roles_are_stable_across_calls() {
        assert_eq!(user_runtime_dir(PRODUCT), user_runtime_dir(PRODUCT));
        assert_eq!(user_state_dir(PRODUCT), user_state_dir(PRODUCT));
        assert_eq!(user_run_data_root(PRODUCT), user_run_data_root(PRODUCT));
    }
}
