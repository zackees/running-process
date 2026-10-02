#!/usr/bin/env bash
# Rebuild hello.com with cosmocc (https://cosmo.zip/pub/cosmocc/cosmocc.zip).
# Usage: COSMOCC=/path/to/cosmocc/bin/cosmocc ./build.sh
set -euo pipefail
cd "$(dirname "$0")"
"${COSMOCC:-cosmocc}" -Os -mtiny -s -o hello.com hello.c
# cosmocc also leaves per-arch debug images next to the output.
rm -f hello.com.dbg hello.aarch64.elf
