
    //! Linux `/proc/<pid>/cmdline` implementation. The kernel writes argv as
    //! NUL-separated opaque bytes, with a trailing NUL.

    use std::os::unix::ffi::OsStringExt;

    /// Return the process argv without flattening its argument boundaries.
    pub fn read_process_argv(pid: u32) -> std::io::Result<Vec<std::ffi::OsString>> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler Ã¢â‚¬â€ not queryable",
            ));
        }
        let path = format!("/proc/{pid}/cmdline");
        let bytes = std::fs::read(&path)?;
        // `/proc/<pid>/cmdline` is empty for kernel threads.
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let bytes = bytes.strip_suffix(&[0]).unwrap_or(&bytes);
        Ok(bytes
            .split(|byte| *byte == 0)
            .map(|argument| std::ffi::OsString::from_vec(argument.to_vec()))
            .collect())
    }

    /// Return a stable human-readable rendering of [`read_process_argv`].
    ///
    /// This intentionally is not shell syntax and must not be parsed back
    /// into argv. It preserves the historical `read_process_cmdline` output.
    pub fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
        Ok(render_display(&read_process_argv(pid)?))
    }

    fn render_display(argv: &[std::ffi::OsString]) -> String {
        argv.iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[cfg(test)]
    mod tests {
        use super::render_display;
        use std::os::unix::ffi::OsStringExt;

        #[test]
        fn display_is_separate_from_structured_argv() {
            let argv = vec![
                std::ffi::OsString::from("tool"),
                std::ffi::OsString::from("has space"),
                std::ffi::OsString::from("quote\""),
                std::ffi::OsString::new(),
                std::ffi::OsString::from_vec(b"back\\slash".to_vec()),
            ];
            assert_eq!(render_display(&argv), "tool has space quote\"  back\\slash");
            assert_eq!(argv[3], std::ffi::OsString::new());
        }
    }
