from __future__ import annotations

import collections

from ci import platform_boundary

# The ledger only shrinks. This is a ceiling, not an exact pin: an exact number
# made every row-removing PR edit this one line, so parallel PRs conflicted and
# main went red when several merged against a stale count (#975). Growth is also
# rejected independently -- a new occurrence fails the source scan and Dylint --
# so the ceiling only has to be lowered when someone wants to lock a gain in.
MAX_LEDGER_ROWS = 196


def test_bootstrap_ledgers_are_valid() -> None:
    rows = platform_boundary.parse_ledger()

    assert rows
    assert len(rows) <= MAX_LEDGER_ROWS
    assert not platform_boundary.validate_ledger(rows)
    assert not platform_boundary.manifest_dependency_violations()
    assert not platform_boundary.neutral_facade_contract_violations()


def test_every_ledger_group_has_contiguous_ordinals() -> None:
    rows = platform_boundary.parse_ledger()
    groups: dict[tuple[str, str, str], list[int]] = {}
    for row in rows:
        groups.setdefault((row.path, row.kind, row.normalized), []).append(row.ordinal)

    assert all(
        sorted(ordinals) == list(range(len(ordinals))) for ordinals in groups.values()
    )


def test_artifact_zones_accept_their_own_artifacts() -> None:
    """Every registered zone is satisfied by the crate it covers.

    A zone that its own artifact violates is a contract written against
    imagined code, so this is the floor before the rejection tests below mean
    anything.
    """
    assert not platform_boundary.artifact_zone_violations()
    assert not platform_boundary.zone_manifest_alignment_violations()


def test_a_zone_is_a_contract_not_a_blanket_exemption() -> None:
    """A zone rejects what its artifact was never meant to reference.

    This is the difference #974 asks for between a named zone and an
    allowlisted path. Each case is unremarkable elsewhere in the tree and wrong
    *here*: another platform's cfg, a host key the artifact never claimed, and
    a native crate outside its contract.

    Checked against text rather than by editing the crate, so the test is safe
    beside a parallel suite and cannot leave the repository dirty if it fails
    part-way.
    """
    zone = platform_boundary.ARTIFACT_ZONES[0]
    offences = {
        "target_os = 'linux'": '#[cfg(target_os = "linux")] fn probe() {}',
        "host key 'target_arch'": '#[cfg(target_arch = "x86_64")] fn probe() {}',
        "native import 'libc'": "fn probe() -> libc::c_int { 0 }",
    }
    for expected, snippet in offences.items():
        failures = platform_boundary.zone_text_violations(zone, "fixture.rs", snippet)
        assert failures, f"zone accepted {expected!r}, which it does not permit"
        assert any(
            expected in failure for failure in failures
        ), f"zone rejected {expected!r} but said something else: {failures}"


def test_a_zone_accepts_what_its_artifact_is_for() -> None:
    """The contract is not vacuous: the artifact's own shape passes.

    Without this, a zone that rejected everything would satisfy the tests
    above and still be wrong.
    """
    zone = platform_boundary.ARTIFACT_ZONES[0]
    permitted = (
        '#[cfg(all(target_os = "windows", target_env = "gnu"))] '
        "fn probe() -> windows_sys::Win32::Foundation::HANDLE { todo!() }"
    )
    assert not platform_boundary.zone_text_violations(zone, "fixture.rs", permitted)


def test_a_zone_covering_nothing_is_a_failure() -> None:
    """An exemption for a crate that no longer exists is stale, not harmless."""
    empty = platform_boundary.ArtifactZone(
        name="gone",
        prefix="crates/this-crate-does-not-exist",
        reason="fixture",
        host_keys=frozenset(),
        host_values=frozenset(),
        native_imports=frozenset(),
    )
    original = platform_boundary.ARTIFACT_ZONES
    try:
        platform_boundary.ARTIFACT_ZONES = (*original, empty)
        failures = platform_boundary.artifact_zone_violations()
        assert any("covers no sources" in failure for failure in failures)
    finally:
        platform_boundary.ARTIFACT_ZONES = original


def test_each_zone_rejects_its_neighbours_mechanics() -> None:
    """One interposer's mechanics are wrong in another's zone.

    This is what a path-based exemption cannot express. The three interposers
    are the same *kind* of artifact and would look identical to a rule that
    only asked "is this file exempt?" -- but a Windows import inside the Linux
    interposer, or `libc` inside the Windows one, is a mistake in exactly the
    way an out-of-zone reference elsewhere in the tree would be.
    """
    zones = {zone.name: zone for zone in platform_boundary.ARTIFACT_ZONES}
    cases = [
        ("interposer-linux", '#[cfg(target_os = "windows")] fn p() {}', "target_os"),
        ("interposer-linux", "fn p() -> windows_sys::X { }", "windows_sys"),
        ("interposer-macos", '#[cfg(target_os = "linux")] fn p() {}', "target_os"),
        ("interposer-windows", "fn p() -> libc::c_int { 0 }", "libc"),
        ("interposer-windows", '#[cfg(target_env = "gnu")] fn p() {}', "target_env"),
    ]
    for zone_name, snippet, expected in cases:
        failures = platform_boundary.zone_text_violations(
            zones[zone_name], "fixture.rs", snippet
        )
        assert any(
            expected in failure for failure in failures
        ), f"{zone_name} accepted {expected!r}, which belongs to another host"


def test_every_zone_is_registered_on_both_sides() -> None:
    """A zone the Dylint lint does not know about deletes rows it still reports.

    The two halves are checked against each other rather than trusted to stay
    in step, because the Dylint lane needs a nightly toolchain and does not run
    in `./lint` -- so a drift shows up only as a red workspace gate on a branch
    whose local gates were green.
    """
    assert not platform_boundary.zone_dylint_alignment_violations()
    assert not platform_boundary.zone_manifest_alignment_violations()


def test_a_zone_resting_on_an_unpublished_crate_checks_that_it_is() -> None:
    """Where a justification rests on a checkable fact, it is checked.

    `test-watchdog` may reach for a debugger because nothing downstream links
    it. That premise lives in a manifest and can change without anyone
    revisiting the zone, so the zone asserts it rather than describing it.
    """
    assert not platform_boundary.zone_premise_violations()

    resting = [
        zone for zone in platform_boundary.ARTIFACT_ZONES if zone.requires_unpublished
    ]
    assert resting, "no zone claims this premise; the check would be vacuous"
    for zone in resting:
        manifest = platform_boundary.ROOT / zone.prefix / "Cargo.toml"
        assert platform_boundary.PUBLISH_FALSE.search(
            manifest.read_text(encoding="utf-8")
        ), f"{zone.name} claims to be unpublished but its manifest does not say so"


def test_a_zone_may_be_narrower_than_a_crate() -> None:
    """A zone covers what is actually constrained, not the crate around it.

    `probe-crash` is the first zone narrower than a crate: the crash handler
    is signal-constrained, while `snapshot/modules.rs` and
    `snapshot/unwind.rs` in the same crate run after every thread has resumed
    and may allocate freely. Exempting the whole crate to reach the handler
    would take the ordinary code with it.
    """
    narrow = [
        zone for zone in platform_boundary.ARTIFACT_ZONES if zone.prefix.count("/") > 1
    ]
    assert narrow, "no narrower-than-crate zone; this check would be vacuous"

    covered = platform_boundary.ZONE_PREFIXES
    still_ledgered = {row.path for row in platform_boundary.parse_ledger()}
    for zone in narrow:
        crate = "/".join(zone.prefix.split("/")[:2])
        siblings = {
            path
            for path in still_ledgered
            if path.startswith(crate) and not path.startswith(covered)
        }
        assert siblings, (
            f"{zone.name} is narrower than its crate but nothing else in that "
            "crate is still in the ledger, so the narrowness buys nothing"
        )


def test_a_host_specific_file_needs_no_host_cfg() -> None:
    """A file the module tree already selects should not select again.

    `snapshot/linux.rs` is reached because `snapshot/mod.rs` chose it, so a
    `target_os` inside it would mean the selection happened twice and one of
    the two is unchecked. Its contract permits `target_arch` -- the register
    set really does differ -- and nothing else.
    """
    zones = {zone.name: zone for zone in platform_boundary.ARTIFACT_ZONES}
    for name in ("probe-capture-linux", "probe-capture-macos", "probe-capture-windows"):
        zone = zones[name]
        assert (
            "target_os" not in zone.host_keys
        ), f"{name} permits target_os, but the module tree already selected it"
        failures = platform_boundary.zone_text_violations(
            zone, "fixture.rs", '#[cfg(target_os = "linux")] fn probe() {}'
        )
        assert failures, f"{name} accepted a redundant host selection"


def test_zone_prefixes_may_name_a_file_or_a_directory() -> None:
    """Both spellings must reach Dylint, or deleted rows come back as failures.

    The alignment check originally looked only for a directory prefix with a
    trailing slash, so the first file-scoped zone reported a false mismatch.
    The trailing slash matters for directories -- without it a prefix would
    also match a sibling whose name merely starts the same way -- so the check
    accepts either spelling rather than dropping it.
    """
    assert not platform_boundary.zone_dylint_alignment_violations()

    files = [z for z in platform_boundary.ARTIFACT_ZONES if z.prefix.endswith(".rs")]
    dirs = [z for z in platform_boundary.ARTIFACT_ZONES if not z.prefix.endswith(".rs")]
    assert files, "no file-scoped zone; the file spelling would be untested"
    assert dirs, "no directory-scoped zone; the slash spelling would be untested"


def test_the_sidecar_zone_does_not_replace_the_sidecar_rule() -> None:
    """Zoning where injection lives must not weaken the rule that it lives only there.

    `probe-sidecar` says the hook-tier negotiation may name all three hosts.
    It says nothing about `running-process` staying free of injection symbols,
    which is the #539 contract that AV/EDR static analysis of consumers finds
    no hooking surface. That is a different claim, enforced by a different
    test, and this asserts that test still exists rather than assuming it.
    """
    guard = (
        platform_boundary.ROOT
        / "crates/running-process/tests/core/probe_facade_surface.rs"
    )
    assert guard.is_file(), (
        "the sidecar zone's reasoning cites this test; without it the zone "
        "would be the only thing said about injection, and it is the wrong "
        "thing to say alone"
    )
    text = guard.read_text(encoding="utf-8")
    assert "CreateRemoteThread" in text, "the guard no longer names what it forbids"


def test_a_zone_may_permit_no_host_selection_at_all() -> None:
    """An empty contract is a real contract, not an unfilled one.

    `probe-inject-windows` is a Windows-only file that needs no cfg: the
    module tree selects it and the API it calls is Windows by construction.
    Permitting nothing is therefore correct, and stricter than its siblings --
    so a cfg appearing there later is a question, not a silent pass.
    """
    zones = {zone.name: zone for zone in platform_boundary.ARTIFACT_ZONES}
    zone = zones["probe-inject-windows"]
    assert not zone.host_keys
    assert not zone.host_values

    failures = platform_boundary.zone_text_violations(
        zone, "fixture.rs", '#[cfg(target_os = "windows")] fn probe() {}'
    )
    assert failures, "a zone permitting no host key must reject every host key"


def test_the_real_tree_holds_the_legacy_interprocess_allowance_exactly() -> None:
    counts = platform_boundary.legacy_ipc_counts()

    assert dict(counts) == platform_boundary.LEGACY_IPC_ALLOWANCE
    assert not platform_boundary.legacy_ipc_violations()


def test_a_new_interprocess_use_outside_the_allowance_is_rejected() -> None:
    path = "crates/running-process/src/broker/brand_new.rs"
    counts = platform_boundary.legacy_ipc_counts(
        {path: "use interprocess::local_socket::Stream;\n"}
    )

    violations = platform_boundary.legacy_ipc_violations(
        counts + collections.Counter(platform_boundary.LEGACY_IPC_ALLOWANCE)
    )

    assert len(violations) == 1
    assert path in violations[0]
    assert "platform::ipc" in violations[0]


def test_growing_an_allowed_file_is_rejected() -> None:
    path = "crates/running-process/src/broker/server/handoff_serve.rs"
    counts = collections.Counter(platform_boundary.LEGACY_IPC_ALLOWANCE)
    counts[path] += 1

    violations = platform_boundary.legacy_ipc_violations(counts)

    assert len(violations) == 1
    assert path in violations[0]


def test_a_stale_allowance_must_be_lowered() -> None:
    path = "crates/running-process/src/broker/server/handoff_serve.rs"
    counts = collections.Counter(platform_boundary.LEGACY_IPC_ALLOWANCE)
    counts[path] -= 1

    violations = platform_boundary.legacy_ipc_violations(counts)

    assert len(violations) == 1
    assert "lower LEGACY_IPC_ALLOWANCE" in violations[0]


def test_comments_and_strings_do_not_count_as_interprocess_uses() -> None:
    counts = platform_boundary.legacy_ipc_counts(
        {
            "crates/running-process/src/x.rs": (
                "// use interprocess::local_socket::Stream;\n"
                '/// see `interprocess::Name`\nconst S: &str = "interprocess::Stream";\n'
            )
        }
    )

    assert not counts


def test_the_real_classifications_are_valid_and_cover_the_argued_rows() -> None:
    rows = platform_boundary.parse_ledger()
    classes, problems = platform_boundary.parse_classes()

    assert not problems
    assert not platform_boundary.classification_violations(rows, classes, problems)
    classified = len(rows) - len(platform_boundary.unclassified_rows(rows, classes))
    # 24 tee-API rows + 1 environment.rs row (compat-4x) + 27 tests.rs rows
    # (host-test) argued in the planning pass, plus every row provably inside
    # test code; the count can only grow as phase 9 proceeds.
    assert classified >= 52
    tee = (
        "crates/running-process/src/daemon/pty_sessions.rs",
        "attr_cfg",
        "unix",
    )
    assert classes[tee][0] == "compat-4x"


def test_a_classification_naming_no_ledger_row_is_stale() -> None:
    rows = platform_boundary.parse_ledger()
    ghost = ("crates/running-process/src/gone.rs", "attr_cfg", "unix")

    violations = platform_boundary.classification_violations(
        rows, {ghost: ("host-test", "why")}, []
    )

    assert len(violations) == 1
    assert "stale classification" in violations[0]


def test_bad_classifications_are_rejected(tmp_path) -> None:
    path = tmp_path / "classes.tsv"
    path.write_text(
        "\n".join(
            [
                "a.rs\tattr_cfg\tunix\tmaybe\twhy",  # unknown class
                "b.rs\tattr_cfg\tunix\thost-test\t ",  # blank note
                "c.rs\tattr_cfg\tunix\tcompat-4x\tkept for now",  # no 5.0 retirement
                "d.rs\tattr_cfg\tunix\thost-test",  # wrong field count
                "e.rs\tattr_cfg\tunix\thost-test\tfine",
                "e.rs\tattr_cfg\tunix\thost-test\tfine again",  # duplicate
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    # parse_classes reports paths relative to the repo; point it at a tmp file.
    original_root = platform_boundary.ROOT
    platform_boundary.ROOT = tmp_path
    try:
        _, problems = platform_boundary.parse_classes(path)
    finally:
        platform_boundary.ROOT = original_root

    text = "\n".join(problems)
    assert "unknown class" in text
    assert "needs a note" in text
    assert "must name its 5.0 retirement" in text
    assert "five tab-separated fields" in text
    assert "classified twice" in text


def test_unclassified_rows_are_counted_not_failed_yet() -> None:
    rows = platform_boundary.parse_ledger()
    classes, _ = platform_boundary.parse_classes()

    assert platform_boundary.unclassified_rows(rows, classes)
    assert "unclassified=" in platform_boundary.totals(rows)


def test_the_shadow_tests_are_classified_because_they_sit_in_a_test_module() -> None:
    classes, _ = platform_boundary.parse_classes()
    key = (
        "crates/running-process/src/daemon/shadow.rs",
        "attr_cfg",
        "target_os",
    )

    assert classes[key][0] == "host-test"


def test_host_test_is_rejected_when_an_occurrence_is_outside_test_code() -> None:
    source = (
        "#[cfg(unix)]\nfn production() {}\n\n"
        "#[cfg(test)]\nmod tests {\n    #[cfg(unix)]\n    fn checks() {}\n}\n"
    )
    key = ("crates/running-process/src/somewhere.rs", "attr_cfg", "unix")

    violations = platform_boundary.host_test_violations(
        {key: ("host-test", "why")}, {key[0]: source}
    )

    assert len(violations) == 1
    assert "outside test code" in violations[0]


def test_host_test_is_accepted_when_every_occurrence_is_in_a_test_module() -> None:
    source = (
        "fn production() {}\n\n"
        "#[cfg(test)]\nmod tests {\n    #[cfg(unix)]\n    fn checks() {}\n}\n"
    )
    key = ("crates/running-process/src/somewhere.rs", "attr_cfg", "unix")

    assert not platform_boundary.host_test_violations(
        {key: ("host-test", "why")}, {key[0]: source}
    )


def test_a_whole_test_file_needs_no_test_module() -> None:
    key = ("crates/running-process/src/foo/tests.rs", "attr_cfg", "unix")

    assert not platform_boundary.host_test_violations(
        {key: ("host-test", "why")}, {key[0]: "#[cfg(unix)]\nfn t() {}\n"}
    )


def test_braces_in_strings_do_not_unbalance_the_test_module_span() -> None:
    code = platform_boundary.code_only(
        "#[cfg(test)]\nmod tests {\n"
        '    const S: &str = "}}}";\n'
        "    #[cfg(unix)]\n    fn t() {}\n}\n"
    )

    spans = platform_boundary.test_module_spans(code)

    assert spans == [(0, code.rindex("}") + 1)]


def _parse(tmp_path, text):
    path = tmp_path / "classes.tsv"
    path.write_text(text, encoding="utf-8")
    original_root = platform_boundary.ROOT
    platform_boundary.ROOT = tmp_path
    try:
        return platform_boundary.parse_classes(path)
    finally:
        platform_boundary.ROOT = original_root


def test_artifact_format_is_accepted_in_the_probe_artifact_crates(tmp_path) -> None:
    _, problems = _parse(
        tmp_path,
        "crates/running-process-probe/src/snapshot/unwind.rs\tattr_cfg\ttarget_arch"
        "\tartifact-format\tunwinder per arch\n"
        "crates/running-process-probe-worker/src/symbolize.rs\tattr_cfg\ttarget_os"
        "\tartifact-format\tPDB vs DWARF reader\n",
    )

    assert not problems


def test_artifact_format_is_rejected_outside_the_artifact_crates(tmp_path) -> None:
    _, problems = _parse(
        tmp_path,
        "crates/running-process/src/lib.rs\tattr_cfg\tunix"
        "\tartifact-format\tnot an artifact crate\n",
    )

    assert len(problems) == 1
    assert "only valid in the probe artifact crates" in problems[0]


def test_the_probe_format_selectors_are_classified_as_artifact_format() -> None:
    classes, problems = platform_boundary.parse_classes()
    unwind = "crates/running-process-probe/src/snapshot/unwind.rs"
    symbolize = "crates/running-process-probe-worker/src/symbolize.rs"

    assert not problems
    assert classes[(unwind, "attr_cfg", "target_arch")][0] == "artifact-format"
    assert classes[(symbolize, "attr_cfg", "target_os")][0] == "artifact-format"
