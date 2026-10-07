"""ghapi 2.1.5 client and CI helpers, adapted for Octo transport.

Copyright (c) the ghapi contributors. Apache-2.0; see LICENSE.
Upstream: https://github.com/AnswerDotAI/ghapi (81b28a5325b311e9878a676a57fef801093242f6).
"""
from collections.abc import Mapping
from contextvars import ContextVar
from datetime import datetime
import os
import stat
from pathlib import Path
from urllib.parse import quote

import httpx2
from fastcore.all import *
from fastspec.spec import SpecParser
from fastspec.oapi import OpenAPIClient, OpFunc
from fasttransport.core import AsyncTransport
from fasttransport.errors import APIError
from .gh_spec import spec

GH_HOST = "http://octo"
_docroot = "https://docs.github.com/rest/reference/"
pspec = SpecParser.from_dict(spec)

def print_summary(method, url, kwargs):
    "Debug callback for `GhApi(debug=...)`: print each request with the token (if any) removed"
    hdrs = {k:v for k,v in (kwargs.get('headers') or {}).items() if k.lower()!='authorization'}
    print(method, url, {**kwargs, 'headers':hdrs})

# %% ../nbs/00_core.ipynb #d4a51c1a
_binary_cts = ('octet-stream', 'zip', 'gzip', 'tar', 'image/', 'audio/', 'video/')


class GhTransport(AsyncTransport):
    "Async transport converting JSON responses to `AttrDict`s and tracking rate-limit and response headers."
    def __init__(self, debug=None, limit_cb=None, **kwargs):
        super().__init__(**kwargs)
        self.debug,self.limit_cb,self.limit_rem = debug,limit_cb,5000
        self.recv_hdrs = {}

    @staticmethod
    def _decode(resp):
        "Like `AsyncTransport._decode`, but GitHub media types (e.g. diffs, shas) decode to `str` unless truly binary"
        res = AsyncTransport._decode(resp)
        ct = resp.headers.get('content-type') or ''
        if isinstance(res, bytes) and not any(t in ct for t in _binary_cts): res = res.decode()
        return res

    def _pre(self, method, url, kwargs):
        debug = self.debug or (print_summary if os.getenv('GHAPI_DEBUG') else None)
        if debug: debug(method, url, kwargs)

    def _post(self, resp, raw):
        self.recv_hdrs = resp.headers
        if 'X-RateLimit-Remaining' in resp.headers:
            newlim = resp.headers['X-RateLimit-Remaining']
            if self.limit_cb is not None and newlim != self.limit_rem: self.limit_cb(int(newlim), int(resp.headers['X-RateLimit-Limit']))
            self.limit_rem = newlim
        if raw: return resp
        res = self._decode(resp)
        return dict2obj(res) if isinstance(res, (dict,list)) else res

    async def request(self, method, url, *, raw=False, **kwargs):
        self._pre(method, url, kwargs)
        return self._post(await super().request(method, url, raw=True, **kwargs), raw)


# %% ../nbs/00_core.ipynb #cecd0177
_gh_override = ContextVar('_gh_override', default={})

class _LiveDefaults(dict):
    "Endpoint defaults, superseded at call time by any active `gh_patch` override"
    def items(self): return {**self, **_gh_override.get()}.items()
    def __contains__(self, k): return dict.__contains__(self, k) or k in _gh_override.get()
    def get(self, k, default=None): return _gh_override.get().get(k, dict.get(self, k, default))

def gh_patch(fn):
    "`patch` `fn` into `GhApi`, adding `owner`/`repo` params that override the client defaults for this call and its internal calls"
    async def _f(self, *args, owner=UNSET, repo=UNSET, **kwargs):
        over = {k:v for k,v in zip(('owner','repo'),(owner,repo)) if v is not UNSET}
        if not over: return await fn(self, *args, **kwargs)
        token = _gh_override.set({**_gh_override.get(), **over})
        try: return await fn(self, *args, **kwargs)
        finally: _gh_override.reset(token)
    return patch(splice_sig(_f, fn, 'self'))

# %% ../nbs/00_core.ipynb #83e8a9ce
class GhOpFunc(OpFunc):
    "Generated operation with strict schema keywords and query/body mapping fields."
    def _split(self, kwargs):
        for p in self.required_params:
            if p in self.op_spec.param_defaults:
                kwargs.setdefault(self.sparams[p], self.op_spec.param_defaults[p])
        parts = super()._split(kwargs)
        _, _, route, query, body, files = parts
        values = {**route, **query, **(body or {}), **files}
        missing = [p for p in self.required_params
                   if values.get(p, UNSET) is UNSET or p in self.route_params and values[p] is None]
        if missing:
            raise TypeError(f"{self.name}: missing required parameter(s): {', '.join(missing)}")
        return parts

    def _prep(self, args, kwargs):
        controls = {'headers_', 'query_', 'body_', 'raw_', 'stream'}
        if self.media_url: controls.update(('media', 'media_type'))
        unknown = kwargs.keys() - (self.sparams.keys() | set(self.sparams.values()) | controls)
        if unknown:
            raise TypeError(f"{self.name}: unexpected keyword argument(s): {', '.join(sorted(unknown))}")
        for option, fields in (('query_', self.query_params), ('body_', self.body_params)):
            extra = kwargs.get(option, {})
            if not isinstance(extra, Mapping):
                raise TypeError(f"{self.name}: {option} must be a mapping")
            unknown = extra.keys() - set(fields)
            if unknown:
                raise TypeError(f"{self.name}: unsupported {option} field(s): {', '.join(sorted(unknown))}")
        return super()._prep(args, kwargs)


class GhApi(OpenAPIClient):
    "Octo-backed client generated from the selected pinned ghapi REST metadata."
    def __init__(self, owner=None, repo=None, *, debug=None, limit_cb=None,
                 timeout=60.0):
        kwargs = {}
        if owner: kwargs['owner'] = owner
        if repo: kwargs['repo'] = repo
        self.headers = {'Accept': 'application/vnd.github+json'}
        self.token, self.gh_host = None, GH_HOST
        socket = Path(os.environ['RHO_SOCKET_PATH']).with_name('octo.sock')
        transport = httpx2.AsyncHTTPTransport(uds=str(socket))
        client = httpx2.AsyncClient(transport=transport, follow_redirects=False, timeout=timeout)
        self.transport = GhTransport(debug=debug, limit_cb=limit_cb, timeout=timeout,
                                     base_headers=self.headers, client=client)
        self.ops = [GhOpFunc(o, self.transport, self.gh_host, defaults=_LiveDefaults(kwargs)) for o in pspec.ops]
        self.func_dict = {f'{o.path}:{o.verb.upper()}': o for o in self.ops}
        self.groups = mk_groups(self.ops)
        for k, v in self.groups.items(): setattr(self, k, v)

    @property
    def debug(self): return self.transport.debug
    @debug.setter
    def debug(self, v): self.transport.debug = v
    @property
    def limit_rem(self): return self.transport.limit_rem
    @property
    def recv_hdrs(self): return self.transport.recv_hdrs

    def _repr_markdown_(self): return "\n".join(f"- [{o}]({_docroot + o.replace('_', '-')})" for o in sorted(self.groups))



class GhRows(L):
    "Result rows whose bare display is one actionable line each"
    def __repr__(self): return '\n'.join(map(repr, self))

# %% ../nbs/00_core.ipynb #16068542

def _dur(r):
    if not (r.get('started_at') and r.get('completed_at')): return ''
    t = lambda s: datetime.fromisoformat(s.replace('Z', '+00:00'))
    secs = int((t(r.completed_at) - t(r.started_at)).total_seconds())
    return f' ({secs//60}m{secs%60:02d}s)' if secs >= 60 else f' ({secs}s)'

class CheckRun(AttrDict):
    def __repr__(self): return f'{self.id}  {self.name}: {self.conclusion or self.status}{_dur(self)}'

class CommitStatus(AttrDict):
    def __repr__(self): return f'{self.context}: {self.state}'

# %% ../nbs/00_core.ipynb #6a9289a9
class _CheckStatus(AttrDict):
    def _repr_markdown_(self):
        runs = self.check_runs
        if not runs: return 'no check runs'
        if any(r.status!='completed' for r in runs): verdict = 'pending'
        elif all(r.conclusion in ('success','neutral','skipped') for r in runs): verdict = 'success'
        else: verdict = 'failure'
        return '\n'.join([f'**{verdict}**', ''] + [f'- {r!r}' for r in runs+self.statuses])
    __repr__ = _repr_markdown_

@gh_patch
async def check_status(self:GhApi, ref:str):
    """Combined commit status and check-run results for a commit SHA.

    Accepts a commit SHA, branch name or tag name as `ref`. Display the result bare for the check-run verdict and all run/status rows. `.state` comes only from legacy commit statuses and can be pending even when every Actions check passed. `.check_runs` and `.statuses` retain the structured rows.
    """
    combined = await self.repos.get_combined_status_for_ref(ref)
    checks = await self.checks.list_for_ref(ref, per_page=100, page=1)
    runs = list(checks.check_runs)
    page = 1
    while len(runs) < checks.total_count:
        page += 1
        more = await self.checks.list_for_ref(ref, per_page=100, page=page)
        if not more.check_runs: raise RuntimeError("GitHub returned an incomplete check-run list")
        runs.extend(more.check_runs)
    return _CheckStatus(dict(state=combined.state, statuses=GhRows(CommitStatus(o) for o in combined.statuses),
        check_runs=GhRows(CheckRun(o) for o in runs)))

@gh_patch
async def pr_status(self:GhApi, pull_number:int):
    "Combined status and check-run results for a PR's head commit"
    return await self.check_status((await self.pulls.get(pull_number)).head.sha)


@patch
def __call__(self:GhApi, path:str, verb:str=None, headers:dict=None, route:dict=None, query:dict=None, data=None):
    """Call an Octo-approved GitHub REST path directly; Octo still validates it."""
    if not path.startswith('/') or path.startswith('//'):
        raise ValueError("GhApi direct calls need a GitHub API path, not a URL")
    if verb is None: verb = 'POST' if data else 'GET'
    if route: path = path.format(**{k:quote(str(v), safe='') for k,v in route.items()})
    kw = dict(content=data) if isinstance(data, (bytes,str)) else dict(json=data)
    return self.transport.request(verb, self.gh_host + path, headers=headers, params=query, **kw)

@patch
def __getitem__(self:GhApi, k):
    a,b = k if isinstance(k,tuple) else (k,'GET')
    return self.func_dict[f'{a}:{b.upper()}']


_attachment_types = {
    ".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg",
    ".gif": "image/gif", ".webp": "image/webp", ".svg": "image/svg+xml",
    ".mp4": "video/mp4", ".mov": "video/quicktime", ".webm": "video/webm",
}

@patch
async def upload_attachment(self:GhApi, path, *, owner=UNSET, repo=UNSET):
    """Upload one local image/video through Octo and return its asset response (`.url`).

    Uses this client's repository defaults or explicit owner/repo overrides.
    Does not post a comment or edit a body. Requires repository write access.
    """
    path = Path(path)
    content_type = _attachment_types.get(path.suffix.lower())
    if content_type is None:
        raise ValueError(f"Unsupported attachment type: {path.suffix}")
    info = path.stat()
    if not stat.S_ISREG(info.st_mode):
        raise ValueError("Attachments must be regular files")
    limit = (100 if content_type.startswith("video/") else 10) * 1024 * 1024
    if not 0 < info.st_size <= limit:
        raise ValueError(f"Attachment must be nonempty and at most {limit} bytes")
    body = path.read_bytes()

    repository = await self.repos.get(owner=owner, repo=repo)
    return await self("/user-attachments/assets", verb="POST",
                      query={"name": path.name, "content_type": content_type,
                             "repository_id": repository.id},
                      headers={"Content-Type": "application/octet-stream"}, data=body)
