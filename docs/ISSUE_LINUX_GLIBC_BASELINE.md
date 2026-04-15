# Issue Draft: Pin Non-Musl Linux Wheels To The Oldest Supported glibc Floor

## Summary

Make the non-musl Linux wheel baseline explicit and test-covered.

## Problem

The repo's release wheel path already uses `manylinux2014`, but that baseline is
implicit in `ci/build_wheel.py`. There is no single test or issue that states:

- which non-musl glibc floor we support
- why it is not lower
- where to change it safely later

That ambiguity makes it easy to regress the wheel target or to ask for an
unsupported lower baseline like `manylinux2010`.

## Decision

For the non-musl Linux wheel path, pin the release build to:

- `manylinux2014`
- glibc `2.17`

This is the oldest supported floor for the current Rust/maturin toolchain path.
`maturin build --help` in this repo's environment explicitly reports that
`manylinux1` and `manylinux2010` are unsupported for Rust wheels.

## Tasks

- [ ] Add a named constant for the Linux non-musl wheel compatibility target
- [ ] Add a helper that returns the Linux release compatibility args
- [ ] Add tests that lock the Linux release path to `manylinux2014`
- [ ] Document the glibc 2.17 floor in the README
- [ ] If we ever need an older floor, investigate a different build/distribution
      path instead of changing the current Rust/maturin release wheel path blindly

## Validation

- `uv run pytest tests/test_ci_build_wheel.py -q`
