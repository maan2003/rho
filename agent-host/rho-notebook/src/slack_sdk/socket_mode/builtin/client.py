"""slack_sdk's default, threaded `SocketModeClient`, refused in rho.

Its threads would run listeners beside the notebook's event loop, which
every task of the agent shares. Use the async client in
`slack_sdk.socket_mode.aiohttp`, which rho runs over the agent host's event
buffer.
"""

from slack_sdk.socket_mode.client import BaseSocketModeClient


class SocketModeClient(BaseSocketModeClient):
    def __init__(self, *args, **kwargs):
        raise TypeError(
            "rho supports only the async Socket Mode client: use slack_sdk.socket_mode.aiohttp.SocketModeClient"
        )
