"""Exercise the pinned upstream ghapi code against an Octo-shaped Unix socket."""
import asyncio
import inspect
import hashlib
import json
import os
import tempfile
import unittest
from unittest.mock import AsyncMock, Mock, patch
from urllib.parse import parse_qs, urlsplit
from pathlib import Path

from ghapi.all import GhApi
from ghapi.gh_spec import spec


class OctoGhApiTest(unittest.IsolatedAsyncioTestCase):
    def test_selected_spec_matches_complete_pinned_upstream_entries(self):
        # Hash independently selected from upstream 81b28a5325b311e9878a676a57fef801093242f6:
        # baseline CI, all selected reads/PRs, and issue/comment create/update writes.
        custom = {("pulls", "review_decision"), ("pulls", "set_draft")}
        upstream = {**spec, "ops": sorted(
            [o for o in spec["ops"] if (o["group"], o["name"]) not in custom],
            key=lambda o: (o["group"], o["name"]),
        )}
        self.assertEqual(len(spec["ops"]), 78)
        self.assertEqual(len(upstream["ops"]), 76)
        writes = {(op["group"], op["name"]) for op in upstream["ops"] if op["verb"] != "GET"}
        self.assertEqual(len(writes), 19)
        self.assertEqual({name for group, name in writes if group == "issues"},
                         {"create", "update", "create_comment", "update_comment"})
        self.assertEqual(sum(op["verb"] == "GET" for op in upstream["ops"]), 57)
        canonical = json.dumps(upstream, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
        self.assertEqual(hashlib.sha256(canonical).hexdigest(), "bd646d0dca2eda7381da9e17d9baaf199dd2266e95ee33886500439a84d6c4b7")
        self.assertEqual({o["group"] for o in spec["ops"]},
                         {"actions", "checks", "issues", "pulls", "repos", "search"})
        draft = next(o for o in spec["ops"] if (o["group"], o["name"]) == ("pulls", "set_draft"))
        self.assertEqual((draft["verb"], draft["path"], draft["body_params"]),
                         ("PUT", "/repos/{owner}/{repo}/pulls/{pull_number}/draft", ["draft"]))
        self.assertIn("draft", draft["required_params"])

    async def test_required_parameters_are_checked_after_binding_and_mapping_overrides(self):
        from fastcore.all import UNSET
        from ghapi.core import _gh_override

        with patch.dict(os.environ, {"RHO_SOCKET_PATH": "/unused/rho.sock"}):
            for sync in (False, True):
                with self.subTest(sync=sync):
                    api = GhApi("acme", "widget", sync=sync)
                    no_owner = GhApi(repo="widget", sync=sync)
                    request = Mock(return_value={"number": 42}) if sync else AsyncMock(return_value={"number": 42})
                    api.transport.request = no_owner.transport.request = request

                    async def invoke(op, *args, **kwargs):
                        result = op(*args, **kwargs)
                        return result if sync else await result

                    for op, args, kwargs, missing in (
                        (no_owner.pulls.list, (), {}, "owner"),
                        (no_owner.pulls.list, (), {"owner": UNSET}, "owner"),
                        (api.pulls.get, (), {}, "pull_number"),
                        (api.pulls.get, (), {"pull_number": None}, "pull_number"),
                        (api.pulls.get, (17,), {"owner": None}, "owner"),
                        (api.search.repos, (), {}, "q"),
                        (api.pulls.set_draft, (17,), {}, "draft"),
                        (api.search.repos, (), {"query_": {"q": UNSET}}, "q"),
                        (api.issues.update_comment, (7,), {}, "body"),
                        (api.issues.update_comment, (7,), {"body": UNSET}, "body"),
                        (api.issues.update_comment, (7,), {"body": "direct", "body_": {"body": UNSET}}, "body"),
                    ):
                        with self.subTest(missing=missing, kwargs=kwargs):
                            with self.assertRaisesRegex(TypeError, missing):
                                await invoke(op, *args, **kwargs)
                    request.assert_not_called()

                    await invoke(api.pulls.get, 17, owner=UNSET)
                    self.assertEqual(request.call_args.args, ("GET", "http://octo/repos/acme/widget/pulls/17"))
                    await invoke(no_owner.pulls.list, owner="explicit")
                    self.assertEqual(request.call_args.args, ("GET", "http://octo/repos/explicit/widget/pulls"))
                    token = _gh_override.set({"owner": "live"})
                    try:
                        await invoke(no_owner.pulls.get, 23)
                        self.assertEqual(request.call_args.args, ("GET", "http://octo/repos/live/widget/pulls/23"))
                    finally:
                        _gh_override.reset(token)
                    await invoke(api.search.repos, "stars:>7")
                    self.assertEqual(request.call_args.kwargs["params"], {"q": "stars:>7"})
                    await invoke(api.search.repos, q="direct", query_={"q": "mapped"})
                    self.assertEqual(request.call_args.kwargs["params"], {"q": "mapped"})
                    await invoke(api.issues.update_comment, 7, "positional")
                    self.assertEqual(request.call_args.kwargs["json"], {"body": "positional"})
                    await invoke(api.issues.update_comment, 7, body_={"body": "mapped"})
                    self.assertEqual(request.call_args.kwargs["json"], {"body": "mapped"})
                    await invoke(api.pulls.create, head="topic", base="main", issue=7, title=UNSET)
                    self.assertEqual(request.call_args.kwargs["json"],
                                     {"head": "topic", "base": "main", "issue": 7})
                    await invoke(api.pulls.update, 7, body=UNSET, maintainer_can_modify=False)
                    self.assertEqual(request.call_args.kwargs["json"], {"maintainer_can_modify": False})
                    await invoke(api.pulls.update, 7, body=None)
                    self.assertEqual(request.call_args.kwargs["json"], {"body": None})
                    await invoke(api.issues.update_comment, 7, body=None)
                    self.assertEqual(request.call_args.kwargs["json"], {"body": None})
                    await invoke(api.search.repos, query_={"q": None})
                    self.assertEqual(request.call_args.kwargs["params"], {"q": None})


    async def test_operation_keywords_and_mapping_fields_are_strict(self):
        with patch.dict(os.environ, {"RHO_SOCKET_PATH": "/unused/rho.sock"}):
            for sync in (False, True):
                with self.subTest(sync=sync):
                    api = GhApi("acme", "widget", sync=sync)
                    request = Mock(return_value={"number": 42}) if sync else AsyncMock(return_value={"number": 42})
                    api.transport.request = request

                    async def invoke(op, *args, **kwargs):
                        result = op(*args, **kwargs)
                        return result if sync else await result

                    for kwargs in (
                        {"sttae": "all"}, {"query": {"state": "all"}},
                        {"query_": {"sttae": "all"}}, {"body_": {"state": "closed"}},
                        {"media": b"not an upload"}, {"media_type": "text/plain"}, {"content_": b"data"},
                        {"query_": None}, {"body_": [("body", "reply")]},
                    ):
                        with self.subTest(kwargs=kwargs):
                            with self.assertRaises(TypeError):
                                await invoke(api.pulls.list, **kwargs)
                    for kwargs in (
                        {"unexpected": 1}, {"body_": {"boddy": "reply"}},
                        {"body_": {"body": "reply", "owner": "other"}},
                        {"query_": {"body": "reply"}},
                    ):
                        with self.subTest(kwargs=kwargs):
                            with self.assertRaises(TypeError):
                                await invoke(api.issues.update_comment, 7, **kwargs)
                    request.assert_not_called()
                    for name in ("add_assignees", "set_labels", "create_label", "delete_comment",
                                 "create_milestone", "add_sub_issue", "approve_suggestion"):
                        self.assertFalse(hasattr(api.issues, name), name)

                    self.assertEqual(inspect.signature(api.pulls.list).parameters["sort"].default, "created")
                    self.assertEqual(inspect.signature(api.search.repos).parameters["order"].default, "desc")
                    result = await invoke(api.pulls.list, owner="other", repo="repo", state="closed",
                                          query_={"page": 3}, headers_={"X-Test": "yes"}, raw_=True)
                    self.assertEqual(result.number, 42)
                    self.assertEqual(request.call_args.args, ("GET", "http://octo/repos/other/repo/pulls"))
                    self.assertEqual(request.call_args.kwargs, {
                        "headers": {"X-Test": "yes"}, "params": {"state": "closed", "page": 3},
                        "json": None, "raw": True,
                    })
                    await invoke(api.issues.update_comment, 7, body_={"body": "edited"})
                    self.assertEqual(request.call_args.args, ("PATCH", "http://octo/repos/acme/widget/issues/comments/7"))
                    self.assertEqual(request.call_args.kwargs["json"], {"body": "edited"})
                    await invoke(api.pulls.list)
                    self.assertEqual(request.call_args.kwargs["params"], {})
                    count = request.call_count
                    with self.assertRaises(TypeError):
                        await invoke(api.pulls.list, raw_=True, stream=True)
                    self.assertEqual(request.call_count, count)
                    if sync:
                        with self.assertRaises(TypeError):
                            await invoke(api.pulls.list, stream=True)
                    else:
                        async def events(*args, **kwargs):
                            yield {"id": 99}
                        api.transport.stream = Mock(side_effect=events)
                        result = await invoke(api.pulls.list, state="open", stream=True)
                        self.assertEqual([event.id async for event in result], [99])
                        self.assertEqual(api.transport.stream.call_args.args,
                                         ("GET", "http://octo/repos/acme/widget/pulls"))
                        self.assertEqual(api.transport.stream.call_args.kwargs["params"], {"state": "open"})
                    self.assertEqual(request.call_count, count)

    async def test_issue_pr_review_and_search_workflow_routes(self):
        with tempfile.TemporaryDirectory() as directory:
            requests = []
            headers_seen = []

            async def serve(reader, writer):
                method, path, _ = (await reader.readline()).decode().split()
                headers = {}
                while line := (await reader.readline()).decode().strip():
                    key, value = line.split(":", 1)
                    headers[key.lower()] = value.strip()
                body = await reader.readexactly(int(headers.get("content-length", 0)))
                requests.append((method, path, json.loads(body) if body else None))
                headers_seen.append(headers)
                content_type = "application/json"
                payload = b'{"id": 42, "items": [{"number": 7}]}'
                if headers.get("accept") == "application/vnd.github.diff":
                    content_type, payload = "application/vnd.github.diff", b"diff --git a/file b/file\n"
                writer.write(
                    f"HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n".encode()
                    + f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload
                )
                await writer.drain()
                writer.close()

            server = await asyncio.start_unix_server(serve, directory + "/octo.sock")
            try:
                with patch.dict(os.environ, {"RHO_SOCKET_PATH": directory + "/rho.sock"}):
                    async with server:
                        for sync in (False, True):
                            with self.subTest(sync=sync):
                                requests.clear()
                                api = GhApi("acme", "widget", sync=sync)

                                async def invoke(op, *args, **kwargs):
                                    if sync: return await asyncio.to_thread(op, *args, **kwargs)
                                    return await op(*args, **kwargs)

                                self.assertEqual((await invoke(api.issues.get_comment, 19)).id, 42)
                                await invoke(api.issues.update_comment, 19, body="edited")
                                await invoke(api.pulls.get_review_comment, 23)
                                await invoke(api.pulls.update_review_comment, 23, body="review edited")
                                await invoke(api.issues.create, title="New issue", body="Details",
                                             assignee="alice", milestone=4, labels=["bug", "ui"],
                                             assignees=["alice", "bob"], issue_field_values={"priority": "High"},
                                             type="Bug", parent_issue_id=31)
                                await invoke(api.issues.update, 7, title="Changed", body="New details",
                                             assignee="carol", state="closed", state_reason="duplicate",
                                             duplicate_issue_id=9, milestone=None, type=None,
                                             labels=[], assignees=["carol"], issue_field_values={"priority": "Low"})
                                await invoke(api.issues.update, 7, body_={"milestone": None, "type": None})
                                await invoke(api.issues.update, 7, body="No milestone or type change")
                                found = await invoke(api.search.issues_and_pull_requests,
                                                     'repo:acme/widget is:pr label:"needs review" foo+bar',
                                                     sort="updated", order="asc", per_page=7, page=3,
                                                     advanced_search=True, search_type="lexical")
                                self.assertEqual(found["items"][0].number, 7)
                                await invoke(api.search.repos, "language:Rust stars:>20",
                                             sort="stars", order="desc", per_page=11, page=2)
                                self.assertEqual(requests[:8], [
                                    ("GET", "/repos/acme/widget/issues/comments/19", None),
                                    ("PATCH", "/repos/acme/widget/issues/comments/19", {"body": "edited"}),
                                    ("GET", "/repos/acme/widget/pulls/comments/23", None),
                                    ("PATCH", "/repos/acme/widget/pulls/comments/23", {"body": "review edited"}),
                                    ("POST", "/repos/acme/widget/issues", {
                                        "title": "New issue", "body": "Details", "assignee": "alice",
                                        "milestone": 4, "labels": ["bug", "ui"], "assignees": ["alice", "bob"],
                                        "issue_field_values": {"priority": "High"}, "type": "Bug", "parent_issue_id": 31,
                                    }),
                                    ("PATCH", "/repos/acme/widget/issues/7", {
                                        "title": "Changed", "body": "New details", "assignee": "carol",
                                        "state": "closed", "state_reason": "duplicate", "duplicate_issue_id": 9,
                                        "milestone": None, "type": None, "labels": [], "assignees": ["carol"],
                                        "issue_field_values": {"priority": "Low"},
                                    }),
                                    ("PATCH", "/repos/acme/widget/issues/7", {"milestone": None, "type": None}),
                                    ("PATCH", "/repos/acme/widget/issues/7", {"body": "No milestone or type change"}),
                                ])
                                self.assertEqual(urlsplit(requests[8][1]).path, "/search/issues")
                                self.assertEqual(parse_qs(urlsplit(requests[8][1]).query), {
                                    "q": ['repo:acme/widget is:pr label:"needs review" foo+bar'],
                                    "sort": ["updated"], "order": ["asc"], "per_page": ["7"], "page": ["3"],
                                    "advanced_search": ["true"], "search_type": ["lexical"],
                                })
                                self.assertEqual(urlsplit(requests[9][1]).path, "/search/repositories")
                                self.assertEqual(parse_qs(urlsplit(requests[9][1]).query), {
                                    "q": ["language:Rust stars:>20"], "sort": ["stars"], "order": ["desc"],
                                    "per_page": ["11"], "page": ["2"],
                                })
                                self.assertEqual(requests[8][0::2], ("GET", None))
                                self.assertEqual(requests[9][0::2], ("GET", None))
                                await invoke(api.issues.update, 7, assignees=["alice", "bob"],
                                             labels=["bug", "UI"], milestone=41)
                                await invoke(api.pulls.request_reviewers, 17,
                                             reviewers=["alice"], team_reviewers=["maintainers"])
                                self.assertIn("comments", inspect.signature(api.pulls.create_review).parameters)
                                await invoke(api.pulls.create_review, 17, body="Reviewed", event="COMMENT",
                                             comments=[{"path": "src/lib.rs", "line": 5, "body": "Check this"}])
                                await invoke(api.pulls.update_review, 17, 29, body="Edited review")
                                await invoke(api.pulls.submit_review, 17, 29, event="APPROVE", body="Approved")
                                await invoke(api.pulls.set_draft, 17, draft=True)
                                await invoke(api.pulls.set_draft, 17, body_={"draft": False})
                                self.assertEqual(requests[10:], [
                                    ("PATCH", "/repos/acme/widget/issues/7", {
                                        "assignees": ["alice", "bob"], "labels": ["bug", "UI"], "milestone": 41,
                                    }),
                                    ("POST", "/repos/acme/widget/pulls/17/requested_reviewers", {
                                        "reviewers": ["alice"], "team_reviewers": ["maintainers"],
                                    }),
                                    ("POST", "/repos/acme/widget/pulls/17/reviews", {
                                        "body": "Reviewed", "event": "COMMENT",
                                        "comments": [{"path": "src/lib.rs", "line": 5, "body": "Check this"}],
                                    }),
                                    ("PUT", "/repos/acme/widget/pulls/17/reviews/29", {"body": "Edited review"}),
                                    ("POST", "/repos/acme/widget/pulls/17/reviews/29/events", {
                                        "event": "APPROVE", "body": "Approved",
                                    }),
                                    ("PUT", "/repos/acme/widget/pulls/17/draft", {"draft": True}),
                                    ("PUT", "/repos/acme/widget/pulls/17/draft", {"draft": False}),
                                ])
                                diff = await invoke(api.pulls.get, 17,
                                                    headers_={"Accept": "application/vnd.github.diff"})
                                self.assertEqual(diff, "diff --git a/file b/file\n")
                                raw = await invoke(api.pulls.get, 17, raw_=True,
                                                   headers_={"Accept": "application/vnd.github.diff"})
                                self.assertEqual(raw.status_code, 200)
                                self.assertEqual(raw.content, b"diff --git a/file b/file\n")
                                self.assertEqual(requests[-1], ("GET", "/repos/acme/widget/pulls/17", None))
                                self.assertTrue(all("authorization" not in h for h in headers_seen))
                                if sync: api.transport.close()
                                else: await api.transport.aclose()
            finally:
                server.close()
                await server.wait_closed()

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
                if path.endswith("/pulls/17"):
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
                    result = {"number": 18, "draft": json.loads(body).get("draft", False)} if method == "POST" else {"number": 18}
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
                        normal = await api.pulls.create(head="fix", base="main", title="Fix", draft=False)
                        self.assertIs(normal.draft, False)
                        self.assertIs(json.loads(requests[-1][3])["draft"], False)
                        implicit = await api.pulls.create(head="fix", base="main", title="Fix")
                        self.assertIs(implicit.draft, False)
                        self.assertNotIn("draft", json.loads(requests[-1][3]))
                        branch_status = await api.check_status("heads/rho/fix")
                        self.assertEqual(len(branch_status.check_runs), 101)
                        self.assertIn("**failure**", repr(branch_status))
                        self.assertIn("/commits/heads/rho/fix/status", requests[-3][1])
                        self.assertIn("/commits/heads/rho/fix/check-runs", requests[-2][1])
                        with self.assertRaises(ValueError):
                            api("https://api.github.com/repos/acme/widget/issues")
                        with self.assertRaises(TypeError):
                            GhApi(token="not-a-host-credential")
            finally:
                server.close()
                await server.wait_closed()

    async def test_complete_upstream_parameters_are_forwarded(self):
        with tempfile.TemporaryDirectory() as directory:
            requests = []

            async def serve(reader, writer):
                method, path, _ = (await reader.readline()).decode().split()
                headers = {}
                while line := (await reader.readline()).decode().strip():
                    key, value = line.split(":", 1)
                    headers[key.lower()] = value.strip()
                body = await reader.readexactly(int(headers.get("content-length", 0)))
                requests.append((method, path, json.loads(body) if body else None))
                payload = b'{"number": 42}'
                writer.write(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                    + f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload
                )
                await writer.drain()
                writer.close()

            server = await asyncio.start_unix_server(serve, directory + "/octo.sock")
            try:
                with patch.dict(os.environ, {"RHO_SOCKET_PATH": directory + "/rho.sock"}):
                    async with server:
                        api = GhApi("acme", "widget")
                        self.assertEqual(inspect.signature(api.pulls.list).parameters["sort"].default,
                                         "created")
                        self.assertEqual(inspect.signature(api.checks.list_for_ref).parameters["filter"].default,
                                         "latest")
                        await api.pulls.list(state="closed", head="alice:rho/fix", base="release/next",
                                             sort="updated", direction="asc", per_page=7, page=2)
                        await api.pulls.list(query_={"sort": "updated", "direction": "desc"})
                        await api.issues.list_for_repo(
                            milestone="none", state="closed", assignee="alice", type="Bug",
                            creator="bob", mentioned="carol", issue_field_values="priority:Urgent",
                            labels="bug,ui", sort="updated", direction="asc",
                            since="2026-09-01T12:34:56Z", per_page=9, page=3,
                        )
                        await api.checks.list_for_ref("a"*40, check_name="unit tests",
                                                     status="completed", filter="all",
                                                     app_id=23, per_page=99, page=2)
                        await api.repos.get_combined_status_for_ref("a"*40, per_page=7, page=3)
                        await api.actions.list_workflow_runs_for_repo(
                            actor="alice", branch="release/next", event="pull_request", status="failure",
                            created="2026-09-01..2026-09-29", exclude_pull_requests=False,
                            check_suite_id=17, head_sha="a"*40, per_page=11, page=2,
                        )
                        await api.actions.get_workflow_run(11, exclude_pull_requests=True)
                        await api.pulls.create(head="rho/fix", head_repo="other-widgets",
                                               base="main", issue=19)
                        await api.pulls.update(42, state="closed", maintainer_can_modify=False)
                        await api.pulls.update(42, state="open", maintainer_can_modify=True)
                        await api.repos.get_combined_status_for_ref("main")
                        await api.checks.list_for_ref("heads/rho/fix")
                        await api.repos.get_combined_status_for_ref("tags/release/v1.2+rc")
                        await api.checks.list_for_ref("rho/修正")
                        await api.repos.get_combined_status_for_ref("rho/50%complete")
                        await api.checks.list_for_ref("rho/fix%2Fother")
            finally:
                server.close()
                await server.wait_closed()

            expected_queries = [
                {"state": ["closed"], "head": ["alice:rho/fix"], "base": ["release/next"],
                 "sort": ["updated"], "direction": ["asc"], "per_page": ["7"], "page": ["2"]},
                {"sort": ["updated"], "direction": ["desc"]},
                {"milestone": ["none"], "state": ["closed"], "assignee": ["alice"], "type": ["Bug"],
                 "creator": ["bob"], "mentioned": ["carol"], "issue_field_values": ["priority:Urgent"],
                 "labels": ["bug,ui"], "sort": ["updated"], "direction": ["asc"],
                 "since": ["2026-09-01T12:34:56Z"], "per_page": ["9"], "page": ["3"]},
                {"check_name": ["unit tests"], "status": ["completed"], "filter": ["all"],
                 "app_id": ["23"], "per_page": ["99"], "page": ["2"]},
                {"per_page": ["7"], "page": ["3"]},
                {"actor": ["alice"], "branch": ["release/next"], "event": ["pull_request"],
                 "status": ["failure"], "created": ["2026-09-01..2026-09-29"],
                 "exclude_pull_requests": ["false"], "check_suite_id": ["17"], "head_sha": ["a"*40],
                 "per_page": ["11"], "page": ["2"]},
                {"exclude_pull_requests": ["true"]},
            ]
            self.assertEqual(len(requests), 16)
            for (_, path, body), expected in zip(requests, expected_queries):
                self.assertEqual(parse_qs(urlsplit(path).query), expected)
                self.assertIsNone(body)
            self.assertEqual(requests[7], ("POST", "/repos/acme/widget/pulls", {
                "head": "rho/fix", "head_repo": "other-widgets", "base": "main", "issue": 19,
            }))
            self.assertEqual(requests[8], ("PATCH", "/repos/acme/widget/pulls/42", {
                "state": "closed", "maintainer_can_modify": False,
            }))
            self.assertEqual(requests[9], ("PATCH", "/repos/acme/widget/pulls/42", {
                "state": "open", "maintainer_can_modify": True,
            }))
            self.assertEqual([path for _, path, _ in requests[10:]], [
                "/repos/acme/widget/commits/main/status",
                "/repos/acme/widget/commits/heads/rho/fix/check-runs",
                "/repos/acme/widget/commits/tags/release/v1.2%2Brc/status",
                "/repos/acme/widget/commits/rho/%E4%BF%AE%E6%AD%A3/check-runs",
                "/repos/acme/widget/commits/rho/50%25complete/status",
                "/repos/acme/widget/commits/rho/fix%252Fother/check-runs",
            ])

    async def test_edit_and_feedback_methods_use_selected_paths_and_bodies(self):
        with tempfile.TemporaryDirectory() as directory:
            requests = []

            async def serve(reader, writer):
                method, path, _ = (await reader.readline()).decode().split()
                headers = {}
                while line := (await reader.readline()).decode().strip():
                    key, value = line.split(":", 1)
                    headers[key.lower()] = value.strip()
                body = await reader.readexactly(int(headers.get("content-length", 0)))
                requests.append((method, path, json.loads(body) if body else None))
                result = ({"review_decision": "APPROVED"} if path.endswith("/review-decision")
                          else [{"id": 7}] if method == "GET" else {"id": 8})
                payload = json.dumps(result).encode()
                writer.write(
                    f"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                    f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload
                )
                await writer.drain()
                writer.close()

            server = await asyncio.start_unix_server(serve, directory + "/octo.sock")
            try:
                with patch.dict(os.environ, {"RHO_SOCKET_PATH": directory + "/rho.sock"}):
                    async with server:
                        api = GhApi("acme", "widget")
                        await api.pulls.update(17, title="New title", body="New description")
                        await api.pulls.update(17, base="release/next")
                        self.assertEqual((await api.pulls.review_decision(17)).review_decision, "APPROVED")
                        await api.issues.list_comments(17, page=2, per_page=5)
                        await api.pulls.list_reviews(17, page=3)
                        await api.pulls.list_review_comments(17, page=4)
                        await api.issues.create_comment(17, body="General reply")
                        await api.pulls.create_reply_for_review_comment(
                            17, 99, body="Inline reply"
                        )
            finally:
                server.close()
                await server.wait_closed()
            self.assertEqual(
                requests,
                [
                    ("PATCH", "/repos/acme/widget/pulls/17", {
                        "title": "New title", "body": "New description"
                    }),
                    ("PATCH", "/repos/acme/widget/pulls/17", {"base": "release/next"}),
                    ("GET", "/repos/acme/widget/pulls/17/review-decision", None),
                    ("GET", "/repos/acme/widget/issues/17/comments?page=2&per_page=5", None),
                    ("GET", "/repos/acme/widget/pulls/17/reviews?page=3", None),
                    ("GET", "/repos/acme/widget/pulls/17/comments?page=4", None),
                    ("POST", "/repos/acme/widget/issues/17/comments", {"body": "General reply"}),
                    ("POST", "/repos/acme/widget/pulls/17/comments/99/replies", {"body": "Inline reply"}),
                ],
            )

    async def test_actions_inspection_logs_and_all_rerun_scopes(self):
        with tempfile.TemporaryDirectory() as directory:
            requests = []

            async def serve(reader, writer):
                method, path, _ = (await reader.readline()).decode().split()
                headers = {}
                while line := (await reader.readline()).decode().strip():
                    key, value = line.split(":", 1)
                    headers[key.lower()] = value.strip()
                body = await reader.readexactly(int(headers.get("content-length", 0)))
                requests.append((method, path, json.loads(body) if body else None, headers))
                if path.endswith("/logs"):
                    content_type = "text/plain" if "/jobs/" in path else "application/zip"
                    payload = b"failure at step 3\n" if "/jobs/" in path else b"PK\x03\x04archive"
                    status = "200 OK"
                elif method == "POST":
                    content_type, payload, status = "application/json", b"", "201 Created"
                else:
                    content_type, payload, status = "application/json", b'{"id": 9}', "200 OK"
                writer.write(
                    f"HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n"
                    f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload
                )
                await writer.drain()
                writer.close()

            server = await asyncio.start_unix_server(serve, directory + "/octo.sock")
            try:
                with patch.dict(os.environ, {"RHO_SOCKET_PATH": directory + "/rho.sock"}):
                    async with server:
                        api = GhApi("acme", "widget")
                        await api.pulls.list_files(17, per_page=20)
                        await api.checks.get(7)
                        await api.checks.list_annotations(7, page=2)
                        await api.actions.list_workflow_runs_for_repo(head_sha="a"*40, page=3)
                        await api.actions.get_workflow_run(11)
                        await api.actions.list_jobs_for_workflow_run(11, filter="all")
                        await api.actions.get_job_for_workflow_run(12)
                        self.assertEqual(await api.actions.download_job_logs_for_workflow_run(12),
                                         "failure at step 3\n")
                        self.assertEqual(await api.actions.download_workflow_run_logs(11),
                                         b"PK\x03\x04archive")
                        await api.actions.re_run_workflow(11, enable_debug_logging=True)
                        await api.actions.re_run_workflow_failed_jobs(11)
                        await api.actions.re_run_job_for_workflow_run(12, enable_debugger=True)
            finally:
                server.close()
                await server.wait_closed()
            self.assertEqual(
                [(method, path, body) for method, path, body, _ in requests],
                [
                    ("GET", "/repos/acme/widget/pulls/17/files?per_page=20", None),
                    ("GET", "/repos/acme/widget/check-runs/7", None),
                    ("GET", "/repos/acme/widget/check-runs/7/annotations?page=2", None),
                    ("GET", "/repos/acme/widget/actions/runs?head_sha="+"a"*40+"&page=3", None),
                    ("GET", "/repos/acme/widget/actions/runs/11", None),
                    ("GET", "/repos/acme/widget/actions/runs/11/jobs?filter=all", None),
                    ("GET", "/repos/acme/widget/actions/jobs/12", None),
                    ("GET", "/repos/acme/widget/actions/jobs/12/logs", None),
                    ("GET", "/repos/acme/widget/actions/runs/11/logs", None),
                    ("POST", "/repos/acme/widget/actions/runs/11/rerun", {"enable_debug_logging": True}),
                    ("POST", "/repos/acme/widget/actions/runs/11/rerun-failed-jobs", {}),
                    ("POST", "/repos/acme/widget/actions/jobs/12/rerun", {"enable_debugger": True}),
                ],
            )
            self.assertTrue(all("authorization" not in headers for _, _, _, headers in requests))



if __name__ == "__main__":
    unittest.main()
