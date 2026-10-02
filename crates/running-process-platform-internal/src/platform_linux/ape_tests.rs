#[cfg(feature = "ape-loader")]
use std::ffi::OsString;
use std::io::Write;
#[cfg(feature = "ape-loader")]
use std::io::Read;
#[cfg(feature = "ape-loader")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::platform::ape;

/// The checked-in cosmocc hello-world: prints `hello world` and its args.
fn hello() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ape-hello/hello.com")
}

/// A shell-prologue image like Cosmopolitan's, with no embedded loader: the
/// kernel refuses it and a shell runs it.
fn shell_image(dir: &Path) -> PathBuf {
    let path = dir.join("tool.com");
    let mut file = File::create(&path).expect("create image");
    file.write_all(b"MZqFpD='\n'\nprintf 'ape-ok:%s|' \"$@\"\nexit 0\n")
        .expect("write prologue");
    drop(file);
    super::mark_executable(&path).expect("mark image executable");
    path
}

#[cfg(all(feature = "async-process", feature = "ape-loader"))]
fn copy_hello(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::copy(hello(), &path).unwrap();
    path
}

#[cfg(feature = "ape-loader")]
fn host_loader() -> Vec<u8> {
    ape::extract_loader(&hello(), std::env::consts::ARCH).expect("fixture embeds host loader")
}

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("scratch directory")
}

/// Spawn `spec` with its output captured, retrying while a sibling test's
/// fork still holds a freshly written file open (`ETXTBSY`).
#[cfg(feature = "async-process")]
fn output(spec: crate::SpawnSpec) -> std::process::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let spec = spec
            .stdout(crate::StreamMode::Piped)
            .stderr(crate::StreamMode::Piped);
        for _ in 0..50 {
            match spec.clone().spawn().await {
                Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                result => {
                    return result
                        .expect("spawn")
                        .wait_with_output()
                        .await
                        .expect("reap")
                }
            }
        }
        panic!("program stayed busy");
    })
}

/// A child environment with no `PATH`, `TMPDIR` or `HOME`, so neither `ape`,
/// shell utilities, nor the prologue's own cache can help: only the loader
/// this crate extracts can run the image.
#[cfg(all(feature = "async-process", feature = "ape-loader"))]
fn hostile(program: impl Into<OsString>, cache: &Path, args: &[&str]) -> std::process::Output {
    let mut spec = crate::SpawnSpec::new(program)
        .clear_env(true)
        .env("PATH", "/nonexistent")
        .env("TMPDIR", "/nonexistent")
        .env("HOME", "/nonexistent")
        .env(ape::CACHE_DIR_ENV, cache);
    for arg in args {
        spec = spec.arg(*arg);
    }
    output(spec)
}

#[cfg(any(feature = "async-process", feature = "ape-loader"))]
fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_kernel_refuses_an_ape_image_without_help() {
    let dir = scratch();
    let image = shell_image(dir.path());
    let error = ape::retry_while_busy(|| std::process::Command::new(&image).output())
        .expect_err("no binfmt registration for the fixture");
    assert!(super::is_exec_format_error(&error), "{error:?}");
}

#[test]
fn a_caller_built_command_is_retried_with_its_settings_intact() {
    if !super::APE_EXECVP_SHELL_FALLBACK {
        return;
    }
    let dir = scratch();
    let image = shell_image(dir.path());
    let mut command = std::process::Command::new("./tool.com");
    command
        .args(["one", "two words"])
        .current_dir(dir.path())
        .stdout(std::process::Stdio::piped());
    let output = ape::retry_while_busy(|| ape::spawn_std(&mut command, |command| command.spawn()))
        .expect("APE image retried")
        .wait_with_output()
        .expect("reap");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"ape-ok:one|ape-ok:two words|");
    assert!(image.exists());
}

#[test]
fn a_refused_native_image_keeps_its_error() {
    let dir = scratch();
    let garbage = dir.path().join("garbage");
    std::fs::write(&garbage, b"\x00\x01\x02\x03 not an image").unwrap();
    super::mark_executable(&garbage).unwrap();
    let mut command = std::process::Command::new(&garbage);
    let error = ape::retry_while_busy(|| ape::spawn_std(&mut command, |command| command.spawn()))
        .expect_err("not an APE image");
    assert!(super::is_exec_format_error(&error), "{error:?}");
}

#[test]
fn command_routes_a_real_image_through_its_loader() {
    let mut command = ape::command(hello());
    command.arg("std").stdout(std::process::Stdio::piped());
    let output = ape::retry_while_busy(|| ape::spawn_std(&mut command, |command| command.spawn()))
        .expect("real APE must spawn")
        .wait_with_output()
        .expect("reap");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"hello world std\n");
}

#[cfg(feature = "async-process")]
#[test]
fn a_spec_with_a_cleared_environment_runs_a_shell_image() {
    let dir = scratch();
    let output = output(crate::SpawnSpec::new(shell_image(dir.path())).arg("x").clear_env(true));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"ape-ok:x|");
}

#[cfg(feature = "async-process")]
#[test]
fn a_real_image_runs_through_the_default_loader_chain() {
    let output = output(crate::SpawnSpec::new(hello()).arg("from").arg("spec"));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output), "hello world from spec\n");
}

#[cfg(all(feature = "async-process", feature = "ape-loader"))]
#[test]
fn a_real_image_runs_with_a_hostile_child_environment_using_its_own_loader() {
    let cache = scratch();
    let output = hostile(hello(), cache.path(), &["hostile"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output), "hello world hostile\n");
    let mut installed: Vec<_> = std::fs::read_dir(cache.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    installed.sort();
    // The loader itself, and the private directory exposing it as `ape`.
    assert_eq!(installed.len(), 2, "{installed:?}");
    assert!(installed[0].starts_with("ape-loader-"), "{installed:?}");
    assert!(installed[1].starts_with("bin-"), "{installed:?}");
    assert!(cache.path().join(&installed[1]).join("ape").is_file());
}

#[cfg(all(feature = "async-process", feature = "ape-loader"))]
#[test]
fn a_broken_ape_on_path_does_not_hijack_the_launch() {
    let bin = scratch();
    let fake = bin.path().join("ape");
    std::fs::write(&fake, "#!/bin/sh\necho hijacked; exit 99\n").unwrap();
    super::mark_executable(&fake).unwrap();
    let image = copy_hello(bin.path(), "hello");
    let cache = scratch();
    let output = output(
        crate::SpawnSpec::new("hello")
            .arg("ok")
            .env("PATH", bin.path())
            .env(ape::CACHE_DIR_ENV, cache.path()),
    );
    assert_eq!(stdout(&output), "hello world ok\n", "{output:?}");
    assert!(image.exists());
}

#[cfg(all(feature = "async-process", feature = "ape-loader"))]
#[test]
fn awkward_image_paths_and_arguments_survive() {
    let dir = scratch();
    let sub = dir.path().join("dir with spaces – ünïcode");
    std::fs::create_dir(&sub).unwrap();
    let image = copy_hello(&sub, "hello world.com");
    let link = dir.path().join("link-to-hello");
    std::os::unix::fs::symlink(&image, &link).unwrap();
    let cache = scratch();
    for program in [&image, &link] {
        let output = hostile(
            program,
            cache.path(),
            &["a b", "", "$HOME", "--assimilate"],
        );
        assert!(output.status.success(), "{output:?}");
        assert_eq!(stdout(&output), "hello world a b  $HOME --assimilate\n");
    }
    // `--assimilate` is just an argument: the image must be untouched.
    assert_eq!(
        std::fs::read(&image).unwrap(),
        std::fs::read(hello()).unwrap()
    );
}

#[cfg(all(feature = "async-process", feature = "ape-loader"))]
#[test]
fn a_rebuilt_image_at_the_same_path_is_reconsidered() {
    let dir = scratch();
    let image = copy_hello(dir.path(), "tool");
    let cache = scratch();
    assert_eq!(
        stdout(&hostile(&image, cache.path(), &["1"])),
        "hello world 1\n"
    );
    // Replace with a native script: it must now run as itself.
    std::fs::write(&image, "#!/bin/sh\necho native \"$@\"\n").unwrap();
    super::mark_executable(&image).unwrap();
    assert_eq!(
        stdout(&output(crate::SpawnSpec::new(&image).arg("2"))),
        "native 2\n"
    );
    // A cache emptied between spawns is re-materialized.
    std::fs::copy(hello(), &image).unwrap();
    for entry in std::fs::read_dir(cache.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(path).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }
    assert_eq!(
        stdout(&hostile(&image, cache.path(), &["3"])),
        "hello world 3\n"
    );
}

#[cfg(all(feature = "async-process", feature = "ape-loader"))]
#[test]
fn many_concurrent_first_spawns_share_one_installed_loader() {
    let cache = scratch();
    let cache_dir = cache.path().join("cold");
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..48)
            .map(|index| {
                let cache_dir = &cache_dir;
                scope.spawn(move || {
                    let arg = index.to_string();
                    let output = hostile(hello(), cache_dir, &[&arg]);
                    assert!(output.status.success(), "spawn {index}: {output:?}");
                    assert_eq!(stdout(&output), format!("hello world {index}\n"));
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
}

#[cfg(feature = "ape-loader")]
fn run_loader(loader: &Path, args: &[&str]) -> std::process::Output {
    let mut command = std::process::Command::new(loader);
    command.arg(hello()).args(args).env_clear();
    ape::retry_while_busy(|| {
        let _fork = ape::fork_guard();
        command.output()
    })
    .expect("loader must spawn")
}

#[cfg(feature = "ape-loader")]
#[test]
fn a_memfd_loader_runs_a_real_image_and_is_sealed() {
    let loader = memfd_loader(&host_loader(), "ape-loader-test-memfd").expect("memfd");
    assert!(loader.starts_with("/proc/self/fd"));
    let output = run_loader(&loader, &["memfd"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output), "hello world memfd\n");
    // Sealed: the running loader cannot be rewritten underneath children.
    assert!(std::fs::OpenOptions::new()
        .write(true)
        .open(&loader)
        .and_then(|mut file| file.write_all(b"x"))
        .is_err());
}

#[cfg(feature = "ape-loader")]
#[test]
fn unusable_directories_fall_back_to_a_memfd() {
    let root = scratch();
    let readonly = root.path().join("ro");
    std::fs::create_dir(&readonly).unwrap();
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();
    let shared = root.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let dirs = [
        readonly.join("cache"),
        shared.clone(),
        PathBuf::from("/proc/running-process-nope"),
    ];
    let bytes = host_loader();
    let loader = ape::install(&dirs, None, "ape-loader-test-fallback", &bytes)
        .or_else(|| anonymous_executable(&bytes, "ape-loader-test-fallback"))
        .unwrap();
    assert!(loader.starts_with("/proc/self/fd"), "got {}", loader.display());
    assert!(
        std::fs::read_dir(&shared).unwrap().next().is_none(),
        "nothing planted in a shared directory"
    );
    let output = run_loader(&loader, &["fallback"]);
    assert_eq!(stdout(&output), "hello world fallback\n", "{output:?}");
}

#[cfg(feature = "ape-loader")]
#[test]
fn a_symlinked_cache_directory_is_rejected() {
    let root = scratch();
    std::fs::create_dir(root.path().join("real")).unwrap();
    std::os::unix::fs::symlink(root.path().join("real"), root.path().join("link")).unwrap();
    assert!(ape::install_in(&root.path().join("link"), &host_loader(), "l").is_none());
}

#[cfg(feature = "ape-loader")]
#[test]
fn a_tampered_or_truncated_install_is_replaced() {
    let dir = scratch();
    let bytes = host_loader();
    let path = ape::install_in(dir.path(), &bytes, "ape-loader-t").expect("install");
    for bad in [&b"#!/bin/sh\nexit 66\n"[..], &bytes[..bytes.len() / 2]] {
        std::fs::write(&path, bad).unwrap();
        assert_eq!(ape::install_in(dir.path(), &bytes, "ape-loader-t"), Some(path.clone()));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let output = run_loader(&path, &["repaired"]);
        assert_eq!(stdout(&output), "hello world repaired\n", "{output:?}");
    }
    // A lost exec bit is repaired too.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    ape::install_in(dir.path(), &bytes, "ape-loader-t").unwrap();
    assert_ne!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o100,
        0
    );
}

#[cfg(feature = "ape-loader")]
#[test]
fn concurrent_installs_converge_without_partial_files() {
    let dir = scratch();
    let cache = dir.path().join("fresh/ape");
    let bytes = host_loader();
    let paths: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..32)
            .map(|_| scope.spawn(|| ape::install_in(&cache, &bytes, "ape-loader-race")))
            .collect();
        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    });
    assert!(paths
        .iter()
        .all(|path| path.as_deref() == Some(cache.join("ape-loader-race").as_path())));
    let entries: Vec<_> = std::fs::read_dir(&cache)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        entries,
        vec![OsString::from("ape-loader-race")],
        "no stray staging files"
    );
    assert_eq!(
        std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let mut installed = Vec::new();
    File::open(cache.join("ape-loader-race"))
        .unwrap()
        .read_to_end(&mut installed)
        .unwrap();
    assert_eq!(installed, bytes);
}

/// The planned launch exposes a private directory holding the real loader as
/// `ape`, and puts it first on the child's `PATH`.
#[cfg(feature = "ape-loader")]
#[test]
fn a_launch_exposes_its_loader_as_ape_for_nested_spawns() {
    let cache = scratch();
    let options = ape::ApeOptions {
        cache_dirs: vec![cache.path().to_path_buf()],
        ..ape::ApeOptions::default()
    };
    let launch = ape::plan_launch(hello().as_os_str(), None, &options).expect("planned");
    assert_eq!(launch.kind, ape::LoaderKind::Embedded);
    let dir = launch.ape_path_dir.clone().expect("an ape PATH directory");
    assert!(dir.starts_with(cache.path()), "{}", dir.display());
    assert_eq!(std::fs::read(dir.join("ape")).unwrap(), host_loader());
    assert_eq!(
        launch.child_path(Some(std::ffi::OsStr::new("/usr/bin:/bin"))),
        Some(std::env::join_paths([dir.clone(), "/usr/bin".into(), "/bin".into()]).unwrap())
    );
    assert_eq!(launch.child_path(None), Some(dir.clone().into_os_string()));

    // A shell loader is never exposed as `ape`.
    let dir_for_shell = scratch();
    let fake = shell_image(dir_for_shell.path());
    let shell_launch = ape::plan_launch(fake.as_os_str(), None, &ape::ApeOptions::default());
    if let Some(launch) = shell_launch {
        if launch.kind == ape::LoaderKind::Shell {
            assert_eq!(launch.ape_path_dir, None);
        }
    }
}

/// What gcc does with `cc1`: a process this crate did not plan execs an APE
/// image. With only the `ape` directory on `PATH` and no `TMPDIR` or `HOME`,
/// the image's own prologue finds the loader through `type ape` and needs no
/// `mkdir`, `dd` or `gzip`.
#[cfg(feature = "ape-loader")]
#[test]
fn a_nested_spawn_finds_the_loader_through_the_ape_path_directory() {
    let cache = scratch();
    let options = ape::ApeOptions {
        cache_dirs: vec![cache.path().to_path_buf()],
        ..ape::ApeOptions::default()
    };
    let launch = ape::plan_launch(hello().as_os_str(), None, &options).expect("planned");
    let path = launch.child_path(None).expect("ape directory");
    let mut command = std::process::Command::new(super::APE_SHELL);
    command
        .arg("-c")
        .arg("exec \"$0\" nested")
        .arg(hello())
        .env_clear()
        .env("PATH", &path)
        .env("TMPDIR", "/nonexistent")
        .env("HOME", "/nonexistent");
    let output = ape::retry_while_busy(|| {
        let _fork = ape::fork_guard();
        command.output()
    })
    .expect("nested launch");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output), "hello world nested\n");
}
