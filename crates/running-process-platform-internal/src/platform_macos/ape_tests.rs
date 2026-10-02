use std::io::Write;
use std::path::{Path, PathBuf};

use crate::platform::ape;

/// A shell-prologue image like Cosmopolitan's: the kernel refuses it, a
/// shell runs it. With `loader`, the prologue also embeds that file as
/// its Linux loader, in the same `dd ... | gzip -dc` form.
fn image(dir: &Path, loader: Option<&[u8]>) -> PathBuf {
    #[cfg(feature = "ape-loader")]
    let member = loader.map(gzip).unwrap_or_default();
    #[cfg(not(feature = "ape-loader"))]
    let member: Vec<u8> = loader.map(<[u8]>::to_vec).unwrap_or_default();
    let script = |offset: usize| {
        format!(
            "MZqFpD='\n'\nt=\"${{TMPDIR:-${{HOME:-.}}}}/.ape-test\"\n\
             if [ ! -d /Applications ]; then\nif [ \"$m\" = {machine} ]; then\n\
             dd if=\"$o\" skip={offset:<10} count={len:<10} bs=1 2>/dev/null | gzip -dc\n\
             fi\nfi\nprintf 'ape-ok:%s|' \"$@\"\nexit 0\n",
            machine = std::env::consts::ARCH,
            len = member.len(),
        )
    };
    let offset = script(0).len();
    let path = dir.join("tool.com");
    let mut file = std::fs::File::create(&path).expect("create image");
    file.write_all(script(offset).as_bytes()).expect("write prologue");
    file.write_all(&member).expect("write loader");
    drop(file);
    super::mark_executable(&path).expect("mark image executable");
    path
}

#[cfg(feature = "ape-loader")]
fn gzip(payload: &[u8]) -> Vec<u8> {
    let mut member = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
    member.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(payload, 1));
    member.extend_from_slice(&0u32.to_le_bytes());
    member.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    member
}

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("scratch directory")
}

#[test]
fn the_kernel_refuses_an_ape_image_without_help() {
    let dir = scratch();
    let error = std::process::Command::new(image(dir.path(), None))
        .output()
        .expect_err("no binfmt registration for the fixture");
    assert!(super::is_exec_format_error(&error), "{error:?}");
}

#[test]
fn a_caller_built_command_is_retried_with_its_settings_intact() {
    if !super::APE_EXECVP_SHELL_FALLBACK {
        return;
    }
    let dir = scratch();
    let image = image(dir.path(), None);
    let mut command = std::process::Command::new("./tool.com");
    command
        .args(["one", "two words"])
        .current_dir(dir.path())
        .stdout(std::process::Stdio::piped());
    let output = ape::spawn_std(&mut command, |command| command.spawn())
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
    let error = ape::spawn_std(&mut command, |command| command.spawn())
        .expect_err("not an APE image");
    assert!(super::is_exec_format_error(&error), "{error:?}");
}

#[cfg(feature = "async-process")]
#[tokio::test]
async fn a_spec_with_a_cleared_environment_runs_through_the_shell() {
    let dir = scratch();
    let image = image(dir.path(), None);
    let output = crate::SpawnSpec::new(&image)
        .arg("x")
        .clear_env(true)
        .stdout(crate::StreamMode::Piped)
        .spawn()
        .await
        .expect("APE image launched")
        .wait_with_output()
        .await
        .expect("reap");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"ape-ok:x|");
}
