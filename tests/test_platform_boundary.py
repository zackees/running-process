from __future__ import annotations

from ci import platform_boundary


def test_bootstrap_ledgers_are_valid() -> None:
    rows = platform_boundary.parse_ledger()

    assert len(rows) == 680
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
