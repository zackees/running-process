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
}
