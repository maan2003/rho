"""Notion for agents: the tools of Notion's hosted MCP server.

Not an upstream package. Calls go to the agent host's Notion server, which
signs in to Notion as the user and forwards a fixed list of tools; see
`agent-host/notion-server`. Only async: a blocking call would hold the
notebook's event loop.

`await call("notion-fetch", id=page_id)` returns the tool's MCP result:
`{"content": [{"type": "text", "text": ...}], ...}`, and `text()` joins its
text. `await tools()` lists the tools agents may call, with their
descriptions and argument schemas.
"""

import os
from pathlib import Path
from typing import Any, Dict, List

import httpx


class NotionError(Exception):
    """The host refused the call, or the tool reported an error."""


async def tools() -> List[Dict[str, Any]]:
    return (await _request("GET", "/tools"))["tools"]


async def call(tool: str, **arguments: Any) -> Dict[str, Any]:
    result = await _request("POST", f"/tools/{tool}", json=arguments)
    if result.get("isError"):
        raise NotionError(text(result))
    return result


def text(result: Dict[str, Any]) -> str:
    """The text parts of a tool result, joined."""
    return "\n".join(part["text"] for part in result.get("content", []) if part.get("type") == "text")


async def _request(method: str, path: str, **kwargs: Any) -> Dict[str, Any]:
    socket = str(Path(os.environ["RHO_SOCKET_PATH"]).with_name("notion.sock"))
    async with httpx.AsyncClient(
        transport=httpx.AsyncHTTPTransport(uds=socket), base_url="http://notion", timeout=120
    ) as client:
        response = await client.request(method, path, **kwargs)
    body = response.json()
    if response.is_error:
        raise NotionError(body.get("error", response.text))
    return body
