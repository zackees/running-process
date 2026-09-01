//! Session fixture that keeps an inherited stdout pipe open and emits slowly.

use std::io::Write;

fn main() {
    let mut stdout = std::io::stdout();
    loop {
        stdout.write_all(b".").expect("write drip byte");
        stdout.flush().expect("flush drip byte");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
