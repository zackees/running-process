//! Host facts, directories, user identity, resources, and autostart primitives.
//!
//! Callers ask what is true of this host and this process -- who am I, am I
//! elevated -- and decide for themselves what that means. Whether the answer
//! came from a uid comparison or a token query is not something a caller
//! should have to know, or be able to tell.

use std::io;

pub use crate::{
    host_current_process_privilege as current_process_privilege,
    host_user_machine_identity as user_machine_identity,
    HostPrivilegedIdentity as PrivilegedIdentity,
};

/// Resolve a machine identity from the first readable of `machine_id_paths`,
/// falling back to a boot-scoped id.
///
/// Lives in the neutral leaf, not the Linux tree, so it compiles and is tested
/// on every host. The rules it encodes are subtle enough to be worth testing
/// where the tests actually run, and only the Linux implementation supplies
/// real paths to it.
// Only the Linux implementation supplies real paths to this, so other
// hosts see it as dead code. It stays compiled on all of them anyway:
// that is what keeps the tests below running everywhere rather than on
// one host.
#[allow(dead_code)]
pub(crate) fn machine_id_from(machine_id_paths: &[&str], boot_id_path: &str) -> io::Result<String> {
    for path in machine_id_paths {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            // An unreadable machine-id stays a hard error rather than falling
            // through: sibling processes of the same user may read the file
            // fine, and deriving a different identity here would split the
            // user across two identities -- two brokers, each believing it is
            // the singleton.
            Err(err) => return Err(io::Error::other(format!("read {path}: {err}"))),
        }
    }
    // Read-only fallback for hosts that ship no machine-id file at all
    // (minimal containers, machine-id-less musl distros): a boot-scoped
    // identity from the kernel's boot_id. Every process in the same boot
    // derives the same value -- exactly the lifetime this must cover -- and
    // file *absence*, unlike readability, cannot differ between one user's
    // processes, so the fallback stays consistent.
    if let Ok(s) = std::fs::read_to_string(boot_id_path) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Ok(format!("boot:{trimmed}"));
        }
    }
    Err(io::Error::other(
        "no /etc/machine-id or /var/lib/dbus/machine-id found, and no usable boot_id fallback",
    ))
}

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

    mod machine_id_sources {
        use super::super::machine_id_from;

        fn temp_dir(label: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "rp-host-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id(),
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            dir
        }

        fn write(dir: &std::path::Path, name: &str, content: &str) -> String {
            let path = dir.join(name);
            std::fs::write(&path, content).expect("write fixture file");
            path.to_string_lossy().into_owned()
        }

        #[test]
        fn machine_id_file_wins_over_boot_fallback() {
            let dir = temp_dir("wins");
            let machine = write(
                &dir,
                "machine-id",
                "  abc123
",
            );
            let boot = write(
                &dir, "boot-id", "zzz
",
            );
            assert_eq!(
                machine_id_from(&[&machine], &boot).expect("resolve"),
                "abc123"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn second_path_is_consulted_when_first_is_missing() {
            let dir = temp_dir("second");
            let missing = dir.join("absent").to_string_lossy().into_owned();
            let machine = write(
                &dir,
                "machine-id",
                "def456
",
            );
            let boot = write(
                &dir, "boot-id", "zzz
",
            );
            assert_eq!(
                machine_id_from(&[&missing, &machine], &boot).expect("resolve"),
                "def456"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn missing_machine_id_files_fall_back_to_boot_id() {
            let dir = temp_dir("fallback");
            let missing = dir.join("absent").to_string_lossy().into_owned();
            let boot = write(
                &dir,
                "boot-id",
                "boot-value
",
            );
            assert_eq!(
                machine_id_from(&[&missing], &boot).expect("resolve"),
                "boot:boot-value"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn empty_machine_id_file_falls_through_to_boot_id() {
            let dir = temp_dir("empty");
            let machine = write(
                &dir,
                "machine-id",
                "   
",
            );
            let boot = write(
                &dir,
                "boot-id",
                "boot-value
",
            );
            assert_eq!(
                machine_id_from(&[&machine], &boot).expect("resolve"),
                "boot:boot-value"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn unreadable_machine_id_stays_a_hard_error_despite_boot_fallback() {
            let dir = temp_dir("unreadable");
            // A directory in the machine-id slot yields a non-NotFound read
            // error -- the split-identity hazard the hard error protects.
            let as_dir = dir.join("machine-id-dir");
            std::fs::create_dir_all(&as_dir).expect("create dir fixture");
            let as_dir = as_dir.to_string_lossy().into_owned();
            let boot = write(&dir, "boot-id", "boot-uuid\n");
            machine_id_from(&[&as_dir], &boot)
                .expect_err("unreadable machine-id must not fall through");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn everything_missing_is_an_error() {
            let dir = temp_dir("nothing");
            let missing = dir.join("absent").to_string_lossy().into_owned();
            let no_boot = dir.join("absent-boot").to_string_lossy().into_owned();
            assert!(machine_id_from(&[&missing], &no_boot).is_err());
            let _ = std::fs::remove_dir_all(&dir);
        }
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
