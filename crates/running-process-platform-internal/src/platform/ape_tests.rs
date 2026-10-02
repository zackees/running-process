use super::*;

/// The Linux branch of a Cosmopolitan 3.x prologue, as `cosmocc` emits it,
/// trimmed of the binary bytes that precede it.
const PROLOGUE: &str = r#"MZqFpD='
' <<'justinew1b5s9'
justinew1b5s9
#'"
o=$(command -v "$0")
[ x"$1" != x--assimilate ] && type ape >/dev/null 2>&1 && exec ape "$o" "$@"
t="${TMPDIR:-${HOME:-.}}/.ape-1.10"
[ x"$1" != x--assimilate ] && [ -x "$t" ] && exec "$t" "$o" "$@"
m=$(uname -m 2>/dev/null) || m=x86_64
if [ ! -d /Applications ]; then
if [ x"$1" = x--assimilate ]; then
exit
fi
else
if [ "$m" = x86_64 ] || [ "$m" = amd64 ]; then
mkdir -p "${t%/*}" ||exit
dd if="$o" skip=111     count=222        bs=1 2>/dev/null | gzip -dc >"$t.$$" ||exit
dd if="$t.$$" of="$t.$$" skip=5 count=8 bs=64 conv=notrunc 2>/dev/null ||exit
exec "$t" "$o" "$@"
fi
fi
if [ ! -d /Applications ]; then
if [ "$m" = x86_64 ] || [ "$m" = amd64 ]; then
mkdir -p "${t%/*}" ||exit
dd if="$o" skip=754624     count=4206       bs=1 2>/dev/null | gzip -dc >"$t.$$" ||exit
exec "$t" "$o" "$@"
fi
if [ "$m" = aarch64 ] || [ "$m" = arm64 ]; then
mkdir -p "${t%/*}" ||exit
dd if="$o" skip=758830     count=4928       bs=1 2>/dev/null | gzip -dc >"$t.$$" ||exit
exec "$t" "$o" "$@"
fi
fi
echo "$0: this ape program lacks $m support" >&2
exit 127
"#;

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("scratch directory")
}

fn write_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write fixture");
    crate::ape_mark_executable(path).expect("mark fixture executable");
}

#[test]
fn every_ape_magic_is_recognized_and_nothing_else() {
    for magic in MAGICS {
        let mut header = magic.to_vec();
        header.extend_from_slice(b"\n'\n");
        assert!(
            is_ape_header(&header),
            "{:?}",
            String::from_utf8_lossy(magic)
        );
    }
    for other in [
        &b"\x7fELF\x02\x01\x01"[..],
        b"MZ\x90\x00\x03",
        b"#!/bin/sh\n",
        b"MZqFpD",
        b"",
    ] {
        assert!(!is_ape_header(other), "{other:?}");
    }
}

#[test]
fn ape_files_are_detected_by_content_and_missing_files_are_not_ape() {
    let dir = scratch();
    let ape = dir.path().join("tool.com");
    let native = dir.path().join("tool");
    write_file(&ape, PROLOGUE.as_bytes());
    write_file(&native, b"\x7fELF\x02\x01\x01\x00");
    assert!(is_ape_file(&ape));
    assert!(!is_ape_file(&native));
    assert!(!is_ape_file(&dir.path().join("missing")));
    assert!(!is_ape_file(dir.path()));
}

#[test]
fn the_linux_branch_names_each_machines_loader() {
    assert_eq!(
        embedded_loader(PROLOGUE.as_bytes(), "x86_64"),
        Some(EmbeddedLoader {
            offset: 754_624,
            len: 4206
        }),
        "the macOS x86_64 block earlier in the prologue must not be chosen"
    );
    assert_eq!(
        embedded_loader(PROLOGUE.as_bytes(), "aarch64"),
        Some(EmbeddedLoader {
            offset: 758_830,
            len: 4928
        })
    );
    assert_eq!(embedded_loader(PROLOGUE.as_bytes(), "riscv64"), None);
    assert_eq!(embedded_loader(b"MZqFpD='\n'\nexit 1\n", "x86_64"), None);
}

#[test]
fn the_prologue_cache_name_is_read_and_validated() {
    assert_eq!(
        loader_cache_name(PROLOGUE.as_bytes()).as_deref(),
        Some(".ape-1.10")
    );
    assert_eq!(
        loader_cache_name(b"t=\"${TMPDIR:-${HOME:-.}}/../../etc/passwd\""),
        None
    );
    assert_eq!(loader_cache_name(b"MZqFpD='\n"), None);
}

#[test]
fn child_environment_applies_edits_over_the_inherited_values() {
    let edits = [
        (OsStr::new("PATH"), Some(OsStr::new("/opt/bin"))),
        (OsStr::new("HOME"), None),
        (OsStr::new("UNRELATED"), Some(OsStr::new("x"))),
    ];
    let edited = ChildEnvironment::with_overrides(false, edits);
    assert_eq!(edited.path.as_deref(), Some(OsStr::new("/opt/bin")));
    assert_eq!(edited.home, None);
    assert_eq!(edited.tmpdir, ChildEnvironment::inherited().tmpdir);

    let cleared =
        ChildEnvironment::with_overrides(true, [(OsStr::new("TMPDIR"), Some(OsStr::new("/t")))]);
    assert_eq!(
        cleared,
        ChildEnvironment {
            path: None,
            tmpdir: Some(OsString::from("/t")),
            home: None,
        }
    );
    assert_eq!(cleared.prologue_cache_dir(), Some(PathBuf::from("/t")));
    let home_only = ChildEnvironment {
        tmpdir: Some(OsString::new()),
        home: Some(OsString::from("/h")),
        ..ChildEnvironment::default()
    };
    assert_eq!(home_only.prologue_cache_dir(), Some(PathBuf::from("/h")));
    assert_eq!(ChildEnvironment::default().prologue_cache_dir(), None);
}

#[test]
fn programs_resolve_like_execvp() {
    let dir = scratch();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    write_file(&bin.join("tool"), PROLOGUE.as_bytes());
    let environment = ChildEnvironment {
        path: Some(std::env::join_paths([dir.path(), &bin]).unwrap()),
        ..ChildEnvironment::default()
    };

    assert_eq!(
        resolve_program(OsStr::new("tool"), None, &environment),
        Some(bin.join("tool"))
    );
    assert_eq!(
        resolve_program(OsStr::new("absent"), None, &environment),
        None
    );
    assert_eq!(
        resolve_program(OsStr::new("tool"), None, &ChildEnvironment::default()),
        None
    );
    assert_eq!(
        resolve_program(OsStr::new("bin/tool"), Some(dir.path()), &environment),
        Some(bin.join("tool"))
    );
    assert_eq!(
        resolve_program(OsStr::new("/abs/tool"), Some(dir.path()), &environment),
        Some(PathBuf::from("/abs/tool"))
    );
}

#[test]
fn a_native_program_has_no_ape_plan() {
    let dir = scratch();
    let native = dir.path().join("native");
    write_file(&native, b"\x7fELF\x02\x01\x01\x00");
    let environment = ChildEnvironment::default();
    assert_eq!(plan_launch(native.as_os_str(), None, &environment), None);
    assert_eq!(
        plan_launch(dir.path().join("missing").as_os_str(), None, &environment),
        None
    );
}

#[test]
fn an_installed_ape_loader_is_preferred_and_receives_the_image_first() {
    let dir = scratch();
    let image = dir.path().join("tool.com");
    write_file(&image, PROLOGUE.as_bytes());
    let loaders = dir.path().join("loaders");
    std::fs::create_dir(&loaders).unwrap();
    write_file(&loaders.join("ape"), b"loader");
    let environment = ChildEnvironment {
        path: Some(std::env::join_paths([dir.path(), &loaders]).unwrap()),
        ..ChildEnvironment::default()
    };

    let planned = plan_launch(OsStr::new("tool.com"), None, &environment);
    if !NEEDS_LOADER {
        assert_eq!(planned, None, "this host runs APE images natively");
        return;
    }
    let launch = planned.expect("APE image with a loader on PATH");
    assert_eq!(launch.kind, LoaderKind::System);
    assert_eq!(launch.loader, loaders.join("ape"));
    assert_eq!(launch.image, image);
    assert_eq!(
        launch.args(["--flag", "value"]),
        vec![
            image.into_os_string(),
            OsString::from("--flag"),
            OsString::from("value")
        ]
    );
}

#[test]
fn a_spawn_error_that_is_not_a_refused_image_is_never_retried() {
    let dir = scratch();
    let image = dir.path().join("tool.com");
    write_file(&image, PROLOGUE.as_bytes());
    let mut command = std::process::Command::new(&image);
    let not_found = io::Error::from(io::ErrorKind::NotFound);
    assert!(!is_exec_format_error(&not_found));
    assert!(!prepare_std_retry(&mut command, &not_found));
}

#[cfg(feature = "ape-loader")]
mod embedded {
    use super::*;

    /// Frame raw deflate output as a gzip member with every optional header.
    pub(super) fn gzip(payload: &[u8]) -> Vec<u8> {
        let mut member = vec![0x1f, 0x8b, 8, 0x04 | 0x08 | 0x10 | 0x02, 0, 0, 0, 0, 0, 3];
        member.extend_from_slice(&3u16.to_le_bytes());
        member.extend_from_slice(b"xyz");
        member.extend_from_slice(b"loader\0");
        member.extend_from_slice(b"comment\0");
        member.extend_from_slice(&[0, 0]);
        member.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(payload, 6));
        member.extend_from_slice(&0u32.to_le_bytes());
        member.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        member
    }

    /// An image whose prologue points every machine at `payload`.
    pub(super) fn image_with_loader(path: &Path, payload: &[u8]) {
        let compressed = gzip(payload);
        let mut prologue = Vec::new();
        let header = |offset: usize| {
            format!(
                "MZqFpD='\n'\nt=\"${{TMPDIR:-${{HOME:-.}}}}/.ape-9.99\"\nif [ ! -d /Applications ]; then\n\
                 if [ \"$m\" = {machine} ] || [ \"$m\" = amd64 ]; then\n\
                 dd if=\"$o\" skip={offset:<10} count={len:<10} bs=1 2>/dev/null | gzip -dc\n\
                 fi\nfi\nexit 127\n",
                machine = host_machine(),
                len = compressed.len(),
            )
        };
        // The offset is printed padded, so the header length does not depend
        // on its value.
        let offset = header(0).len();
        prologue.extend_from_slice(header(offset).as_bytes());
        assert_eq!(prologue.len(), offset);
        prologue.extend_from_slice(&compressed);
        write_file(path, &prologue);
    }

    #[test]
    fn gunzip_reads_a_member_with_every_optional_field() {
        let payload = b"\x7fELF loader bytes ".repeat(100);
        assert_eq!(gunzip(&gzip(&payload), 1 << 20).unwrap(), payload);
    }

    #[test]
    fn gunzip_refuses_corrupt_and_oversized_members() {
        let payload = vec![7u8; 4096];
        let member = gzip(&payload);
        assert!(gunzip(&member, 100).is_err(), "over the limit");
        assert!(gunzip(&member[..12], 1 << 20).is_err(), "truncated");
        let mut wrong_size = member.clone();
        let at = wrong_size.len() - 4;
        wrong_size[at] ^= 1;
        assert!(gunzip(&wrong_size, 1 << 20).is_err(), "trailer size");
        assert!(gunzip(b"not gzip at all, clearly", 1 << 20).is_err());
    }

    #[test]
    fn the_embedded_loader_is_extracted_for_this_machine() {
        let dir = scratch();
        let image = dir.path().join("tool.com");
        image_with_loader(&image, b"\x7fELF fake loader");
        assert_eq!(
            extract_embedded_loader(&image, host_machine()).unwrap(),
            Some(b"\x7fELF fake loader".to_vec())
        );
        assert_eq!(extract_embedded_loader(&image, "pdp11").unwrap(), None);
    }

    #[test]
    fn the_loader_lands_where_the_prologue_looks_and_is_reused_only_if_identical() {
        let dir = scratch();
        let image = dir.path().join("tool.com");
        image_with_loader(&image, b"\x7fELF fake loader");
        let cache = dir.path().join("cache");
        let environment = ChildEnvironment {
            tmpdir: Some(cache.clone().into_os_string()),
            ..ChildEnvironment::default()
        };
        if !EMBEDDED_LOADER {
            assert_eq!(
                materialize_embedded_loader(&image, &environment, false),
                None
            );
            return;
        }

        let loader =
            materialize_embedded_loader(&image, &environment, false).expect("loader materialized");
        assert_eq!(loader, cache.join(".ape-9.99"));
        assert_eq!(std::fs::read(&loader).unwrap(), b"\x7fELF fake loader");

        std::fs::write(&loader, b"tampered").unwrap();
        assert_eq!(
            materialize_embedded_loader(&image, &environment, false),
            Some(loader.clone())
        );
        assert_eq!(std::fs::read(&loader).unwrap(), b"\x7fELF fake loader");

        assert_eq!(
            materialize_embedded_loader(&image, &ChildEnvironment::default(), false),
            None,
            "without TMPDIR or HOME the prologue would look in its working directory"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&cache)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![OsString::from(".ape-9.99")]);
    }

    #[test]
    fn a_payload_that_is_not_an_elf_loader_is_never_materialized() {
        let dir = scratch();
        let image = dir.path().join("tool.com");
        image_with_loader(&image, b"#!/bin/sh\necho not a loader\n");
        let environment = ChildEnvironment {
            tmpdir: Some(dir.path().as_os_str().to_os_string()),
            ..ChildEnvironment::default()
        };
        assert_eq!(
            materialize_embedded_loader(&image, &environment, false),
            None
        );
    }
}
