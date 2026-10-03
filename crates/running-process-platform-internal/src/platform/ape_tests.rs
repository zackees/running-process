use super::*;

/// Minimal APE-shaped script: the `MZqFpD='...'` header is a shell
/// assignment, exactly like a real cosmocc image's prologue.
const FAKE_APE: &str = "MZqFpD='\n'\necho fake-ape \"$@\"\n";

/// The checked-in cosmocc hello-world (`tests/data/ape-hello`): a fat
/// x86_64 + aarch64 image that prints `hello world` and its arguments.
pub(crate) fn hello_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ape-hello/hello.com")
}

fn write_exe(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    crate::ape_mark_executable(&path).unwrap();
    path
}

fn options(path: Option<&OsStr>, loader: Option<&str>) -> ApeOptions {
    ApeOptions {
        path: path.map(OsStr::to_os_string),
        loader: loader.map(OsString::from),
        cache_dirs: Vec::new(),
    }
}

#[test]
fn recognizes_every_ape_magic_and_rejects_other_formats() {
    assert!(is_ape_header(b"MZqFpD='\n\n\0"));
    assert!(is_ape_header(b"jartsr='\n"));
    assert!(is_ape_header(b"APEDBG='\n"));
    assert!(!is_ape_header(b"MZ\x90\0\x03\0\0\0")); // plain PE
    assert!(!is_ape_header(b"\x7fELF\x02\x01\x01\0")); // ELF
    assert!(!is_ape_header(b"#!/bin/sh\n"));
    assert!(!is_ape_header(b"MZqF"));
    assert!(!is_ape_header(b""));
}

#[test]
fn ape_files_are_detected_by_content_and_unreadable_files_are_not_ape() {
    let dir = tempfile::tempdir().unwrap();
    assert!(is_ape_file(&hello_fixture()));
    assert!(is_ape_file(&write_exe(dir.path(), "fake", FAKE_APE)));
    assert!(!is_ape_file(&write_exe(
        dir.path(),
        "script",
        "#!/bin/sh\n"
    )));
    assert!(!is_ape_file(&dir.path().join("missing")));
    assert!(!is_ape_file(dir.path()));
}

#[test]
fn a_host_that_runs_ape_natively_plans_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let ape = write_exe(dir.path(), "tool", FAKE_APE);
    let plan = plan_launch(ape.as_os_str(), None, &options(None, Some("/bin/sh")));
    assert_eq!(plan.is_none(), !NEEDS_LOADER);
}

#[test]
fn a_program_that_is_not_ape_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_exe(dir.path(), "tool", "#!/bin/sh\necho hi\n");
    let explicit = options(None, Some("/bin/sh"));
    assert_eq!(plan_launch(script.as_os_str(), None, &explicit), None);
    assert_eq!(
        plan_launch(dir.path().join("missing").as_os_str(), None, &explicit),
        None
    );
}

#[test]
fn an_explicit_loader_wins() {
    if !NEEDS_LOADER {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ape = write_exe(dir.path(), "tool", FAKE_APE);
    let plan = plan_launch(
        ape.as_os_str(),
        None,
        &options(None, Some("/opt/cosmo/ape")),
    )
    .expect("APE must be planned");
    assert_eq!(plan.kind, LoaderKind::Explicit);
    assert_eq!(plan.loader, PathBuf::from("/opt/cosmo/ape"));
    assert_eq!(plan.image, ape);
    assert_eq!(
        plan.args(["a", "b c"]),
        vec![
            ape.into_os_string(),
            OsString::from("a"),
            OsString::from("b c")
        ]
    );
}

#[test]
fn a_bare_name_resolves_on_path_and_finds_an_ape_loader_there() {
    if !NEEDS_LOADER {
        return;
    }
    let bin = tempfile::tempdir().unwrap();
    // A shell-only image carries no embedded loader, so `ape` is next.
    let ape = write_exe(bin.path(), "tool", FAKE_APE);
    let loader = write_exe(bin.path(), "ape", "#!/bin/sh\n");
    let plan = plan_launch(
        OsStr::new("tool"),
        None,
        &options(Some(bin.path().as_os_str()), None),
    )
    .expect("APE on PATH must be planned");
    assert_eq!(plan.image, ape);
    assert_eq!(plan.kind, LoaderKind::System);
    assert_eq!(plan.loader, loader);
}

#[test]
fn without_any_ape_loader_the_shell_runs_the_prologue() {
    if !NEEDS_LOADER || !Path::new(SHELL).is_file() {
        return;
    }
    if SYSTEM_LOADERS
        .iter()
        .any(|loader| Path::new(loader).exists())
    {
        return; // an installed `ape` correctly wins on this host
    }
    let dir = tempfile::tempdir().unwrap();
    let ape = write_exe(dir.path(), "tool", FAKE_APE);
    let plan = plan_launch(ape.as_os_str(), None, &options(None, None)).expect("planned");
    assert_eq!(plan.kind, LoaderKind::Shell);
    assert_eq!(plan.loader, PathBuf::from(SHELL));
}

#[test]
fn a_relative_program_resolves_against_the_child_cwd() {
    if !NEEDS_LOADER {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("bin")).unwrap();
    let ape = write_exe(&dir.path().join("bin"), "tool", FAKE_APE);
    let plan = plan_launch(
        OsStr::new("bin/tool"),
        Some(dir.path()),
        &options(None, Some("/bin/sh")),
    )
    .expect("relative APE must be planned");
    assert_eq!(plan.image, ape);
}

#[test]
fn options_apply_child_environment_edits() {
    let edits = [
        (OsStr::new("PATH"), Some(OsStr::new("/opt/bin"))),
        (OsStr::new(LOADER_ENV), Some(OsStr::new("/opt/ape"))),
        (OsStr::new(CACHE_DIR_ENV), Some(OsStr::new("/c"))),
        (OsStr::new("UNRELATED"), Some(OsStr::new("x"))),
    ];
    let edited = ApeOptions::with_overrides(true, edits);
    assert_eq!(edited.path.as_deref(), Some(OsStr::new("/opt/bin")));
    assert_eq!(edited.loader.as_deref(), Some(OsStr::new("/opt/ape")));
    assert_eq!(edited.cache_dirs.first(), Some(&PathBuf::from("/c")));
    assert_eq!(
        &edited.cache_dirs[1..],
        crate::ape_default_loader_dirs().as_slice(),
        "host defaults follow the requested directory"
    );

    let cleared = ApeOptions::with_overrides(true, [(OsStr::new("PATH"), None)]);
    assert_eq!(cleared.path, None);
    assert_eq!(cleared.loader, None);
    let empty = ApeOptions::with_overrides(true, [(OsStr::new(LOADER_ENV), Some(OsStr::new("")))]);
    assert_eq!(empty.loader, None, "an empty override is unset");
}

#[test]
fn a_spawn_error_that_is_not_a_refused_image_is_never_retried() {
    let dir = tempfile::tempdir().unwrap();
    let image = write_exe(dir.path(), "tool", FAKE_APE);
    let mut command = std::process::Command::new(&image);
    let not_found = io::Error::from(io::ErrorKind::NotFound);
    assert!(!is_exec_format_error(&not_found));
    assert!(!prepare_std_retry(&mut command, &not_found));
}

#[test]
fn prologue_parse_selects_the_linux_branch_per_cpu() {
    let mut prologue = Vec::new();
    File::open(hello_fixture())
        .unwrap()
        .take(64 * 1024)
        .read_to_end(&mut prologue)
        .unwrap();
    // The macOS x86_64 branch shares the blob but patches it; the Linux one
    // is the last plain extraction.
    assert_eq!(
        loader_blob_range(&prologue, "x86_64"),
        Some((275_392, 4180))
    );
    assert_eq!(
        loader_blob_range(&prologue, "aarch64"),
        Some((279_572, 4928))
    );
    assert_eq!(loader_blob_range(&prologue, "riscv64"), None);
}

#[test]
fn the_fixture_prologue_yields_every_branch() {
    let mut prologue = Vec::new();
    File::open(hello_fixture())
        .unwrap()
        .take(64 * 1024)
        .read_to_end(&mut prologue)
        .unwrap();
    let parsed = Prologue::parse(&prologue);
    assert_eq!(parsed.linux_loader_x86_64, Some((275_392, 4180)));
    assert_eq!(parsed.linux_loader_aarch64, Some((279_572, 4928)));
    // The macOS x86_64 branch shares the Linux blob, then patches it.
    assert_eq!(parsed.macos_loader_x86_64, Some((275_392, 4180)));
    assert_eq!(parsed.macos_loader_source_aarch64, Some((284_500, 10_590)));
    assert_eq!(
        Prologue::parse(b"MZqFpD='\n\0\xff\xfe random\n"),
        Prologue::default()
    );
}

#[test]
fn the_fork_lock_admits_spawns_together_and_writers_alone() {
    let first = fork_guard();
    let second = fork_guard();
    drop((first, second));
    drop(exclusive_fork_guard());
    let mut attempts = 0;
    let result = retry_while_busy(|| {
        attempts += 1;
        if attempts < 3 {
            Err(io::Error::from(io::ErrorKind::ExecutableFileBusy))
        } else {
            Ok(attempts)
        }
    });
    assert_eq!(result.unwrap(), 3);
}

#[cfg(feature = "ape-loader")]
mod embedded {
    use super::*;

    fn gzip(payload: &[u8]) -> Vec<u8> {
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
    fn both_cpus_loaders_are_extracted_from_the_fat_fixture() {
        let x86 = extract_loader(&hello_fixture(), "x86_64").expect("x86_64 loader");
        let arm = extract_loader(&hello_fixture(), "aarch64").expect("aarch64 loader");
        assert!(x86.starts_with(b"\x7fELF") && arm.starts_with(b"\x7fELF"));
        assert_ne!(x86, arm);
        assert_eq!(extract_loader(&hello_fixture(), "riscv64"), None);
    }

    #[test]
    fn the_macos_loaders_are_extracted_on_any_host() {
        let macho = extract_macos_x86_64_loader(&hello_fixture()).expect("x86_64 Mach-O loader");
        assert!(macho.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]), "MH_MAGIC_64");
        assert_eq!(macho[4..8], [7, 0, 0, 1], "CPU_TYPE_X86_64");
        let source =
            extract_macos_aarch64_loader_source(&hello_fixture()).expect("ape-m1.c source");
        let source = String::from_utf8(source).expect("C source is text");
        assert!(
            source.contains("main"),
            "the loader source has an entry point"
        );
    }

    #[test]
    fn install_reuses_identical_files_and_refuses_shared_directories() {
        if LOADER_HOST == LoaderHost::None {
            return; // nothing is ever installed on a host that runs APE natively
        }
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let first = install(std::slice::from_ref(&cache), Some("bin-x"), "ape", b"bytes")
            .expect("installed");
        assert_eq!(first, cache.join("bin-x").join("ape"));
        assert_eq!(
            install(std::slice::from_ref(&cache), Some("bin-x"), "ape", b"bytes"),
            Some(first.clone())
        );
        // A file where the parent should be is never a usable directory.
        let file_parent = root.path().join("file");
        std::fs::write(&file_parent, b"").unwrap();
        assert_eq!(
            install(&[file_parent.join("cache")], None, "ape", b"bytes"),
            None
        );
    }

    /// Hostile/corrupt images never panic and never yield a loader.
    #[test]
    fn corrupt_images_yield_no_embedded_loader() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::read(hello_fixture()).unwrap();

        let truncated = dir.path().join("truncated");
        std::fs::write(&truncated, &real[..20_000]).unwrap();
        assert_eq!(extract_loader(&truncated, "x86_64"), None);

        let mut garbage = real.clone();
        garbage[275_392..275_392 + 4180].fill(0xA5);
        let garbled = dir.path().join("garbled");
        std::fs::write(&garbled, &garbage).unwrap();
        assert_eq!(extract_loader(&garbled, "x86_64"), None);

        let huge = dir.path().join("huge-range");
        std::fs::write(
            &huge,
            "MZqFpD='\n'\nif [ \"$m\" = x86_64 ]; then\ndd if=\"$o\" skip=18446744073709551615 count=99 bs=1 2>/dev/null | gzip -dc >\"$t.$$\" ||exit\nfi\n",
        )
        .unwrap();
        assert_eq!(extract_loader(&huge, "x86_64"), None);

        // A valid gzip of a non-ELF payload is rejected too.
        let member = gzip(b"#!/bin/sh\necho pwned\n");
        let mut fake = format!(
            "MZqFpD='\n'\nif [ \"$m\" = x86_64 ]; then\ndd if=\"$o\" skip=200 count={} bs=1 2>/dev/null | gzip -dc >\"$t.$$\" ||exit\nfi\n",
            member.len()
        )
        .into_bytes();
        fake.resize(200, b'\n');
        fake.extend_from_slice(&member);
        let not_elf = dir.path().join("not-elf");
        std::fs::write(&not_elf, &fake).unwrap();
        assert_eq!(extract_loader(&not_elf, "x86_64"), None);
    }
}
