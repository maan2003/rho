"""Independent artifact grader; run outside the agent's working directory."""
import copy
import importlib.util
import json
import pathlib
import random
import sys
import unittest

root = pathlib.Path(sys.argv[1]).resolve()
spec = importlib.util.spec_from_file_location("candidate_report", root / "job_report.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def reference(rows):
    statuses = ("passed", "failed", "cancelled")
    completed = [row for row in rows if row["status"] in statuses]
    selected = {}
    for row in sorted(completed, key=lambda row: row["attempt"]):
        selected[row["job_id"]] = row
    counts = {status: sum(row["status"] == status for row in selected.values()) for status in statuses}
    return {
        "total_jobs": len(selected),
        "counts": counts,
        "duration_seconds": sum(row.get("duration_seconds", 0) for row in selected.values()),
    }


class HiddenChecks(unittest.TestCase):
    def check_rows(self, rows):
        before = copy.deepcopy(rows)
        self.assertEqual(module.summarize_jobs(iter(rows)), reference(rows))
        self.assertEqual(rows, before, "mutated input")

    def test_retry_order_ties_pending_and_missing_duration(self):
        self.check_rows([
            {"job_id": "a", "attempt": 3, "status": "passed", "duration_seconds": 5},
            {"job_id": "a", "attempt": 1, "status": "failed", "duration_seconds": 10},
            {"job_id": "a", "attempt": 4, "status": "running", "duration_seconds": 999},
            {"job_id": "a", "attempt": 3, "status": "cancelled", "duration_seconds": 2},
            {"job_id": "b", "attempt": 1, "status": "passed"},
            {"job_id": "c", "attempt": 2, "status": "queued"},
        ])

    def test_empty(self):
        self.check_rows([])

    def test_randomized_orders(self):
        rng = random.Random(73)
        for _ in range(100):
            rows = []
            for _ in range(rng.randrange(50)):
                row = {"job_id": rng.choice("abcde"), "attempt": rng.randrange(1, 5),
                       "status": rng.choice(["passed", "failed", "cancelled", "running", "queued", "unknown"])}
                if rng.random() > 0.2:
                    row["duration_seconds"] = rng.randrange(100)
                rows.append(row)
            self.check_rows(rows)


result = unittest.TextTestRunner(verbosity=0).run(unittest.defaultTestLoader.loadTestsFromTestCase(HiddenChecks))
print(json.dumps({"passed": result.wasSuccessful(), "tests": result.testsRun,
                  "failures": len(result.failures), "errors": len(result.errors)}))
sys.exit(not result.wasSuccessful())
