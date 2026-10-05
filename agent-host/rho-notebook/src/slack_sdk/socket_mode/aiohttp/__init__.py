"""slack_sdk's async `SocketModeClient`, over the agent host's event buffer.

Replaces upstream's aiohttp client (slack_sdk 3.45.0) the way
`slack_sdk.socket_mode.builtin` is replaced: the host holds the one Socket
Mode connection and acknowledges every envelope, and this client long-polls
the recent ones with `rho.events`.
"""

import asyncio
import json
import logging
from asyncio import Lock, Queue
from logging import Logger
from typing import Optional

from slack_sdk.socket_mode.async_client import AsyncBaseSocketModeClient
from slack_sdk.web.async_client import AsyncWebClient


class SocketModeClient(AsyncBaseSocketModeClient):
    def __init__(
        self,
        app_token: Optional[str] = None,
        logger: Optional[Logger] = None,
        web_client: Optional[AsyncWebClient] = None,
        auto_reconnect_enabled: bool = True,
        trace_enabled: bool = False,
        **_websocket_options,
    ):
        # The host holds the app token. One passed here is not used.
        self.app_token = None
        self.logger = logger or logging.getLogger(__name__)
        self.web_client = web_client or AsyncWebClient()
        self.auto_reconnect_enabled = auto_reconnect_enabled
        self.trace_enabled = trace_enabled
        self.wss_uri = None
        self.message_queue = Queue()
        self.message_listeners = []
        self.socket_mode_request_listeners = []
        self.closed = False
        self.connect_operation_lock = Lock()
        self._poller: Optional[asyncio.Task] = None
        self._cursor: Optional[str] = None

    async def is_connected(self) -> bool:
        return self._poller is not None and not self._poller.done()

    async def connect(self):
        if not await self.is_connected():
            self._poller = asyncio.ensure_future(self._poll())

    async def disconnect(self):
        if self._poller is not None:
            self._poller.cancel()
            self._poller = None

    async def connect_to_new_endpoint(self, force: bool = False):
        await self.connect()

    async def send_message(self, message: str):
        pass

    async def _poll(self) -> None:
        while not self.closed:
            try:
                reply = await self.web_client.api_call(
                    "rho.events", http_verb="GET", params={"cursor": self._cursor, "timeout": 20}
                )
            except Exception as e:
                self.logger.warning(f"Failed to poll rho.events: {e}")
                await asyncio.sleep(5)
                continue
            if reply["truncated"]:
                self.logger.warning("Missed Socket Mode envelopes: read the conversations again")
            self._cursor = reply["cursor"]
            for envelope in reply["events"]:
                await self.run_message_listeners(envelope, json.dumps(envelope))
