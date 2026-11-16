"""The formatter wrapper must honor its pin and propagate tool failures."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class FormatterTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "scripts").mkdir()
        (self.root / "bin").mkdir()
        shutil.copyfile(ROOT / "scripts/fmt.sh", self.root / "scripts/fmt.sh")
        self.pin = (ROOT / "rustfmt-toolchain").read_text().strip()
        (self.root / "rustfmt-toolchain").write_text(self.pin + "\n")
        self.capture = self.root / "arguments"
        self.environment = os.environ | {
            "PATH": str(self.root / "bin") + os.pathsep + os.environ["PATH"],
            "FMT_TEST_CAPTURE": self.capture.as_posix(),
            "FMT_TEST_EXIT": "0",
        }
        self.stub("rustup", "exit 0\n")
        self.stub("cargo", 'printf "%s\\n" "$@" > "$FMT_TEST_CAPTURE" || exit 1\nexit "$FMT_TEST_EXIT"\n')

    def stub(self, name, body):
        path = self.root / "bin" / name
        path.write_text("#!/bin/sh\n" + body, newline="\n")
        path.chmod(0o755)

    def run_formatter(self):
        return subprocess.run(
            ["sh", "scripts/fmt.sh", "--check"], cwd=self.root,
            env=self.environment, capture_output=True, text=True, timeout=15,
        )

    def test_pinned_formatter_receives_check_argument(self):
        result = self.run_formatter()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.capture.read_text().splitlines(), ["+" + self.pin, "fmt", "--all", "--", "--check"])

    def test_formatter_failure_is_not_reported_as_success(self):
        self.environment["FMT_TEST_EXIT"] = "17"
        self.assertEqual(self.run_formatter().returncode, 17)

    def test_missing_pin_fails_before_cargo_is_started(self):
        (self.root / "rustfmt-toolchain").unlink()
        self.assertNotEqual(self.run_formatter().returncode, 0)
        self.assertFalse(self.capture.exists())


if __name__ == "__main__":
    unittest.main()
