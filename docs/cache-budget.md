# GitHub Actions cache budget

The main-only cache-budget workflow keeps repository Actions-cache usage below
9.5 billion bytes, leaving margin under GitHub's 10 GiB eviction threshold.
It removes only explicitly retired cache-key families on `refs/heads/main`;
PR refs and unrelated target/profile caches are never selected.

The optional all-features matrix remains enabled, but its Windows/macOS Rust
target caches are restore-only. This retains platform coverage without growing
three multi-hundred-megabyte copies on each run.

The six platform release-build caches and six cross-target release-binaries
caches are also restore-only and retired from the main ref. The measured
12-entry family occupied 4,078,226,358 bytes. Release packaging, target builds,
and validation still run unchanged; the tradeoff is that a future release can
start those Rust target builds cold instead of restoring the old multi-gigabyte
archives. Shared development/preflight, coverage, musl, security, and Dylint
caches remain eligible for main saves.
