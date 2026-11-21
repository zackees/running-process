
    //! Windows `NtQueryInformationProcess(ProcessCommandLineInformation)`
    //! implementation. The Info class is undocumented but stable on
    //! Win8.1+ Ã¢â‚¬â€ empirically validated in clud#468 t03.

    use std::ffi::c_void;

    /// `ProcessCommandLineInformation` from `ntddk.h` Ã¢â‚¬â€ info class 60.
    /// Stable since Windows 8.1. Returns a `UNICODE_STRING` header
    /// followed by the inline wide-character cmdline bytes.
    const PROCESS_COMMAND_LINE_INFORMATION: i32 = 60;

    /// `STATUS_INFO_LENGTH_MISMATCH` (0xC0000004) Ã¢â‚¬â€ expected on the
    /// initial size-probe call.
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004u32 as i32;

    /// `STATUS_SUCCESS` (0).
    const STATUS_SUCCESS: i32 = 0;

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryInformationProcess(
            process_handle: *mut c_void,
            process_information_class: i32,
            process_information: *mut c_void,
            process_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    /// Return the legacy native command-line display string.
    ///
    /// This is not structured argv; use [`read_process_argv`] to preserve
    /// argument boundaries.
    pub fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
        Ok(String::from_utf16_lossy(&read_process_cmdline_wide(pid)?))
    }

    /// Return the process argv parsed with Windows' native command-line rules.
    pub fn read_process_argv(pid: u32) -> std::io::Result<Vec<std::ffi::OsString>> {
        parse_command_line(&read_process_cmdline_wide(pid)?)
    }

    fn read_process_cmdline_wide(pid: u32) -> std::io::Result<Vec<u16>> {
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the system idle process Ã¢â‚¬â€ not queryable",
            ));
        }

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let result = query_cmdline_wide(handle as *mut c_void);
        unsafe { CloseHandle(handle) };
        result
    }

    fn query_cmdline_wide(handle: *mut c_void) -> std::io::Result<Vec<u16>> {
        // Size probe: pass a zero-length buffer; expect
        // STATUS_INFO_LENGTH_MISMATCH and the required size in
        // `needed`.
        let mut needed: u32 = 0;
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if status != STATUS_INFO_LENGTH_MISMATCH && status != STATUS_SUCCESS {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess size probe returned status=0x{:08x}",
                status as u32,
            )));
        }
        if needed < std::mem::size_of::<UnicodeString>() as u32 {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess returned needed={needed}, smaller than UNICODE_STRING header",
            )));
        }

        let mut buf = vec![0u8; needed as usize];
        let mut returned: u32 = 0;
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                buf.as_mut_ptr() as *mut c_void,
                needed,
                &mut returned,
            )
        };
        if status != STATUS_SUCCESS {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess returned status=0x{:08x}",
                status as u32,
            )));
        }

        // The buffer begins with a UNICODE_STRING whose `buffer` field
        // points into the same allocation, immediately past the header.
        // We cannot dereference `us.buffer` directly across the FFI
        // boundary on systems that may relocate it; instead, compute the
        // header size and read inline.
        // `buf` has byte alignment, so the FFI header must be read
        // unaligned. Do not cast its payload to `&[u16]` for the same reason.
        let us = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const UnicodeString) };
        let len_bytes = us.length as usize;
        if len_bytes == 0 {
            return Ok(Vec::new());
        }
        if !len_bytes.is_multiple_of(std::mem::size_of::<u16>()) {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess returned odd UTF-16 byte length {len_bytes}",
            )));
        }
        // The string is wide-char (UTF-16 LE) and located just after the
        // UNICODE_STRING header. The kernel writes `buffer` as a pointer
        // into our supplied allocation, but the safest portable parse is
        // to read the chars from header_size..header_size+len_bytes in
        // our own buffer.
        let header_size = std::mem::size_of::<UnicodeString>();
        if header_size + len_bytes > buf.len() {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess wrote less than {} bytes for cmdline (returned={returned}, len={len_bytes})",
                header_size + len_bytes,
            )));
        }
        Ok(buf[header_size..header_size + len_bytes]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect())
    }

    fn parse_command_line(command_line: &[u16]) -> std::io::Result<Vec<std::ffi::OsString>> {
        use std::os::windows::ffi::OsStringExt;
        use winapi::shared::minwindef::HLOCAL;
        use winapi::um::shellapi::CommandLineToArgvW;
        use winapi::um::winbase::LocalFree;

        let mut nul_terminated = command_line.to_vec();
        nul_terminated.push(0);
        let mut argc = 0;
        let argv = unsafe { CommandLineToArgvW(nul_terminated.as_ptr(), &mut argc) };
        if argv.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let result = unsafe {
            std::slice::from_raw_parts(argv, argc as usize)
                .iter()
                .map(|argument| {
                    let length = (0..).take_while(|index| *(*argument).add(*index) != 0).count();
                    std::ffi::OsString::from_wide(std::slice::from_raw_parts(*argument, length))
                })
                .collect()
        };
        unsafe { LocalFree(argv as HLOCAL) };
        Ok(result)
    }

    #[cfg(test)]
    mod tests {
        use super::parse_command_line;

        #[test]
        fn native_parser_keeps_spaces_quotes_empty_args_and_backslashes() {
            // This is the same `CommandLineToArgvW` grammar used by
            // CreateProcessW consumers, including the doubled trailing
            // backslashes before a closing quote.
            let command_line: Vec<u16> =
                r#"tool "has space" "quote\"" "" "back\slash\\""#
                    .encode_utf16()
                    .collect();
            assert_eq!(
                parse_command_line(&command_line).expect("parse"),
                ["tool", "has space", "quote\"", "", r"back\slash\"]
                    .map(std::ffi::OsString::from)
            );
        }
    }
