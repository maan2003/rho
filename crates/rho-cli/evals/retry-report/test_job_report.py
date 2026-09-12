import unittest

from job_report import summarize_jobs


class JobReportTests(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(summarize_jobs([]), {
            "total_jobs": 0,
            "counts": {"passed": 0, "failed": 0, "cancelled": 0},
            "duration_seconds": 0,
        })

    def test_single_attempt_jobs(self):
        self.assertEqual(summarize_jobs([
            {"job_id": "build", "attempt": 1, "status": "passed", "duration_seconds": 12},
            {"job_id": "lint", "attempt": 1, "status": "failed", "duration_seconds": 3},
        ]), {
            "total_jobs": 2,
            "counts": {"passed": 1, "failed": 1, "cancelled": 0},
            "duration_seconds": 15,
        })


if __name__ == "__main__":
    unittest.main()
