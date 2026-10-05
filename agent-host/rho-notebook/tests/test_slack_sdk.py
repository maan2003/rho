"""Exercise the adapted slack_sdk clients against a Slack-server-shaped Unix socket."""
import asyncio
import json
import os
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

from slack_sdk import WebClient
from slack_sdk.errors import SlackApiError
from slack_sdk.rho import ThreadSubscriptions
from slack_sdk.socket_mode import SocketModeClient
from slack_sdk.socket_mode.aiohttp import SocketModeClient as AsyncSocketModeClient
from slack_sdk.socket_mode.response import SocketModeResponse
from slack_sdk.web.async_client import AsyncWebClient

ENVELOPE = {"envelope_id": "e1", "type": "events_api",
            "payload": {"event": {"type": "message", "channel": "D1", "text": "LGTM"}}}
EVENTS = [
    {"ok": True, "events": [ENVELOPE], "cursor": "7:1", "truncated": False},
    {"ok": True, "events": [], "cursor": "7:1", "truncated": False},
]


class FakeSlackServer:
    """Answers each request from `replies`, keyed by method, and records it."""

    def __init__(self, directory, replies):
        self.path, self.replies, self.requests = directory + "/slack.sock", replies, []

    async def serve(self, reader, writer):
        method, target, _ = (await reader.readline()).decode().split()
        headers = {}
        while line := (await reader.readline()).decode().strip():
            key, value = line.split(":", 1)
            headers[key.lower()] = value.strip()
        body = await reader.readexactly(int(headers.get("content-length", 0)))
        url = urlsplit(target)
        api = url.path.removeprefix("/api/")
        self.requests.append(dict(method=method, api=api, query=parse_qs(url.query), headers=headers, body=body))
        reply = self.replies[api]
        if isinstance(reply, list):
            reply = reply.pop(0) if len(reply) > 1 else reply[0]
        payload = json.dumps(reply).encode()
        writer.write(f"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                     f"Content-Length: {len(payload)}\r\nConnection: close\r\n\r\n".encode() + payload)
        await writer.drain()
        writer.close()


class SlackSdkTest(unittest.IsolatedAsyncioTestCase):
    async def run_with(self, replies, body):
        with tempfile.TemporaryDirectory() as directory:
            fake = self.fake = FakeSlackServer(directory, replies)
            server = await asyncio.start_unix_server(fake.serve, fake.path)
            with patch.dict(os.environ, {"RHO_SOCKET_PATH": directory + "/rho.sock"}):
                async with server:
                    await body()
            return fake.requests

    async def test_async_client_sends_json_form_and_query_without_a_token(self):
        async def body():
            client = AsyncWebClient(token="xoxb-agent-guess")
            posted = await client.chat_postMessage(channel="D1", text="Review #12?", unfurl_links=False,
                                                   blocks=[{"type": "section", "text": {"type": "mrkdwn", "text": "hi"}}])
            self.assertEqual(posted["ts"], "1.5")
            await client.conversations_open(users=["U1", "U2"])
            await client.conversations_replies(channel="D1", ts="1.5", inclusive=True)

        requests = await self.run_with({
            "chat.postMessage": {"ok": True, "ts": "1.5"},
            "conversations.open": {"ok": True, "channel": {"id": "D1"}},
            "conversations.replies": {"ok": True, "messages": []},
        }, body)

        post, open_, replies = requests
        self.assertEqual((post["method"], post["api"]), ("POST", "chat.postMessage"))
        self.assertTrue(post["headers"]["content-type"].startswith("application/json"))
        self.assertEqual(json.loads(post["body"]), {
            "channel": "D1", "text": "Review #12?", "unfurl_links": False,
            "blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": "hi"}}]})
        self.assertNotIn("authorization", post["headers"])
        self.assertEqual(open_["query"], {"users": ["U1,U2"]})
        self.assertEqual((replies["method"], replies["query"]),
                         ("GET", {"channel": ["D1"], "ts": ["1.5"], "inclusive": ["1"]}))

    async def test_async_pagination_follows_the_cursor(self):
        texts = []

        async def body():
            async for page in await AsyncWebClient().conversations_history(channel="C1", limit=1):
                texts.extend(m["text"] for m in page["messages"])

        requests = await self.run_with({"conversations.history": [
            {"ok": True, "messages": [{"text": "a"}], "response_metadata": {"next_cursor": "c2"}},
            {"ok": True, "messages": [{"text": "b"}], "response_metadata": {"next_cursor": ""}},
        ]}, body)

        self.assertEqual(texts, ["a", "b"])
        self.assertEqual([r["query"].get("cursor") for r in requests], [None, ["c2"]])

    async def test_api_errors_raise_and_sync_clients_refuse(self):
        async def body():
            with self.assertRaises(SlackApiError) as raised:
                await AsyncWebClient().auth_revoke()
            self.assertEqual(raised.exception.response["error"], "rho_method_refused")
            for sync in (WebClient, SocketModeClient):
                with self.assertRaisesRegex(TypeError, "only the async"):
                    sync(token="xoxb-agent-guess")

        await self.run_with({"auth.revoke": {"ok": False, "error": "rho_method_refused"}}, body)

    async def test_files_go_multipart_with_their_fields(self):
        async def body():
            await AsyncWebClient().api_call("files.upload", files={"file": b"log line"},
                                            data={"channels": "C1", "filename": "ci.log"})

        (request,) = await self.run_with({"files.upload": {"ok": True}}, body)

        self.assertTrue(request["headers"]["content-type"].startswith("multipart/form-data; boundary="))
        self.assertIn(b'name="channels"\r\n\r\nC1', request["body"])
        self.assertIn(b"log line", request["body"])

    async def test_async_socket_mode_client_runs_listeners_from_rho_events(self):
        async def body():
            client = AsyncSocketModeClient(app_token="xapp-agent-guess")
            seen = asyncio.Event()

            async def listener(client, request):
                self.assertEqual((request.type, request.envelope_id), ("events_api", "e1"))
                self.assertEqual(request.payload["event"]["text"], "LGTM")
                await client.send_socket_mode_response(SocketModeResponse(envelope_id=request.envelope_id))
                seen.set()

            client.socket_mode_request_listeners.append(listener)
            await client.connect()
            await asyncio.wait_for(seen.wait(), 5)
            while len(self.fake.requests) < 2:
                await asyncio.sleep(0.01)
            await client.close()
            self.assertFalse(await client.is_connected())

        requests = await self.run_with({"rho.events": list(EVENTS)}, body)

        self.assertEqual(requests[0]["query"], {"timeout": ["20"]})
        self.assertEqual(requests[1]["query"], {"cursor": ["7:1"], "timeout": ["20"]})

    async def test_thread_subscriptions_route_events_to_their_threads(self):
        def envelope(event):
            return {"envelope_id": "e", "type": "events_api", "payload": {"event": event}}

        reply_a = {"type": "message", "channel": "D1", "ts": "2.0", "thread_ts": "1.0", "text": "LGTM"}
        root_a = {"type": "message", "channel": "D1", "ts": "1.0"}
        same_ts_elsewhere = {"type": "message", "channel": "C9", "ts": "3.0", "thread_ts": "1.0"}
        edit_b = {"type": "message", "subtype": "message_changed", "channel": "C2",
                  "message": {"ts": "6.0", "thread_ts": "5.0", "text": "edited"}}
        reaction_a = {"type": "reaction_added", "item": {"channel": "D1", "ts": "1.0"}}
        polls = [
            {"ok": True, "events": [envelope(e) for e in [reply_a, root_a, same_ts_elsewhere, edit_b, reaction_a]],
             "cursor": "7:5", "truncated": False},
            {"ok": True, "events": [], "cursor": "8:0", "truncated": True},
            {"ok": True, "events": [], "cursor": "8:0", "truncated": False},
        ]
        a, b = [], []

        async def body():
            threads = ThreadSubscriptions()

            def on_a(event):
                a.append(event)
                raise RuntimeError("a failing callback does not stop the others")

            async def on_b(event):
                b.append(event)

            threads.subscribe("D1", "1.0", on_a)
            threads.subscribe("C2", "5.0", on_b)
            while len(self.fake.requests) < 3:
                await asyncio.sleep(0.01)
            threads.unsubscribe("D1", "1.0")
            threads.unsubscribe("C2", "5.0")
            self.assertIsNone(threads._poller)

        requests = await self.run_with({"rho.events": polls}, body)

        truncated = {"type": "rho_truncated"}
        self.assertEqual(a, [reply_a, root_a, reaction_a, truncated])
        self.assertEqual(b, [edit_b, truncated])
        self.assertEqual([r["query"].get("cursor") for r in requests[:3]], [None, ["7:5"], ["8:0"]])


if __name__ == "__main__":
    unittest.main()
