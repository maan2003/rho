"""Exercise rho_notion against a Notion-server-shaped Unix socket."""
import asyncio
import json
import os
import tempfile
import unittest
from unittest.mock import patch

import rho_notion

PAGE = "00000000-0000-0000-0000-000000000001"


def comments(*comments):
    """A `notion-get-comments` result: one inline discussion holding these
    `(id, text)` comments."""
    body = "".join(
        f'<comment id="{id}" url="u" user-url="user://1/a@b.c" datetime="2026-10-06T00:00:0{i}Z">{text}</comment>'
        for i, (id, text) in enumerate(comments)
    )
    discussion = (f'<discussions total-count="1" shown-count="1">\n<discussion id="discussion://{PAGE}/b/d" '
                  f'comment-count="{len(comments)}" resolved="false" type="comment" context="inline" '
                  f'text-context="The &quot;plan&quot;">\n{body}\n</discussion>\n</discussions>')
    return {"content": [{"type": "text", "text": json.dumps({"text": discussion})}]}


class FakeNotionServer:
    """Answers each request with the next of `replies`: `(status, body)`."""

    def __init__(self, directory, replies):
        self.path, self.replies, self.requests = directory + "/notion.sock", replies, []

    async def serve(self, reader, writer):
        method, target, _ = (await reader.readline()).decode().split()
        headers = {}
        while line := (await reader.readline()).decode().strip():
            key, value = line.split(":", 1)
            headers[key.lower()] = value.strip()
        body = await reader.readexactly(int(headers.get("content-length", 0)))
        self.requests.append((method, target, json.loads(body or b"null")))
        status, reply = self.replies.pop(0) if len(self.replies) > 1 else self.replies[0]
        payload = json.dumps(reply).encode()
        writer.write(f"HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n"
                     f"Content-Length: {len(payload)}\r\nConnection: close\r\n\r\n".encode() + payload)
        await writer.drain()
        writer.close()


class RhoNotionTest(unittest.IsolatedAsyncioTestCase):
    async def run_with(self, replies, body):
        with tempfile.TemporaryDirectory() as directory:
            fake = FakeNotionServer(directory, replies)
            server = await asyncio.start_unix_server(fake.serve, fake.path)
            with patch.dict(os.environ, {"RHO_SOCKET_PATH": directory + "/rho.sock"}):
                async with server:
                    await body()
            return fake.requests

    async def test_call_sends_the_arguments_and_raises_on_refusals_and_tool_errors(self):
        async def body():
            self.assertEqual(await rho_notion.call("notion-fetch", id=PAGE), {"content": []})
            with self.assertRaisesRegex(rho_notion.NotionError, "rho_page_outside_root"):
                await rho_notion.call("notion-fetch", id="x")
            with self.assertRaisesRegex(rho_notion.NotionError, "no such page"):
                await rho_notion.call("notion-fetch", id="y")

        requests = await self.run_with([
            (200, {"content": []}),
            (403, {"error": "rho_page_outside_root: ..."}),
            (200, {"isError": True, "content": [{"type": "text", "text": "no such page"}]}),
        ], body)
        self.assertEqual(requests[0], ("POST", "/tools/notion-fetch", {"id": PAGE}))

    async def test_watch_reports_each_new_comment_once_and_stops_on_a_refusal(self):
        got = []
        done = asyncio.Event()

        async def on_event(event):
            got.append(event)
            if event["type"] == "rho_error":
                done.set()

        async def body():
            task = rho_notion.watch(PAGE, on_event, interval=0)
            await asyncio.wait_for(done.wait(), 5)
            await asyncio.sleep(0.05)
            self.assertTrue(task.done())

        old = ("c1", "existing")
        new = ("c2", "&lt;b&gt;LGTM&lt;/b&gt; &amp; thanks")
        requests = await self.run_with([
            (200, comments(old)),
            (200, comments(old, new)),
            (200, comments(old, new)),
            (503, {"error": "rho_no_notion_grant: run `rho notion init`"}),
        ], body)

        self.assertEqual(got, [
            {"type": "comment", "page_id": PAGE, "discussion_id": f"discussion://{PAGE}/b/d", "comment_id": "c2",
             "user": "user://1/a@b.c", "datetime": "2026-10-06T00:00:01Z", "context": 'The "plan"',
             "text": "<b>LGTM</b> & thanks"},
            {"type": "rho_error", "error": "rho_no_notion_grant: run `rho notion init`"},
        ])
        self.assertEqual(len(requests), 4)
        self.assertEqual(requests[0], ("POST", "/tools/notion-get-comments", {"page_id": PAGE, "include_all_blocks": True}))


if __name__ == "__main__":
    unittest.main()
