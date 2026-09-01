//! Session fixture that consumes measurable direct-child CPU time.

use std::hint::black_box;

fn main() {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut value = 0_u64;
    while std::time::Instant::now() < until {
        value = value.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        black_box(value);
    }
}
