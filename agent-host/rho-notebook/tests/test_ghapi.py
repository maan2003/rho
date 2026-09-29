"""Exercise the selected upstream ghapi code against an Octo-shaped Unix socket."""
import asyncio
import json
import os
import tempfile
import unittest
from unittest.mock import patch

from ghapi.all import GhApi


class OctoGhApiTest(unittest.IsolatedAsyncioTestCase):
    async def test_requests_and_complete_pr_status(self):
        with tempfile.TemporaryDirectory() as directory:
            requests = []

            async def serve(reader, writer):
                method, path, _ = (await reader.readline()).decode().split()
                headers = {}
                while line := (await reader.readline()).decode().strip():
                    key, value = line.split(":", 1)
                    headers[key.lower()] = value.strip()
                body = await reader.readexactly(int(headers.get("content-length", 0)))
                requests.append((method, path, headers, body))
                status = "200 OK"
                if path.endswith("/pulls/17"):
                    result = {"head": {"sha": "a" * 40}}
                elif path.endswith("/status"):
                    result = {"state": "pending", "statuses": [{"context": "legacy", "state": "success"}]}
                elif "/check-runs?" in path:
                    def run(i, conclusion):
                        return {"id": i, "name": f"check-{i}", "status": "completed",
                                "conclusion": conclusion, "started_at": None, "completed_at": None}
                    page = path.rsplit("page=", 1)[-1]
                    result = {"total_count": 101,
                              "check_runs": ([run(i, "success") for i in range(100)]
                                             if page == "1" else [run(100, "failure")])}
                elif "/issues?" in path:
                    result = [{"number": 42}]
                else:
                    result = {"number": 18, "draft": json.loads(body).get("draft", False)} if method == "POST" else {"number": 18}
                payload = json.dumps(result).encode()
                writer.write(f"HTTP/1.1 {status}\r\nContent-Type: application/json\r\n"
                             f"X-RateLimit-Remaining: 4900\r\nX-RateLimit-Limit: 5000\r\n"
                             f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload)
                await writer.drain()
                writer.close()

            server = await asyncio.start_unix_server(serve, directory + "/octo.sock")
            try:
                with patch.dict(os.environ, {"RHO_SOCKET_PATH": directory + "/rho.sock"}):
                    async with server:
                        limits = []
                        api = GhApi("acme", "widget", limit_cb=lambda remaining, quota: limits.append((remaining, quota)))
                        issues = await api.issues.list_for_repo(state="open", per_page=20)
                        status = await api.pr_status(17)
                        draft = await api.pulls.create(head="fix", base="main", title="Fix", draft=True)
                        self.assertEqual(limits, [(4900, 5000)])
                        self.assertEqual(issues[0].number, 42)
                        self.assertEqual(draft.number, 18)
                        self.assertEqual(status.state, "pending")
                        self.assertEqual(len(status.check_runs), 101)
                        self.assertIn("**failure**", repr(status))
                        self.assertEqual(json.loads(requests[-1][3])["draft"], True)
                        self.assertTrue(all("authorization" not in headers for _, _, headers, _ in requests))
                        self.assertEqual(requests[3][1].rsplit("page=", 1)[-1], "1")
                        self.assertEqual(requests[4][1].rsplit("page=", 1)[-1], "2")
                        normal = await api.pulls.create(head="fix", base="main", title="Fix", draft=False)
                        self.assertIs(normal.draft, False)
                        self.assertIs(json.loads(requests[-1][3])["draft"], False)
                        implicit = await api.pulls.create(head="fix", base="main", title="Fix")
                        self.assertIs(implicit.draft, False)
                        self.assertNotIn("draft", json.loads(requests[-1][3]))
                        with self.assertRaises(ValueError):
                            api("https://api.github.com/repos/acme/widget/issues")
                        with self.assertRaises(TypeError):
                            GhApi(token="not-a-host-credential")
            finally:
                server.close()
                await server.wait_closed()

    async def test_edit_and_feedback_methods_use_selected_paths_and_bodies(self):
        with tempfile.TemporaryDirectory() as directory:
            requests = []

            async def serve(reader, writer):
                method, path, _ = (await reader.readline()).decode().split()
                headers = {}
                while line := (await reader.readline()).decode().strip():
                    key, value = line.split(":", 1)
                    headers[key.lower()] = value.strip()
                body = await reader.readexactly(int(headers.get("content-length", 0)))
                requests.append((method, path, json.loads(body) if body else None))
                result = ({"review_decision": "APPROVED"} if path.endswith("/review-decision")
                          else [{"id": 7}] if method == "GET" else {"id": 8})
                payload = json.dumps(result).encode()
                writer.write(
                    f"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                    f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload
                )
                await writer.drain()
                writer.close()

            server = await asyncio.start_unix_server(serve, directory + "/octo.sock")
            try:
                with patch.dict(os.environ, {"RHO_SOCKET_PATH": directory + "/rho.sock"}):
                    async with server:
                        api = GhApi("acme", "widget")
                        await api.pulls.update(17, title="New title", body="New description")
                        await api.pulls.update(17, base="release/next")
                        self.assertEqual((await api.pulls.review_decision(17)).review_decision, "APPROVED")
                        await api.issues.list_comments(17, page=2, per_page=5)
                        await api.pulls.list_reviews(17, page=3)
                        await api.pulls.list_review_comments(17, page=4)
                        await api.issues.create_comment(17, body="General reply")
                        await api.pulls.create_reply_for_review_comment(
                            17, 99, body="Inline reply"
                        )
            finally:
                server.close()
                await server.wait_closed()
            self.assertEqual(
                requests,
                [
                    ("PATCH", "/repos/acme/widget/pulls/17", {
                        "title": "New title", "body": "New description"
                    }),
                    ("PATCH", "/repos/acme/widget/pulls/17", {"base": "release/next"}),
                    ("GET", "/repos/acme/widget/pulls/17/review-decision", None),
                    ("GET", "/repos/acme/widget/issues/17/comments?page=2&per_page=5", None),
                    ("GET", "/repos/acme/widget/pulls/17/reviews?page=3", None),
                    ("GET", "/repos/acme/widget/pulls/17/comments?page=4", None),
                    ("POST", "/repos/acme/widget/issues/17/comments", {"body": "General reply"}),
                    ("POST", "/repos/acme/widget/pulls/17/comments/99/replies", {"body": "Inline reply"}),
                ],
            )

    async def test_actions_inspection_logs_and_all_rerun_scopes(self):
        with tempfile.TemporaryDirectory() as directory:
            requests = []

            async def serve(reader, writer):
                method, path, _ = (await reader.readline()).decode().split()
                headers = {}
                while line := (await reader.readline()).decode().strip():
                    key, value = line.split(":", 1)
                    headers[key.lower()] = value.strip()
                body = await reader.readexactly(int(headers.get("content-length", 0)))
                requests.append((method, path, json.loads(body) if body else None, headers))
                if path.endswith("/logs"):
                    content_type = "text/plain" if "/jobs/" in path else "application/zip"
                    payload = b"failure at step 3\n" if "/jobs/" in path else b"PK\x03\x04archive"
                    status = "200 OK"
                elif method == "POST":
                    content_type, payload, status = "application/json", b"", "201 Created"
                else:
                    content_type, payload, status = "application/json", b'{"id": 9}', "200 OK"
                writer.write(
                    f"HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n"
                    f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload
                )
                await writer.drain()
                writer.close()

            server = await asyncio.start_unix_server(serve, directory + "/octo.sock")
            try:
                with patch.dict(os.environ, {"RHO_SOCKET_PATH": directory + "/rho.sock"}):
                    async with server:
                        api = GhApi("acme", "widget")
                        await api.pulls.list_files(17, per_page=20)
                        await api.checks.get(7)
                        await api.checks.list_annotations(7, page=2)
                        await api.actions.list_workflow_runs_for_repo(head_sha="a"*40, page=3)
                        await api.actions.get_workflow_run(11)
                        await api.actions.list_jobs_for_workflow_run(11, filter="all")
                        await api.actions.get_job_for_workflow_run(12)
                        self.assertEqual(await api.actions.download_job_logs_for_workflow_run(12),
                                         "failure at step 3\n")
                        self.assertEqual(await api.actions.download_workflow_run_logs(11),
                                         b"PK\x03\x04archive")
                        await api.actions.re_run_workflow(11, enable_debug_logging=True)
                        await api.actions.re_run_workflow_failed_jobs(11)
                        await api.actions.re_run_job_for_workflow_run(12, enable_debugger=True)
            finally:
                server.close()
                await server.wait_closed()
            self.assertEqual(
                [(method, path, body) for method, path, body, _ in requests],
                [
                    ("GET", "/repos/acme/widget/pulls/17/files?per_page=20", None),
                    ("GET", "/repos/acme/widget/check-runs/7", None),
                    ("GET", "/repos/acme/widget/check-runs/7/annotations?page=2", None),
                    ("GET", "/repos/acme/widget/actions/runs?head_sha="+"a"*40+"&page=3", None),
                    ("GET", "/repos/acme/widget/actions/runs/11", None),
                    ("GET", "/repos/acme/widget/actions/runs/11/jobs?filter=all", None),
                    ("GET", "/repos/acme/widget/actions/jobs/12", None),
                    ("GET", "/repos/acme/widget/actions/jobs/12/logs", None),
                    ("GET", "/repos/acme/widget/actions/runs/11/logs", None),
                    ("POST", "/repos/acme/widget/actions/runs/11/rerun", {"enable_debug_logging": True}),
                    ("POST", "/repos/acme/widget/actions/runs/11/rerun-failed-jobs", {}),
                    ("POST", "/repos/acme/widget/actions/jobs/12/rerun", {"enable_debugger": True}),
                ],
            )
            self.assertTrue(all("authorization" not in headers for _, _, _, headers in requests))



if __name__ == "__main__":
    unittest.main()
