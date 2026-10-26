#!/usr/bin/env bash
# Install a Rust target's standard library, and reinstall it when the
# toolchain claims it but its files are missing.
#
# Usage: ci/install_rust_target.sh <target-triple> [toolchain]
#
# setup-soldr restores the toolchain from a solo-toolchain cache whose key does
# not include the added targets. A payload saved from a job that had added a
# target can come back with rustup's component manifest listing
# `rust-std-<target>` but without the library files. `rustup target add` then
# reports "up to date" and changes nothing, and soldr refuses the build
# ("lists rust-std ... but the library files are missing"). Removing the
# component first makes the add a real install. `rustup component remove`
# itself rolls back when a file listed in the component's manifest is gone,
# so if it fails the component is dropped from the toolchain's installed list
# (`components`) and its file list (`manifest-*`), and the removal retried:
# rustup then only unregisters it from the channel config.
set -euo pipefail

target="${1:?usage: install_rust_target.sh <target-triple> [toolchain]}"
toolchain="${2:-1.95.0}"

soldr rustup target add "$target" --toolchain "$toolchain"

rustc="$(soldr rustup which rustc --toolchain "$toolchain")"
rustlib="$(dirname "$(dirname "$rustc")")/lib/rustlib"
libdir="$rustlib/$target/lib"
if compgen -G "$libdir/libstd-*.rlib" > /dev/null; then
    exit 0
fi

echo "rust-std for $target is registered but $libdir has no libstd; reinstalling" >&2
if ! soldr rustup component remove "rust-std-$target" --toolchain "$toolchain"; then
    sed -i "/^rust-std-$target\$/d" "$rustlib/components"
    rm -f "$rustlib/manifest-rust-std-$target"
    soldr rustup component remove "rust-std-$target" --toolchain "$toolchain"
fi
soldr rustup target add "$target" --toolchain "$toolchain"
if ! compgen -G "$libdir/libstd-*.rlib" > /dev/null; then
    echo "rust-std for $target is still missing from $libdir after reinstall" >&2
    exit 1
fi
