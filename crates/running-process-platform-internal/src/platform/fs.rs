//! Runtime-artifact identity, permissions, secure-open, replacement, and directory primitives.
//!
//! Callers name the *role* a directory plays for their product -- ephemeral
//! runtime artifacts, persistent state, per-run scratch. Which location on this
//! host plays that role, and how two accounts are kept apart there, is decided
//! here. Callers still own their own layout beneath it: leaf names, extensions,
//! and subdirectories are product conventions, not host mechanics.

#[cfg(feature = "fs")]
pub use crate::{
    fs_file_identity as file_identity, fs_path_identity as path_identity,
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

    /// The roles are stable: asking twice gives the same answer, so a path
    /// derived at startup still names the same directory later.
    #[test]
    fn roles_are_stable_across_calls() {
        assert_eq!(user_runtime_dir(PRODUCT), user_runtime_dir(PRODUCT));
        assert_eq!(user_state_dir(PRODUCT), user_state_dir(PRODUCT));
        assert_eq!(user_run_data_root(PRODUCT), user_run_data_root(PRODUCT));
    }
}
