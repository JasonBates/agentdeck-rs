"""Public-source hygiene checks for the synthetic evaluator."""

from __future__ import annotations

import json
import subprocess
import unittest
from pathlib import Path


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
FIXTURES = HERE / "fixtures"
PUBLIC_FILES = (
    HERE / "README.md",
    HERE / "config.json",
    HERE / "run.py",
    HERE / "test_run.py",
    FIXTURES / "checkpoints.json",
    FIXTURES / "gold.json",
)
ALLOWED_HARNESS_FILES = {
    ".gitignore",
    "README.md",
    "config.json",
    "fixtures/checkpoints.json",
    "fixtures/gold.json",
    "run.py",
    "test_public_hygiene.py",
    "test_run.py",
}
HOME_DISCOVERY_MARKERS = (
    "/Users/",
    "Path.home()",
    ".claude/",
    ".codex/",
    ".pi/agent",
    "session-state/",
    ".rglob(",
)


class PublicHygieneTests(unittest.TestCase):
    def test_config_uses_only_synthetic_fixture_sources(self) -> None:
        config = json.loads((HERE / "config.json").read_text())
        for key in ("checkpoint_source", "gold_source"):
            source = config[key]
            self.assertTrue(source.startswith("fixtures/"), source)
            self.assertTrue((HERE / source).is_file(), source)

    def test_fixture_schema_is_small_and_synthetic(self) -> None:
        checkpoints = json.loads((FIXTURES / "checkpoints.json").read_text())
        gold = json.loads((FIXTURES / "gold.json").read_text())
        self.assertEqual(len(checkpoints), 10)
        self.assertEqual(set(gold), {checkpoint["id"] for checkpoint in checkpoints})
        for checkpoint in checkpoints:
            self.assertEqual(
                set(checkpoint), {"id", "title", "prev_reply", "last_prompt", "recent"}
            )
            self.assertRegex(checkpoint["id"], r"^synthetic-[0-9]{2}$")
            self.assertTrue(all(len(value) <= 200 for value in checkpoint.values()))

    def test_public_harness_has_no_home_discovery_markers(self) -> None:
        for path in PUBLIC_FILES:
            text = path.read_text(encoding="utf-8")
            for marker in HOME_DISCOVERY_MARKERS:
                self.assertNotIn(marker, text, f"{path}: {marker}")

    def test_harness_contains_only_documented_source_files(self) -> None:
        source_files = {
            path.relative_to(HERE).as_posix()
            for path in HERE.rglob("*")
            if path.is_file()
            and path.suffix in {"", ".json", ".md", ".py"}
            and path.name != "RESULTS.md"
            and "runs" not in path.relative_to(HERE).parts
        }
        self.assertEqual(source_files, ALLOWED_HARNESS_FILES)

    def test_local_run_artifacts_and_results_are_ignored(self) -> None:
        # Check a representative file below the ignored directory: a pristine
        # clone has no `runs/` directory yet, and Git does not apply a trailing-
        # slash directory pattern to that nonexistent path without a child.
        for path in (HERE / "runs" / "example.json", HERE / "RESULTS.md"):
            result = subprocess.run(
                ["git", "check-ignore", "--no-index", "-q", str(path.relative_to(ROOT))],
                cwd=ROOT,
            )
            self.assertEqual(result.returncode, 0, path)


if __name__ == "__main__":
    unittest.main()
