from __future__ import annotations

from running_process._native import native_test_capture_rust_debug_trace


def test_native_test_capture_rust_debug_trace_includes_nested_frames() -> None:
    trace = native_test_capture_rust_debug_trace()

    assert "running_process_py::native_test_capture_rust_debug_trace::outer" in trace
    assert "running_process_py::native_test_capture_rust_debug_trace::inner" in trace
