"""Refuse a mismatched free-threaded import instead of faulting (#1142).

The published wheels are ABI3 builds of the regular, GIL-enabled CPython
runtime. On Windows an ABI3 extension is installed as a bare ``_native.pyd``,
and a free-threaded interpreter sharing the same ``site-packages`` will happily
load it and fault inside ``create_module`` — the segfault reported in #1142.

pip and uv already refuse to *install* an ABI3 wheel into a free-threaded
interpreter, so this only fires for an interpreter reaching an extension that
was installed for a different runtime: two interpreters sharing one
``site-packages``, or a local dev build. Importing this module first turns that
crash into the ``ImportError`` #1142 asked for.

Importing it is the check; it has no public API.
"""

from __future__ import annotations

import sysconfig
from pathlib import Path

_EXTENSION_SUFFIXES = (".so", ".pyd", ".dylib")


def _mismatched_extensions() -> list[str]:
    """Return installed ``_native`` extensions this interpreter must not load.

    Empty on a GIL-enabled interpreter, and empty on a free-threaded one whose
    exact tagged extension is present — only a build for the *other* runtime is
    reported.
    """
    if not sysconfig.get_config_var("Py_GIL_DISABLED"):
        return []
    suffix = sysconfig.get_config_var("EXT_SUFFIX")
    installed = sorted(
        path.name
        for path in Path(__file__).resolve().parent.glob("_native*")
        if path.suffix in _EXTENSION_SUFFIXES
    )
    if not installed:
        # Nothing to load; let the normal import machinery report it.
        return []
    if suffix and f"_native{suffix}" in installed:
        return []
    return installed


def _check() -> None:
    mismatched = _mismatched_extensions()
    if not mismatched:
        return
    raise ImportError(
        "running_process's native extension was built for the GIL-enabled "
        f"CPython runtime ({', '.join(mismatched)}) and cannot be loaded by "
        "this free-threaded interpreter; loading it anyway crashes the process "
        "(issue #1142). No free-threaded wheel is published yet — import "
        "running_process from the regular interpreter, or give the "
        "free-threaded interpreter its own environment and build from source "
        "with `pip install --no-binary running-process running-process`."
    )


_check()
