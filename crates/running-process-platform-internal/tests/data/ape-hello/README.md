# ape-hello — APE test fixture

A minimal [Actually Portable Executable](https://justine.lol/ape.html)
(cosmocc) used by `crate::platform::ape` tests to prove fbuild can spawn APE
binaries on hosts with no APE `binfmt_misc` handler (e.g. NixOS), where a direct
`posix_spawn` fails with `ENOEXEC` ("Exec format error").

| File       | Purpose |
| ---------- | ------- |
| `hello.c`  | Source: prints `hello world` followed by its arguments. |
| `hello.com`| Fat x86_64 + aarch64 APE built from `hello.c` (`-Os -mtiny -s`, ~295 KB). Test-only. |
| `build.sh` | Rebuilds `hello.com`; set `COSMOCC` to the `cosmocc` driver. |

```console
$ ./hello.com a b        # works from bash (it retries ENOEXEC via sh)
hello world a b
```
