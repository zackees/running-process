//! Content-hash primitive: blake3 of a file's *bytes* (#891).
//!
//! soldr-daemon, `FastLED/fbuild`, and standalone zccache all obtain their
//! daemon identity/discovery through running-process, and all three hit the
//! same failure in **dev**: two builds sharing one home root rendezvous on the
//! same daemon pipe + pid file, each sees the other as "stale-version", and
//! displaces it on every invocation — a `displace-stale` war that wedges the
//! compile daemon. The shims are already version-namespaced; the daemon
//! *identity* is not. Rather than reimplement isolation in each consumer, this
//! module provides the shared primitive: a content hash of a file, so a dev
//! build can stamp its own identity with `"<version>-<first-16-hex of
//! blake3_file(current_exe)>"`. Distinct dev builds → distinct identities → no
//! cross-build displacement. (Full root-cause + evidence: zackees/soldr#2352.)
//!
//! ## Why the bytes, and why mmap the file (not the loaded image)
//!
//! - **Hash the bytes, not the path string.** Path-string hashing returns the
//!   same value across rebuilds → no isolation. The file's contents change
//!   every build → the identity changes every build (isolating same-*version*
//!   rebuilds), which is the whole point.
//! - **mmap the file, do NOT hash the in-memory mapped image.** The loaded
//!   module is mutated by ASLR base relocations, the resolved IAT, and live
//!   `.data`/`.bss`, so it differs from the file **and differs every run**
//!   (ASLR) → effectively a nonce → non-reproducible identity.
//!   [`blake3::Hasher::update_mmap_rayon`] on the file is page-cache-warm (the
//!   exe just executed) → memory-speed, no `read()` copy, multi-core. A 20 MB
//!   binary is ~1–3 ms this way (vs ~20 ms for a naive read), paid at most
//!   once per build (compute the stamp once and propagate the *value* down the
//!   process tree — see the issue for the client/daemon agreement).

use std::io;
use std::path::Path;

/// The blake3 digest type, re-exported so callers of [`blake3_file`] can name
/// the return type without taking their own `blake3` dependency.
pub use blake3::Hash;

/// blake3 of the **file's bytes** at `path` (open → mmap → hash, multi-core).
///
/// This hashes the on-disk contents, not the path string and not the
/// in-memory mapped image — see the [module docs](self) for why that
/// distinction is the whole point of the primitive.
///
/// Uses [`blake3::Hasher::update_mmap_rayon`]: the file is memory-mapped
/// (page-cache-warm for a just-executed binary) and hashed across all cores.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the file cannot be opened or mapped
/// (e.g. it does not exist, or permission is denied).
pub fn blake3_file(path: &Path) -> io::Result<Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_mmap_rayon(path)?;
    Ok(hasher.finalize())
}

/// Longest stamp accepted from the environment. The stamp is spelled into
/// pipe and pid-file names, which have their own length limits.
#[cfg(feature = "client")]
const MAX_STAMP_LEN: usize = 64;

/// Whether an inherited stamp is safe to spell into a pipe or pid-file name.
///
/// Rejects path separators, NUL and anything else outside the characters a
/// `<version>-<hex>` stamp uses, because a hostile or mangled value would
/// otherwise reach the filesystem namespace.
#[cfg(feature = "client")]
fn is_safe_stamp(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_STAMP_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
}

/// First 16 hex characters of `blake3_file(path)`.
#[cfg(feature = "client")]
fn file_hex16(path: &Path) -> io::Result<String> {
    Ok(blake3_file(path)?.to_hex()[..16].to_owned())
}

/// `<version>-<hex16>`.
#[cfg(feature = "client")]
fn stamp_from_hex(version: &str, hex16: &str) -> String {
    format!("{version}-{hex16}")
}

/// The stamp decision, with every input passed in so it is testable without
/// touching the process environment.
///
/// Outside dev scope there is no stamp, whatever `inherited` holds: release
/// scope keeps its bare identity and single-daemon upgrade semantics. In dev
/// scope a safe inherited value is used verbatim (no re-hash); an unsafe one is
/// recomputed rather than trusted.
#[cfg(feature = "client")]
fn resolve_stamp(
    dev_scope: bool,
    inherited: Option<String>,
    compute: impl FnOnce() -> io::Result<String>,
) -> io::Result<Option<String>> {
    if !dev_scope {
        return Ok(None);
    }
    match inherited {
        Some(value) if is_safe_stamp(&value) => Ok(Some(value)),
        _ => compute().map(Some),
    }
}

/// The dev-scope daemon identity stamp for the calling tool (#1252), or `None`
/// outside dev scope.
///
/// `version` is the *calling tool's* version, not running-process's: the stamp
/// identifies the consumer binary (soldr, zccache, ...). When
/// `RUNNING_PROCESS_DAEMON_IDENTITY_STAMP` is already set to a safe value it is
/// returned as-is, so a 200-process build hashes once, not 200 times. Otherwise
/// the stamp is computed from the running executable's bytes and cached for
/// the life of the process.
///
/// This only *computes* the value. Putting it on child processes is the
/// caller's job (see [`daemon_identity_stamp_env`]); a library mutating the
/// process environment is unsound in a multithreaded host.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] if the executable cannot be located or
/// hashed.
#[cfg(feature = "client")]
pub fn daemon_identity_stamp(version: &str) -> io::Result<Option<String>> {
    static EXE_HEX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let dev_scope = crate::env_vars::DAEMON_SCOPE
        .text()
        .is_some_and(|scope| scope.eq_ignore_ascii_case("dev"));
    resolve_stamp(
        dev_scope,
        crate::env_vars::DAEMON_IDENTITY_STAMP.text(),
        || {
            let hex = match EXE_HEX.get() {
                Some(hex) => hex.clone(),
                None => {
                    let hex = file_hex16(&std::env::current_exe()?)?;
                    EXE_HEX.get_or_init(|| hex).clone()
                }
            };
            Ok(stamp_from_hex(version, &hex))
        },
    )
}

/// [`daemon_identity_stamp`] as the `(name, value)` pair to put on a child
/// [`std::process::Command`] with `.env(name, value)`.
///
/// # Errors
///
/// As [`daemon_identity_stamp`].
#[cfg(feature = "client")]
pub fn daemon_identity_stamp_env(version: &str) -> io::Result<Option<(&'static str, String)>> {
    Ok(daemon_identity_stamp(version)?
        .map(|value| (crate::env_vars::DAEMON_IDENTITY_STAMP.name, value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rp-content-hash-{}-{}-{}",
            std::process::id(),
            name,
            bytes.len()
        ));
        let mut f = std::fs::File::create(&path).expect("create temp file");
        f.write_all(bytes).expect("write temp file");
        f.flush().expect("flush temp file");
        path
    }

    #[test]
    fn hashes_the_bytes_not_the_path() {
        // The digest must equal a plain blake3 hash of the same bytes: proof
        // we hash file *contents*, independent of where the file lives.
        let bytes = b"the quick brown fox jumps over the lazy dog";
        let path = write_temp("bytes", bytes);
        let got = blake3_file(&path).expect("hash temp file");
        std::fs::remove_file(&path).ok();
        assert_eq!(got, blake3::hash(bytes));
    }

    #[test]
    fn same_contents_at_different_paths_hash_equal() {
        // Two files with identical bytes but different paths must hash equal —
        // this is what lets two worktrees / two dev builds of the same content
        // resolve to the same identity, and different content to different.
        let bytes = b"identical contents";
        let a = write_temp("dup-a", bytes);
        let b = write_temp("dup-b", bytes);
        let ha = blake3_file(&a).expect("hash a");
        let hb = blake3_file(&b).expect("hash b");
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
        assert_eq!(ha, hb, "content-based hash must ignore the path");
    }

    #[test]
    fn different_contents_hash_differently() {
        let a = write_temp("diff-a", b"content one");
        let b = write_temp("diff-b", b"content two");
        let ha = blake3_file(&a).expect("hash a");
        let hb = blake3_file(&b).expect("hash b");
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
        assert_ne!(ha, hb);
    }

    #[test]
    fn empty_file_hashes_like_empty_input() {
        let path = write_temp("empty", b"");
        let got = blake3_file(&path).expect("hash empty file");
        std::fs::remove_file(&path).ok();
        assert_eq!(got, blake3::hash(b""));
    }

    #[test]
    fn first_16_hex_is_a_stable_stamp() {
        // The documented consumer usage: `<version>-<first 16 hex chars>`.
        let bytes = b"stamp me";
        let path = write_temp("stamp", bytes);
        let hash = blake3_file(&path).expect("hash temp file");
        std::fs::remove_file(&path).ok();
        let hex = hash.to_hex();
        let stamp16 = &hex[..16];
        assert_eq!(stamp16.len(), 16);
        assert!(stamp16.chars().all(|c| c.is_ascii_hexdigit()));
        // Recomputing over identical bytes yields the same stamp.
        assert_eq!(stamp16, &blake3::hash(bytes).to_hex()[..16]);
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rp-content-hash-does-not-exist-{}",
            std::process::id()
        ));
        let err = blake3_file(&path).expect_err("missing file must error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[cfg(feature = "client")]
    mod stamp {
        use super::super::*;
        use super::write_temp;

        fn never_computed() -> io::Result<String> {
            panic!("an inherited stamp must not be re-hashed")
        }

        #[test]
        fn stamp_is_none_outside_dev_scope_even_when_inherited() {
            let inherited = Some("4.1.0-deadbeefdeadbeef".to_owned());
            assert_eq!(
                resolve_stamp(false, inherited, never_computed).unwrap(),
                None
            );
            assert_eq!(resolve_stamp(false, None, never_computed).unwrap(), None);
        }

        #[test]
        fn stamp_uses_inherited_value_verbatim_without_hashing() {
            let inherited = Some("4.1.0-deadbeefdeadbeef".to_owned());
            assert_eq!(
                resolve_stamp(true, inherited, never_computed).unwrap(),
                Some("4.1.0-deadbeefdeadbeef".to_owned())
            );
        }

        #[test]
        fn stamp_is_computed_when_dev_scope_has_nothing_inherited() {
            let got = resolve_stamp(true, None, || Ok("1.2.3-0123456789abcdef".into())).unwrap();
            assert_eq!(got, Some("1.2.3-0123456789abcdef".to_owned()));
        }

        #[test]
        fn unsafe_inherited_value_is_recomputed_not_trusted() {
            for bad in ["", "a/b", "a\\b", "nul\0byte", "has space", &"x".repeat(65)] {
                let got =
                    resolve_stamp(true, Some(bad.to_owned()), || Ok("1.0.0-cafe".into())).unwrap();
                assert_eq!(got, Some("1.0.0-cafe".to_owned()), "{bad:?}");
            }
        }

        #[test]
        fn computed_stamp_is_version_dash_sixteen_hex_of_the_file_bytes() {
            let path = write_temp("stamp-shape", b"some binary");
            let stamp = stamp_from_hex("4.1.0", &file_hex16(&path).unwrap());
            std::fs::remove_file(&path).ok();
            let expected = blake3::hash(b"some binary").to_hex();
            assert_eq!(stamp, format!("4.1.0-{}", &expected[..16]));
            assert!(is_safe_stamp(&stamp));
        }

        #[test]
        fn different_bytes_give_different_stamps_and_same_bytes_the_same() {
            let a = write_temp("stamp-a", b"build one");
            let b = write_temp("stamp-b", b"build two");
            let c = write_temp("stamp-c", b"build one");
            let stamp = |path| stamp_from_hex("4.1.0", &file_hex16(path).unwrap());
            let (sa, sb, sc) = (stamp(&a), stamp(&b), stamp(&c));
            for path in [&a, &b, &c] {
                std::fs::remove_file(path).ok();
            }
            assert_ne!(sa, sb, "same version, different bytes must not collide");
            assert_eq!(sa, sc, "the path must not matter");
        }
    }
}
