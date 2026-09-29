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
                result = [{"id": 7}] if method == "GET" else {"id": 8}
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
                    ("GET", "/repos/acme/widget/issues/17/comments?page=2&per_page=5", None),
                    ("GET", "/repos/acme/widget/pulls/17/reviews?page=3", None),
                    ("GET", "/repos/acme/widget/pulls/17/comments?page=4", None),
                    ("POST", "/repos/acme/widget/issues/17/comments", {"body": "General reply"}),
                    ("POST", "/repos/acme/widget/pulls/17/comments/99/replies", {"body": "Inline reply"}),
                ],
            )



if __name__ == "__main__":
    unittest.main()
