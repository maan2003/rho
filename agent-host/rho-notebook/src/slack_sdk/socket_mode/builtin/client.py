"""slack_sdk's default `SocketModeClient`, over the agent host's event buffer.

Replaces upstream's WebSocket client (slack_sdk 3.45.0). Slack sends each
event to only one Socket Mode connection, so the host holds the one
connection, acknowledges every envelope, and keeps the recent ones; this
client long-polls them with `rho.events` and runs the upstream listeners.
`send_socket_mode_response` therefore sends nothing.
"""

import json
import logging
import threading
from logging import Logger
from queue import Queue
from threading import Lock
from typing import Optional

from slack_sdk.socket_mode.client import BaseSocketModeClient
from slack_sdk.web import WebClient


class SocketModeClient(BaseSocketModeClient):
    def __init__(
        self,
        app_token: Optional[str] = None,
        logger: Optional[Logger] = None,
        web_client: Optional[WebClient] = None,
        auto_reconnect_enabled: bool = True,
        trace_enabled: bool = False,
        **_websocket_options,
    ):
        # The host holds the app token. One passed here is not used.
        self.app_token = None
        self.logger = logger or logging.getLogger(__name__)
        self.web_client = web_client or WebClient()
        self.auto_reconnect_enabled = auto_reconnect_enabled
        self.trace_enabled = trace_enabled
        self.wss_uri = None
        self.message_queue = Queue()
        self.message_listeners = []
        self.socket_mode_request_listeners = []
        self.closed = False
        self.connect_operation_lock = Lock()
        self._stop: Optional[threading.Event] = None
        self._thread: Optional[threading.Thread] = None
        self._cursor: Optional[str] = None

    def is_connected(self) -> bool:
        return self._thread is not None and self._thread.is_alive() and not self._stop.is_set()

    def connect(self) -> None:
        if self.is_connected():
            return
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._poll, args=(self._stop,), daemon=True)
        self._thread.start()

    def disconnect(self) -> None:
        if self._stop is not None:
            self._stop.set()

    def connect_to_new_endpoint(self, force: bool = False):
        self.connect()

    def send_message(self, message: str) -> None:
        pass

    def _poll(self, stop: threading.Event) -> None:
        while not stop.is_set() and not self.closed:
            try:
                reply = self.web_client.api_call(
                    "rho.events", http_verb="GET", params={"cursor": self._cursor, "timeout": 20}
                )
            except Exception as e:
                self.logger.warning(f"Failed to poll rho.events: {e}")
                stop.wait(5)
                continue
            if reply["truncated"]:
                self.logger.warning("Missed Socket Mode envelopes: read the conversations again")
            self._cursor = reply["cursor"]
            for envelope in reply["events"]:
                if stop.is_set():
                    return
                self.run_message_listeners(envelope, json.dumps(envelope))
