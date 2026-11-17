#!/usr/bin/env python3
"""Tests for alternating comparative benchmark orchestration."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest


ROOT = Path(__file__).resolve().parents[1]
PERFORMANCE = ROOT / "tests" / "performance"
sys.path.insert(0, str(PERFORMANCE))
SPEC = importlib.util.spec_from_file_location("masil_performance_compare", PERFORMANCE / "compare.py")
assert SPEC and SPEC.loader
compare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compare)


def benchmark_report(product: str, sha: str = "abc") -> dict[str, object]:
    geometry = [{"columns": 93, "rows": 39}]
    measurements = {
        "status": "measured",
        "attached_input_rtt": {"inner_geometry": geometry},
        "idle": [{"pane_count": 1, "inner_geometry": geometry}],
        "output": [],
    }
    return {
        "harness": {"benchmark_sha256": "bench", "fixture_sha256": "fixture"},
        "methodology": {"pane_counts": [1]},
        "host": {"machine": "test"},
        "products": {
            product: {
                "binary": {"sha256": sha, "version_output": product},
                "measurements": measurements,
            }
        }
    }


class PerformanceCompareTest(unittest.TestCase):
    def test_order_reverses_each_round(self) -> None:
        products = ["masil", "herdr"]
        self.assertEqual(compare.alternating_order(products, 0), ["masil", "herdr"])
        self.assertEqual(compare.alternating_order(products, 1), ["herdr", "masil"])
        self.assertEqual(compare.alternating_order(products, 2), ["masil", "herdr"])

    def test_assessment_requires_stable_provenance_and_matched_geometry(self) -> None:
        runs = []
        for round_index in range(2):
            for product in compare.alternating_order(["masil", "herdr"], round_index):
                runs.append(
                    {
                        "round": round_index,
                        "product": product,
                        "exit_code": 0,
                        "report": benchmark_report(product),
                    }
                )

        result = compare.assess_runs(runs, ["masil", "herdr"], 2)

        self.assertEqual(result["status"], "valid")
        self.assertTrue(result["direct_comparison_supported"])

    def test_assessment_rejects_a_binary_change(self) -> None:
        runs = [
            {"round": 0, "product": "masil", "exit_code": 0, "report": benchmark_report("masil")},
            {"round": 0, "product": "herdr", "exit_code": 0, "report": benchmark_report("herdr")},
            {"round": 1, "product": "herdr", "exit_code": 0, "report": benchmark_report("herdr", "changed")},
            {"round": 1, "product": "masil", "exit_code": 0, "report": benchmark_report("masil")},
        ]

        result = compare.assess_runs(runs, ["masil", "herdr"], 2)

        self.assertEqual(result["status"], "invalid")
        self.assertIn("herdr: binary provenance changed between rounds", result["errors"])

    def test_assessment_rejects_changed_harness_or_methodology(self) -> None:
        changed = benchmark_report("herdr")
        changed["harness"] = {"benchmark_sha256": "different", "fixture_sha256": "fixture"}
        runs = [
            {"round": 0, "product": "masil", "exit_code": 0, "report": benchmark_report("masil")},
            {"round": 0, "product": "herdr", "exit_code": 0, "report": benchmark_report("herdr")},
            {"round": 1, "product": "herdr", "exit_code": 0, "report": changed},
            {"round": 1, "product": "masil", "exit_code": 0, "report": benchmark_report("masil")},
        ]

        result = compare.assess_runs(runs, ["masil", "herdr"], 2)

        self.assertEqual(result["status"], "invalid")
        self.assertTrue(
            any("harness hashes, methodology, or host metadata changed" in error for error in result["errors"])
        )


if __name__ == "__main__":
    unittest.main()
