"""The slack_sdk 3.45.0 `AsyncBaseClient`, adapted for the agent host's Slack server.

Copyright (c) the slack_sdk contributors. MIT License.
Upstream: https://github.com/slackapi/python-slack-sdk (dd615799e83ff20cd6b6acd2909ea19604679ef1).

It sends requests with the transport in `base_client.py`, on httpx instead
of aiohttp.
"""

import logging
from ssl import SSLContext
from typing import Any, Dict, Optional

import httpx

from .async_slack_response import AsyncSlackResponse
from .base_client import _request_kwargs, _response, _socket
from .deprecation import show_deprecation_warning_if_any
from .file_upload_v2_result import FileUploadV2Result
from .internal_utils import _build_req_args, _get_url, get_user_agent


class AsyncBaseClient:
    BASE_URL = "http://slack/api/"

    def __init__(
        self,
        token: Optional[str] = None,
        base_url: str = BASE_URL,
        timeout: int = 30,
        ssl: Optional[SSLContext] = None,
        proxy: Optional[str] = None,
        session: Any = None,
        trust_env_in_session: bool = False,
        headers: Optional[dict] = None,
        user_agent_prefix: Optional[str] = None,
        user_agent_suffix: Optional[str] = None,
        # for Org-Wide App installation
        team_id: Optional[str] = None,
        logger: Optional[logging.Logger] = None,
        retry_handlers: Optional[list] = None,
    ):
        # The host holds the token. One passed here is not sent.
        self.token = None
        if not base_url.endswith("/"):
            base_url += "/"
        self.base_url = base_url
        self.timeout = timeout
        self.ssl = ssl
        self.proxy = proxy
        self.session = None
        self.trust_env_in_session = trust_env_in_session
        self.headers = headers or {}
        self.headers["User-Agent"] = get_user_agent(user_agent_prefix, user_agent_suffix)
        self.default_params = {}
        if team_id is not None:
            self.default_params["team_id"] = team_id
        self._logger = logger if logger is not None else logging.getLogger(__name__)
        self.retry_handlers = retry_handlers or []

    @property
    def logger(self) -> logging.Logger:
        """The logger this client uses."""
        return self._logger

    async def api_call(
        self,
        api_method: str,
        *,
        http_verb: str = "POST",
        files: Optional[dict] = None,
        data: Optional[dict] = None,
        params: Optional[dict] = None,
        json: Optional[dict] = None,
        headers: Optional[dict] = None,
        auth: Optional[dict] = None,
    ) -> AsyncSlackResponse:
        """Calls a Slack Web API method, e.g. `'chat.postMessage'`, through the host."""
        api_url = _get_url(self.base_url, api_method)
        headers = headers or {}
        headers.update(self.headers)
        req_args = _build_req_args(
            token=None,
            http_verb=http_verb,
            files=files,  # type: ignore[arg-type]
            data=data,  # type: ignore[arg-type]
            default_params=self.default_params,
            params=params,  # type: ignore[arg-type]
            json=json,  # type: ignore[arg-type]
            headers=headers,
            auth=None,  # type: ignore[arg-type]
            ssl=None,
            proxy=None,
        )
        show_deprecation_warning_if_any(api_method)
        response = await self._request(http_verb=http_verb, api_url=api_url, req_args=req_args)
        return AsyncSlackResponse(
            client=self, http_verb=http_verb, api_url=api_url, req_args=req_args, **response
        ).validate()

    async def _request(self, *, http_verb, api_url, req_args) -> Dict[str, Any]:
        """Sends one request. `AsyncSlackResponse` pagination calls this for each page."""
        opened, kwargs = _request_kwargs(http_verb, api_url, req_args)
        try:
            transport = httpx.AsyncHTTPTransport(uds=_socket())
            async with httpx.AsyncClient(transport=transport, timeout=self.timeout) as client:
                return _response(await client.request(**kwargs))
        finally:
            for f in opened:
                f.close()

    async def _upload_file(
        self,
        *,
        url: str,
        data: bytes,
        logger: logging.Logger,
        timeout: int,
        proxy: Optional[str],
        ssl: Optional[SSLContext],
    ) -> FileUploadV2Result:
        """Uploads to the URL that `files.getUploadURLExternal` issued. The URL
        itself authorizes the upload, so it goes straight to Slack."""
        async with httpx.AsyncClient(timeout=timeout) as client:
            resp = await client.post(url, content=data)
        return FileUploadV2Result(status=resp.status_code, body=resp.text)
