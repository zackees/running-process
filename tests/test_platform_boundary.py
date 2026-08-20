from __future__ import annotations

from ci import platform_boundary


def test_bootstrap_ledgers_are_valid() -> None:
    rows = platform_boundary.parse_ledger()

    assert len(rows) == 1584
    assert not platform_boundary.validate_ledger(rows)
    assert not platform_boundary.manifest_dependency_violations()
    assert not platform_boundary.neutral_facade_contract_violations()


def test_every_ledger_group_has_contiguous_ordinals() -> None:
    rows = platform_boundary.parse_ledger()
    groups: dict[tuple[str, str, str], list[int]] = {}
    for row in rows:
        groups.setdefault((row.path, row.kind, row.normalized), []).append(row.ordinal)

    assert all(sorted(ordinals) == list(range(len(ordinals))) for ordinals in groups.values())
