//! Runtime-artifact identity, permissions, secure-open, replacement, and directory primitives.
//!
//! Callers name the *role* a directory plays for their product -- ephemeral
//! runtime artifacts, persistent state, per-run scratch. Which location on this
//! host plays that role, and how two accounts are kept apart there, is decided
//! here. Callers still own their own layout beneath it: leaf names, extensions,
//! and subdirectories are product conventions, not host mechanics.

#[cfg(feature = "fs")]
pub use crate::{
    fs_user_run_data_root as user_run_data_root, fs_user_runtime_dir as user_runtime_dir,
    fs_user_state_dir as user_state_dir,
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

    /// The roles are stable: asking twice gives the same answer, so a path
    /// derived at startup still names the same directory later.
    #[test]
    fn roles_are_stable_across_calls() {
        assert_eq!(user_runtime_dir(PRODUCT), user_runtime_dir(PRODUCT));
        assert_eq!(user_state_dir(PRODUCT), user_state_dir(PRODUCT));
        assert_eq!(user_run_data_root(PRODUCT), user_run_data_root(PRODUCT));
    }
}
