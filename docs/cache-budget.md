# GitHub Actions cache budget

The nightly `ci.yml` run invokes cache-budget maintenance after the full Linux,
macOS, Windows, coverage, and all-features cache-writing jobs have settled.
It keeps repository Actions-cache usage below 9.5 billion bytes, leaving margin
under GitHub's 10 GiB eviction threshold. It removes only explicitly retired
cache-key families on `refs/heads/main`; PR refs and unrelated target/profile
caches are never selected. Ordinary PRs and main pushes do not run cleanup.

The optional all-features matrix remains enabled, but its Windows/macOS Rust
target caches are restore-only. This retains platform coverage without growing
three multi-hundred-megabyte copies on each run.

The six platform release-build caches and six cross-target release-binaries
caches are also restore-only and retired from the main ref. The measured
12-entry family occupied 4,078,226,358 bytes. Release packaging, target builds,
and validation still run unchanged; the tradeoff is that a future release can
start those Rust target builds cold instead of restoring the old multi-gigabyte
archives.

Windows ARM preflight and coverage retain Cargo registry downloads but no
workspace target artifacts. Their prior main-ref target archives occupied
2,105,526,302 and 944,115,359 bytes, respectively, in the observed full
no-eviction cycle. The replacement registry-only keys use new namespaces, so
the budget workflow retires only the superseded target-bearing keys and keeps
the smaller replacements. Test coverage remains unchanged; Windows ARM and
coverage builds may take longer when their target artifacts are cold. The
measured no-eviction steady-state projection after these retirements is about
8.44 billion bytes, leaving roughly 1.06 billion bytes below the 9.5-billion
guard. Future Cargo.lock key churn can temporarily add replacement entries;
the main writer barrier and post-writer cleanup keep obsolete disabled-family
entries out of the settled budget, but that transient headroom should be
rechecked if future full cycles grow materially.

Other development/preflight, musl, security, and Dylint caches remain eligible
for main saves.
