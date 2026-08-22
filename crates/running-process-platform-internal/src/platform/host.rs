//! Host facts, directories, user identity, resources, and autostart primitives.
//!
//! Callers ask what is true of this host and this process -- who am I, am I
//! elevated -- and decide for themselves what that means. Whether the answer
//! came from a uid comparison or a token query is not something a caller
//! should have to know, or be able to tell.

pub use crate::{
    host_current_process_privilege as current_process_privilege,
    HostPrivilegedIdentity as PrivilegedIdentity,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// A test process is not the machine's system account.
    ///
    /// Asserted as the property rather than against a uid or a SID: the point
    /// of the facade is that a caller cannot tell which host answered. A run
    /// that really is elevated is a broken environment, and this failing is
    /// the correct outcome there.
    #[test]
    fn an_ordinary_test_process_is_not_privileged() {
        let privilege = current_process_privilege().expect("privilege lookup must succeed");
        assert_eq!(
            privilege, None,
            "test runs are expected unprivileged; got {privilege:?}"
        );
    }

    /// Each identity prints the detail an operator needs to recognise it.
    #[test]
    fn privileged_identities_describe_themselves_concretely() {
        assert_eq!(
            PrivilegedIdentity::UnixRoot.to_string(),
            "root (effective uid 0)"
        );
        assert_eq!(
            PrivilegedIdentity::WindowsLocalSystem.to_string(),
            "Windows LocalSystem (S-1-5-18)"
        );
    }
}
