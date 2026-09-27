fn main() {
    #[cfg(all(windows, not(target_os = "linux")))]
    use std::os::windows::process::CommandExt as _;
}
