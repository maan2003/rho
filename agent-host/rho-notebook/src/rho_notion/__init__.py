"""Notion for agents: the tools of Notion's hosted MCP server.

Not an upstream package. Calls go to the agent host's Notion server, which
signs in to Notion as the user and forwards a fixed list of tools; see
`agent-host/notion-server`. Only async: a blocking call would hold the
notebook's event loop.
"""

import os
from pathlib import Path
from typing import Any, Dict, List

import httpx


class NotionError(Exception):
    """The host refused the call, or the tool reported an error."""


class Notion:
    """`await notion.call("notion-fetch", id=page_id)` returns the tool's MCP
    result: `{"content": [{"type": "text", "text": ...}], ...}`. `text()`
    joins its text. `await notion.tools()` lists the tools agents may call,
    with their descriptions and argument schemas."""

    def __init__(self, timeout: float = 120):
        socket = str(Path(os.environ["RHO_SOCKET_PATH"]).with_name("notion.sock"))
        self._http = httpx.AsyncClient(
            transport=httpx.AsyncHTTPTransport(uds=socket), base_url="http://notion", timeout=timeout
        )

    async def tools(self) -> List[Dict[str, Any]]:
        return (await self._request("GET", "/tools"))["tools"]

    async def call(self, tool: str, **arguments: Any) -> Dict[str, Any]:
        result = await self._request("POST", f"/tools/{tool}", json=arguments)
        if result.get("isError"):
            raise NotionError(text(result))
        return result

    async def _request(self, method: str, path: str, **kwargs: Any) -> Dict[str, Any]:
        response = await self._http.request(method, path, **kwargs)
        body = response.json()
        if response.is_error:
            raise NotionError(body.get("error", response.text))
        return body


def text(result: Dict[str, Any]) -> str:
    """The text parts of a tool result, joined."""
    return "\n".join(part["text"] for part in result.get("content", []) if part.get("type") == "text")
