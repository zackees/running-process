"""Remote writer permission must not disable local compiler cache reuse."""

import unittest
from pathlib import Path


class RemoteCacheGuardTests(unittest.TestCase):
    def test_explicit_remote_permission_cannot_enable_pr_saves(self):
        from ci.cache_save_guard import check_text

        text = """jobs:
  test:
    steps:
      - uses: zackees/setup-soldr@v0
        with:
          save-cache: auto
          save-cache-remote: true
"""
        violations = check_text(Path("workflow.yml"), text)
        assert len(violations) == 1
        assert "save-cache-remote" in violations[0].message

    def test_remote_main_only_keeps_local_auto_valid(self):
        from ci.cache_save_guard import check_text

        text = """jobs:
  test:
    steps:
      - uses: zackees/setup-soldr@v0
        with:
          save-cache: auto
          save-cache-remote: ${{ github.ref == 'refs/heads/main' && 'auto' || 'false' }}
"""
        assert check_text(Path("workflow.yml"), text) == []

    def test_explicit_global_false_disables_every_writer(self):
        from ci.cache_save_guard import check_text

        text = """jobs:
  test:
    steps:
      - uses: zackees/setup-soldr@v0
        with:
          save-cache: false
          save-cache-remote: true
"""
        assert check_text(Path("workflow.yml"), text) == []
