"""Pure safety and cleanup checks for the staged local-model evaluator."""

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from run import HERE, endpoint_url, tidy_subtitle


class EvaluatorTests(unittest.TestCase):
    def test_rejects_production_and_non_loopback_endpoints(self) -> None:
        with self.assertRaises(ValueError):
            endpoint_url("http://127.0.0.1:11434", "/api/tags")
        with self.assertRaises(ValueError):
            endpoint_url("https://example.test:11435", "/api/tags")

    def test_accepts_only_an_isolated_loopback_endpoint(self) -> None:
        self.assertEqual(endpoint_url("http://127.0.0.1:11435", "/api/tags"),
                         "http://127.0.0.1:11435/api/tags")

    def test_subtitle_step_cleanup_is_leading_only(self) -> None:
        self.assertEqual(tidy_subtitle("sTeP: Build portable bridge", None),
                         ("Build portable bridge", None))
        self.assertEqual(tidy_subtitle("Plan the next step: review markers", None),
                         ("Plan the next step: review markers", None))

    def test_config_points_to_synthetic_fixture_sources(self) -> None:
        config = json.loads((HERE / "config.json").read_text())
        self.assertTrue(config["checkpoint_source"].startswith("fixtures/"))
        self.assertTrue(config["gold_source"].startswith("fixtures/"))


if __name__ == "__main__":
    unittest.main()
