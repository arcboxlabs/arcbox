"""Verify frozen ClickHouse configs retain their permissions and validated bytes."""

import hashlib
import os
import stat
import tempfile
import unittest
from pathlib import Path

from run_clickhouse_restart import freeze_config


class FreezeConfigTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.source = Path(directory.name) / "source.xml"
        self.target = Path(directory.name) / "frozen.xml"
        self.contents = b"<clickhouse><logger/></clickhouse>\n"
        self.source.write_bytes(self.contents)
        self.expected = hashlib.sha256(self.contents).hexdigest()

    def freeze_under_umask(self, mask):
        previous = os.umask(mask)
        try:
            freeze_config(self.source, self.target, self.expected)
        finally:
            os.umask(previous)

    def test_readable_config_survives_private_umask(self):
        self.source.chmod(0o644)
        self.freeze_under_umask(0o077)
        self.assertEqual(stat.S_IMODE(self.target.stat().st_mode), 0o644)
        self.assertEqual(self.target.read_bytes(), self.contents)

    def test_private_config_keeps_private_mode(self):
        self.source.chmod(0o600)
        self.freeze_under_umask(0o022)
        self.assertEqual(stat.S_IMODE(self.target.stat().st_mode), 0o600)
        self.assertEqual(self.target.read_bytes(), self.contents)

    def test_hash_mismatch_creates_no_target(self):
        with self.assertRaisesRegex(RuntimeError, "Retained config hash changed"):
            freeze_config(self.source, self.target, "0" * 64)
        self.assertFalse(self.target.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
