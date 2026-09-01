//! macOS bounded terminal-graphics probe, independent of PTY allocation.

pub fn active_graphics_probe(
    timeout: std::time::Duration,
) -> crate::platform::terminal_graphics::TerminalGraphicsProbe {
    use std::fs::OpenOptions;
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::time::Instant;

    let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return crate::platform::terminal_graphics::TerminalGraphicsProbe::default();
    };
    let fd = tty.as_raw_fd();
    let mut old_termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    let have_termios = unsafe { libc::tcgetattr(fd, old_termios.as_mut_ptr()) == 0 };
    let old_termios = have_termios.then(|| unsafe { old_termios.assume_init() });
    if let Some(mut raw) = old_termios {
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    }
    let old_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if old_flags >= 0 {
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags | libc::O_NONBLOCK) };
    }
    let _ = tty.write_all(
        b"\x1b[c\x1b[?2;1;0S\x1b_Gi=running-process-probe,a=q;\x1b\\\x1b]1337;Capabilities\x07",
    );
    let _ = tty.flush();
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    while Instant::now() < deadline {
        let mut chunk = [0_u8; 512];
        match tty.read(&mut chunk) {
            Ok(0) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    if old_flags >= 0 {
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags) };
    }
    if let Some(old) = old_termios {
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &old) };
    }
    let reply = String::from_utf8_lossy(&bytes).into_owned();
    crate::platform::terminal_graphics::TerminalGraphicsProbe {
        sixel_xtsmgraphics: reply.contains('S').then(|| reply.clone()),
        sixel_da1: reply.contains("[?").then(|| reply.clone()),
        kitty_graphics: reply.contains("_G").then(|| reply.clone()),
        iterm2_capabilities: reply.contains("Capabilities=").then_some(reply),
    }
}
