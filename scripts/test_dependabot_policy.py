"""Dependabot merges must use the dedicated App without release authority."""

import unittest
from pathlib import Path


class DependabotPolicyTests(unittest.TestCase):
    def test_dedicated_merge_identity(self):
        root = Path(__file__).resolve().parents[1]
        path = root / ".github/workflows/dependabot-merge.yml"
        self.assertTrue(path.exists(), "A trusted completed-CI handler is required")
        workflow = path.read_text()
        self.assertNotIn("secrets.RELEASE_WRITER_APP_", workflow)
        self.assertIn("app-id: ${{ secrets.RELEASE_MERGER_APP_ID }}", workflow)
        self.assertIn("private-key: ${{ secrets.RELEASE_MERGER_APP_KEY }}", workflow)
        action = workflow.split("merge-tested-dependabot@", 1)[1]
        self.assertIn("app-id: ${{ secrets.RELEASE_MERGER_APP_ID }}", action)
        self.assertIn("workflow_run:", workflow)
        self.assertNotIn("pull_request_target:", workflow)


if __name__ == "__main__":
    unittest.main()
