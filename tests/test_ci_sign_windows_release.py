"""Tests for Azure Artifact Signing of the Windows release archives.

Signing itself needs Azure credentials and only runs in auto-release.yml; these
tests cover everything around it: the pinned jsign command line, the
structural signature check that fails the job on an unsigned PE, the wheel
RECORD rewrite, the zip repack, and the workflow wiring that makes every
publish job wait for signing. PE images are assembled with ``struct.pack`` so
no binary fixture ships.
"""

from __future__ import annotations

import struct
import subprocess
import zipfile
from pathlib import Path

import pytest

from ci import sign_windows_release as sign

ROOT = Path(__file__).resolve().parents[1]
PUBLISHER = "Zachary Vorhies"
PYD = "running_process/_native.pyd"
TRAMPOLINE = "running_process/assets/daemon-trampoline.exe"
RECORD = "running_process-4.6.4.dist-info/RECORD"
WHEEL = "running_process-4.6.4-cp310-abi3-win_amd64.whl"
ZIP = "running-process-4.6.4-aarch64-pc-windows-msvc.zip"
ZIP_DIR = "running-process-4.6.4-aarch64-pc-windows-msvc"


def make_pe(cert: bytes | None = None, *, pe32_plus: bool = True) -> bytes:
    """A minimal PE whose security directory points at ``cert`` (if any)."""
    pe_offset = 0x40
    image = bytearray(0x200)
    image[0:2] = b"MZ"
    struct.pack_into("<I", image, 0x3C, pe_offset)
    image[pe_offset : pe_offset + 4] = b"PE\0\0"
    optional = pe_offset + 24
    struct.pack_into("<H", image, optional, 0x20B if pe32_plus else 0x10B)
    if cert is not None:
        directories = optional + (112 if pe32_plus else 96)
        struct.pack_into("<II", image, directories + 4 * 8, len(image), len(cert))
        image += cert
    return bytes(image)


def win_certificate(payload: bytes) -> bytes:
    return struct.pack("<IHH", 8 + len(payload), 0x0200, 0x0002) + payload


SIGNED_PE = make_pe(
    win_certificate(b"\x30\x82" + PUBLISHER.encode() + sign.RFC3161_TIMESTAMP_OID)
)
CONFIG = sign.SigningConfig("eus.codesigning.azure.net", "acct", "prof", PUBLISHER)


def make_wheel(path: Path, members: dict[str, bytes]) -> Path:
    record = "".join(
        f"{n},{sign.record_hash(d)},{len(d)}\n" for n, d in members.items()
    )
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in members.items():
            archive.writestr(name, data)
        archive.writestr(RECORD, record + f"{RECORD},,\n")
    return path


def make_zip(path: Path, members: dict[str, bytes]) -> Path:
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in members.items():
            archive.writestr(name, data)
    return path


def fake_jsign(seen: list[list[str]]):
    def run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        env = kwargs["env"]
        assert isinstance(env, dict)
        assert env[sign.TOKEN_ENV] == "token"
        seen.append(command)
        for arg in command[command.index("--replace") + 1 :]:
            Path(arg).write_bytes(SIGNED_PE)
        return subprocess.CompletedProcess(command, 0)

    return run


def test_unsigned_pe_is_rejected() -> None:
    assert "no Authenticode signature" in (
        sign.signature_problem(make_pe(), PUBLISHER) or ""
    )


@pytest.mark.parametrize("pe32_plus", [True, False])
def test_timestamped_signature_by_publisher_passes(pe32_plus: bool) -> None:
    image = make_pe(
        win_certificate(PUBLISHER.encode() + sign.RFC3161_TIMESTAMP_OID),
        pe32_plus=pe32_plus,
    )
    assert sign.signature_problem(image, PUBLISHER) is None


def test_wrong_publisher_and_missing_timestamp_are_rejected() -> None:
    other = make_pe(win_certificate(b"CN=Someone Else" + sign.RFC3161_TIMESTAMP_OID))
    assert "publisher" in (sign.signature_problem(other, PUBLISHER) or "")
    untimed = make_pe(win_certificate(PUBLISHER.encode()))
    assert "timestamp" in (sign.signature_problem(untimed, PUBLISHER) or "")


def test_verify_fails_on_an_unsigned_trampoline(tmp_path: Path) -> None:
    wheel = make_wheel(tmp_path / WHEEL, {PYD: SIGNED_PE, TRAMPOLINE: make_pe()})
    with pytest.raises(sign.SigningError, match=r"daemon-trampoline\.exe"):
        sign.verify_archive(wheel, PUBLISHER)


def test_verify_fails_on_a_stale_record(tmp_path: Path) -> None:
    wheel = make_wheel(tmp_path / WHEEL, {PYD: make_pe(), TRAMPOLINE: make_pe()})
    infos, members = sign.read_archive(wheel)
    members[PYD] = SIGNED_PE
    members[TRAMPOLINE] = SIGNED_PE
    sign.write_archive(wheel, infos, members)
    with pytest.raises(sign.SigningError, match="RECORD"):
        sign.verify_archive(wheel, PUBLISHER)


def test_verify_requires_at_least_one_pe(tmp_path: Path) -> None:
    archive = make_zip(tmp_path / ZIP, {f"{ZIP_DIR}/README.md": b""})
    with pytest.raises(sign.SigningError, match="no Windows PE"):
        sign.verify_archive(archive, PUBLISHER)


def test_sign_archives_signs_wheels_and_zips_in_one_call(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    wheel = make_wheel(
        tmp_path / WHEEL,
        {"running_process/__init__.py": b"x", PYD: make_pe(), TRAMPOLINE: make_pe()},
    )
    archive = make_zip(
        tmp_path / ZIP,
        {
            f"{ZIP_DIR}/runpm.exe": make_pe(),
            f"{ZIP_DIR}/running-process-daemon.exe": make_pe(),
            f"{ZIP_DIR}/LICENSE": b"MIT",
        },
    )
    seen: list[list[str]] = []
    monkeypatch.setattr(sign, "access_token", lambda: "token")
    monkeypatch.setattr(sign.subprocess, "run", fake_jsign(seen))
    sign.sign_archives(Path("jsign.jar"), CONFIG, [wheel, archive])
    assert len(seen) == 1
    assert "token" not in seen[0]
    assert len(seen[0][seen[0].index("--replace") + 1 :]) == 4
    sign.verify_archive(wheel, PUBLISHER)
    sign.verify_archive(archive, PUBLISHER)
    _infos, members = sign.read_archive(wheel)
    assert list(members) == ["running_process/__init__.py", PYD, TRAMPOLINE, RECORD]
    _infos, members = sign.read_archive(archive)
    assert members[f"{ZIP_DIR}/LICENSE"] == b"MIT"


def test_resolve_archives_rejects_non_windows_and_empty_globs(tmp_path: Path) -> None:
    with pytest.raises(sign.SigningError, match="no release archive"):
        sign.resolve_archives([str(tmp_path / "*.whl")])
    linux = tmp_path / "running_process-4.6.4-cp310-abi3-manylinux_2_28_x86_64.whl"
    linux.write_bytes(b"")
    with pytest.raises(sign.SigningError, match="not a Windows"):
        sign.resolve_archives([str(linux)])
    (tmp_path / WHEEL).write_bytes(b"")
    (tmp_path / ZIP).write_bytes(b"")
    found = sign.resolve_archives(
        [str(tmp_path / "*-win_*.whl"), str(tmp_path / "*.zip")]
    )
    assert [p.name for p in found] == [WHEEL, ZIP]


def test_endpoint_and_publisher_normalisation() -> None:
    assert (
        sign.endpoint_host("https://eus.codesigning.azure.net/")
        == "eus.codesigning.azure.net"
    )
    with pytest.raises(sign.SigningError):
        sign.endpoint_host("https://example.com/")
    dn = "CN=Zachary Vorhies, O=Zachary Vorhies, L=Berkeley, C=US"
    assert sign.publisher_common_name(dn) == PUBLISHER


def test_jsign_command_keeps_the_token_off_argv() -> None:
    command = sign.jsign_command(Path("jsign.jar"), CONFIG, [Path("a.exe")])
    assert command[command.index("--storetype") + 1] == "TRUSTEDSIGNING"
    assert command[command.index("--keystore") + 1] == "eus.codesigning.azure.net"
    assert command[command.index("--alias") + 1] == "acct/prof"
    assert command[command.index("--storepass") + 1] == f"env:{sign.TOKEN_ENV}"
    assert (
        command[command.index("--tsaurl") + 1] == "http://timestamp.acs.microsoft.com"
    )
    assert command[command.index("--tsmode") + 1] == "RFC3161"
    assert command[command.index("--alg") + 1] == "SHA-256"
    assert command[-1] == "a.exe"
    assert sign.JSIGN_VERSION == "7.6"
    assert (
        sign.JSIGN_SHA256
        == "6cf58aa080e7ff82656f320cff8d4a3bff30408df8766806dcba3740dd839fa5"
    )


def _job(workflow: str, name: str) -> str:
    start = workflow.index(f"\n  {name}:\n")
    nxt = workflow.find("\n  ", start + len(name) + 4)
    while nxt != -1 and workflow[nxt + 3] == " ":
        nxt = workflow.find("\n  ", nxt + 1)
    return workflow[start:] if nxt == -1 else workflow[start:nxt]


def test_every_publish_job_waits_for_windows_signing() -> None:
    workflow = (ROOT / ".github/workflows/auto-release.yml").read_text(encoding="utf-8")
    job = _job(workflow, "sign-windows")
    assert "id-token: write" in job
    assert "environment: release-signing" in job
    assert "azure/login@a457da9ea143d694b1b9c7c869ebb04ebe844ef5" in job
    assert job.index("sign_windows_release.py sign") < job.index(
        "sign_windows_release.py check"
    )
    assert job.index("sign_windows_release.py check") < job.index("upload-artifact")
    assert job.count("overwrite: true") == 4
    for publisher in ("publish-pypi", "publish-release"):
        assert "      - sign-windows\n" in _job(workflow, publisher)
    assert "needs.sign-windows.result == 'success'" in _job(workflow, "publish-release")
