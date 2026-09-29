"""Exercise the selected upstream ghapi code against an Octo-shaped Unix socket."""
import asyncio
import json
import os
import tempfile
import unittest
from unittest.mock import patch

from ghapi.all import APIError, GhApi


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
                if method == "POST" and json.loads(body).get("draft") is False:
                    status = "403 Forbidden"
                    result = {"message": "only draft pull requests are available"}
                elif path.endswith("/pulls/17"):
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
                    result = {"number": 18, "draft": True}
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
                        with self.assertRaises(APIError) as denied:
                            await api.pulls.create(head="fix", base="main", title="Fix", draft=False)
                        self.assertEqual(denied.exception.status_code, 403)
                        self.assertIs(json.loads(requests[-1][3])["draft"], False)
                        with self.assertRaises(ValueError):
                            api("https://api.github.com/repos/acme/widget/issues")
                        with self.assertRaises(TypeError):
                            GhApi(token="not-a-host-credential")
            finally:
                server.close()
                await server.wait_closed()


if __name__ == "__main__":
    unittest.main()
