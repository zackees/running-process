"""Keyboard interrupt handler for managing interrupts across threads."""

from __future__ import annotations

import _thread
import threading


def is_main_thread() -> bool:
    """Check if we're running in the main thread."""
    return threading.current_thread() is threading.main_thread()


def handle_keyboard_interrupt(exc: KeyboardInterrupt) -> None:
    """
    Handle KeyboardInterrupt properly across main and worker threads.

    In the main thread, this will raise the exception.
    In worker threads, this will interrupt the main thread.

    Args:
        exc: The KeyboardInterrupt exception to handle
    """
    if is_main_thread():
        raise exc
    # In worker thread, notify main thread
    _thread.interrupt_main()
    raise exc
