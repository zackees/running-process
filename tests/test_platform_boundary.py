from __future__ import annotations

from ci import platform_boundary


def test_bootstrap_ledgers_are_valid() -> None:
    rows = platform_boundary.parse_ledger()

    assert len(rows) == 1019
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
