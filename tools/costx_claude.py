#!/usr/bin/env python3
"""Cost attribution for Claude Code transcripts (~/.claude/projects/*/*.jsonl), Fable models only.

Same three views as costx.py (occupancy / round-trip / output) plus the same
counterfactuals (caps, wait, compaction threshold, dedup).  Block token sizes are
calibrated per request against the uncached token count the API reported.
"""
import argparse, collections, glob, hashlib, json, os, re, statistics, sys
from multiprocessing import Pool

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from costx import classify_commands, rg_purpose, primary_purpose, PURPOSE_PRIORITY  # noqa: E402

PRICES = {  # $/M
    "fable": dict(inp=10.0, cr=0.25, cw5=12.5, cw1=12.5, out=50.0),   # per pricing page: read $0.25, write $12.50
    "opus": dict(inp=5.0, cr=0.5, cw5=6.25, cw1=10.0, out=25.0),
}
# Fitted against reported uncached tokens (least squares over 21k tool-loop requests, R2 0.67).
# Thinking in the transcript is a summary (about 0.23 chars per billed thinking token) and the
# previous step's thinking is re-sent as new input inside a tool loop, so it is scaled up.
CHARS_PER_TOKEN = {"tool_result": 2.3, "thinking": 0.226, "text": 1.9, "user": 2.9, "mail": 2.9, "tool_use": 2.2, "summary": 2.9, "notif": 2.9}
CLAUDE_DIR = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")


def price_of(model):
    return PRICES["opus"] if "opus" in (model or "") else PRICES["fable"]


def usd_input(u, p):
    return (u["inp"] * p["inp"] + u["cr"] * p["cr"] + u["cw5"] * p["cw5"] + u["cw1"] * p["cw1"]) / 1e6


def categorize_tool(name, inp):
    """tool_use -> (fine, purposes:list, flags:set)"""
    inp = inp if isinstance(inp, dict) else {}
    flags = set()
    if name == "Bash":
        cmd = inp.get("command") or ""
        if inp.get("run_in_background"):
            flags.add("bg")
        segs = classify_commands(cmd, cmd)
        if not segs:
            return ("bash", ["misc/bash"], flags)
        fines = [f for f, _ in segs]
        purposes = [p for _, p in segs]
        if len(segs) > 1:
            flags.add("multi")
        return ("+".join(fines[:3]), purposes, flags)
    if name in ("BashOutput", "TaskOutput"):
        return (name, ["wait/poll"], flags)
    if name == "Monitor":
        return ("Monitor", ["wait/monitor"], flags)
    if name == "Read":
        if inp.get("offset") is not None or inp.get("limit") is not None:
            return ("Read range", ["read/range"], flags)
        return ("Read", ["read/file"], flags)
    if name == "Grep":
        if inp.get("output_mode") in ("files_with_matches", "count"):
            return ("Grep files", ["search/files"], flags)
        words = ["rg"]
        for k in ("-A", "-B", "-C"):
            if inp.get(k) is not None:
                flags.add("ctx")
        words.append(str(inp.get("pattern", "")))
        return ("Grep", [rg_purpose(words)], flags)
    if name == "Glob":
        return ("Glob", ["search/files"], flags)
    if name in ("Edit", "MultiEdit", "Write", "NotebookEdit"):
        return (name, ["edit/" + name.lower()], flags)
    if name.startswith("mcp__rho__"):
        return (name[10:], ["comms/" + name[10:]], flags)
    if name in ("SendMessage", "ListAgents", "AskUserQuestion", "Task", "Agent"):
        return (name, ["comms/" + name.lower()], flags)
    if name in ("WebFetch", "WebSearch"):
        return (name, ["web/" + name[3:].lower()], flags)
    if name in ("KillShell", "KillBash"):
        return (name, ["proc/kill"], flags)
    return (name, ["misc/" + name.lower()], flags)


def user_kind(text, is_summary):
    t = text.lstrip()
    if is_summary:
        return "summary", "compaction/summary"
    if t.startswith("<task-notification>"):
        return "notif", "wait/notify"
    m = re.match(r"Message Type: (\w+)\s*\nSender: ([a-z]+)-", t)
    if m:
        return "mail", "mail/" + m.group(2)
    return "user", "user/message"


def block_text(content):
    if isinstance(content, str):
        return content
    out = []
    for b in content or []:
        if isinstance(b, dict):
            if b.get("type") == "text":
                out.append(b.get("text", ""))
            elif b.get("type") == "image":
                out.append("<image>" + " " * 1500)
        elif isinstance(b, str):
            out.append(b)
    return "\n".join(out)


def est(kind, chars):
    return max(1, int(chars / CHARS_PER_TOKEN.get(kind, 4.0)))


KEEP_TEXT_CATS = ("search", "read", "vcs")


def parse_file(path):
    """One transcript file -> dict(blocks, requests, compactions, meta)."""
    blocks, requests, compactions = [], [], []
    tools = {}              # tool_use_id -> block idx
    pending = []            # new block idxs since last request
    ctx_start = 0           # first block idx of the current context (after compaction)
    turn_start = 0          # first block idx of the current assistant turn (thinking before it is stripped)
    cur_rid, cur_req = None, None
    prev_req = None
    by_rid = {}
    sid = os.path.basename(path)[:-6]
    meta = dict(session=sid, path=path, entrypoint=None, cwd=None, first_ts=None, last_ts=None, models=collections.Counter())

    def add_block(kind, chars, cat, fine="", tuid=None, name=None, text=None, flags=()):
        b = dict(kind=kind, chars=chars, est=est(kind, chars), tok=None, cat=cat, fine=fine, tuid=tuid, name=name,
                 idx=len(blocks), req=len(requests), flags=sorted(flags))
        if text is not None:
            b["text"] = text
        blocks.append(b)
        return b["idx"]

    with open(path, "rb") as fh:
        for raw in fh:
            try:
                d = json.loads(raw)
            except Exception:
                continue
            t = d.get("type")
            ts = d.get("timestamp")
            if ts:
                meta["first_ts"] = meta["first_ts"] or ts
                meta["last_ts"] = ts
            if meta["entrypoint"] is None and d.get("entrypoint"):
                meta["entrypoint"] = d["entrypoint"]
                meta["cwd"] = d.get("cwd")
            if t == "system" and d.get("subtype") == "compact_boundary":
                cm = d.get("compactMetadata") or {}
                compactions.append(dict(req=len(requests), pre=cm.get("preTokens"), trigger=cm.get("trigger"), ts=ts))
                ctx_start = len(blocks)
                turn_start = ctx_start
                pending = []
                prev_req = None
                continue
            if t == "user":
                m = d.get("message") or {}
                content = m.get("content")
                if isinstance(content, str):
                    kind, cat = user_kind(content, d.get("isCompactSummary"))
                    if kind == "notif":
                        mm = re.search(r"<tool-use-id>(\S+)</tool-use-id>", content)
                        if mm and mm.group(1) in tools:
                            cat = "wait/notify:" + blocks[tools[mm.group(1)]]["cat"]
                    if kind != "notif":
                        turn_start = len(blocks)
                    pending.append(add_block(kind, len(content), cat))
                    continue
                for b in content or []:
                    if not isinstance(b, dict):
                        continue
                    bt = b.get("type")
                    if bt == "tool_result":
                        txt = block_text(b.get("content"))
                        tu = tools.get(b.get("tool_use_id"))
                        if tu is not None:
                            cat, fine, name, flags = blocks[tu]["cat"], blocks[tu]["fine"], blocks[tu]["name"], set(blocks[tu]["flags"])
                        else:
                            cat, fine, name, flags = "misc/orphan", "", None, set()
                        if b.get("is_error"):
                            flags.add("error")
                        if "run_in_background" in txt[:200] or txt.startswith("Command running in background"):
                            flags.add("bg-ack")
                        keep = txt if cat.split("/")[0] in KEEP_TEXT_CATS and len(txt) < 400_000 else None
                        pending.append(add_block("tool_result", len(txt), cat, fine, b.get("tool_use_id"), name, keep, flags))
                    elif bt == "text":
                        txt = b.get("text", "")
                        kind, cat = user_kind(txt, d.get("isCompactSummary"))
                        if kind != "notif":
                            turn_start = len(blocks)
                        pending.append(add_block(kind, len(txt), cat))
                    elif bt == "image":
                        pending.append(add_block("user", 1600, "user/image"))
                continue
            if t != "assistant":
                continue
            m = d.get("message") or {}
            rid = d.get("requestId") or ("nr:" + str(d.get("uuid")))
            model = m.get("model") or ""
            usage = m.get("usage") or {}
            if rid != cur_rid and rid in by_rid:
                cur_rid, cur_req = rid, by_rid[rid]   # continuation of an earlier request line
            elif rid != cur_rid:
                cur_rid = rid
                cc = usage.get("cache_creation") or {}
                cw5 = cc.get("ephemeral_5m_input_tokens", usage.get("cache_creation_input_tokens", 0) or 0)
                cw1 = cc.get("ephemeral_1h_input_tokens", 0) or 0
                u = dict(inp=usage.get("input_tokens", 0) or 0, cr=usage.get("cache_read_input_tokens", 0) or 0,
                         cw5=cw5, cw1=cw1, out=usage.get("output_tokens", 0) or 0,
                         think=(usage.get("output_tokens_details") or {}).get("thinking_tokens"))
                cur_req = dict(seq=len(requests), rid=rid, ts=ts, model=model, u=u, new=list(pending), outs=[],
                               ctx_start=ctx_start, turn_start=turn_start, ctx_end=len(blocks), fresh=prev_req is None,
                               billed=bool(usage) and "<synthetic>" not in model)
                meta["models"][model] += 1
                # calibrate new blocks against uncached tokens
                newb = [blocks[i] for i in cur_req["new"]]
                if prev_req is not None:
                    newb = [blocks[i] for i in prev_req["outs"]] + newb
                unc = u["inp"] + u["cw5"] + u["cw1"]
                total = unc + u["cr"]
                se = sum(b["est"] for b in newb)
                if prev_req is not None and cur_req["billed"] and u["cr"] > 0 and se > 0 and unc <= 2.0 * se + 3000:
                    f = max(0.6, min(1.6, unc / se))
                    for b in newb:
                        if b["tok"] is None:
                            b["tok"] = max(1, int(b["est"] * f))
                    cur_req["cal"] = f
                for b in newb:
                    if b["tok"] is None:
                        b["tok"] = b["est"]
                cur_req["total"] = total
                requests.append(cur_req)
                by_rid[rid] = cur_req
                pending = []
                prev_req = cur_req if cur_req["billed"] else prev_req
            for b in m.get("content") or []:
                if not isinstance(b, dict):
                    continue
                bt = b.get("type")
                if bt == "thinking":
                    txt = b.get("thinking", "") or ""
                    i = add_block("thinking", len(txt) + 40, "reasoning/thinking")
                    if cur_req["u"].get("think"):
                        blocks[i]["tok"] = blocks[i]["est"] = max(1, cur_req["u"]["think"])
                    cur_req["outs"].append(i)
                elif bt == "text":
                    cur_req["outs"].append(add_block("text", len(b.get("text", "")), "assistant/text"))
                elif bt == "tool_use":
                    name = b.get("name", "?")
                    fine, purposes, flags = categorize_tool(name, b.get("input"))
                    cat = primary_purpose(set(purposes))
                    inp = b.get("input")
                    chars = len(json.dumps(inp)) if inp is not None else 20
                    idx = add_block("tool_use", chars + 30, cat, fine, b.get("id"), name, None, flags)
                    blocks[idx]["purposes"] = purposes
                    tools[b.get("id")] = idx
                    cur_req["outs"].append(idx)
    for b in blocks:
        if b["tok"] is None:
            b["tok"] = b["est"]
    # base (system prompt + tools) = residual on fresh requests
    resid = []
    for r in requests:
        if r["fresh"] and r["billed"] and r["total"]:
            inctx = ctx_blocks(blocks, r)
            resid.append(r["total"] - sum(blocks[i]["tok"] for i in inctx))
    meta["base"] = max(0, int(statistics.median(resid))) if resid else 0
    meta["nreq"] = len(requests)
    meta["models"] = dict(meta["models"])
    return dict(blocks=blocks, requests=requests, compactions=compactions, meta=meta)


def ctx_blocks(blocks, r):
    """Block idxs in context for request r (thinking from earlier turns is stripped)."""
    out = []
    for i in range(r["ctx_start"], r["ctx_end"]):
        b = blocks[i]
        if b["kind"] == "thinking" and i < r["turn_start"]:
            continue
        out.append(i)
    return out


def trip_label(blocks, r):
    cats = collections.Counter()
    for i in r["new"]:
        b = blocks[i]
        c = b["cat"]
        if c.startswith("wait/notify:"):
            c = "wait/notify"
        cats[c] += 1
    if not cats:
        return "continuation"
    if "compaction/summary" in cats:
        return "compaction/summary"
    if "user/message" in cats or "user/image" in cats:
        return "user/message"
    for c in cats:
        if c.startswith("mail/"):
            return c
    return primary_purpose(set(cats))


# --------------------------------------------------------------------------
# aggregation

def add(d, k, *vals):
    cur = d.get(k)
    if cur is None:
        d[k] = list(vals)
    else:
        for i, v in enumerate(vals):
            cur[i] += v


def analyze(S, model_filter="fable", since=None):
    blocks, requests, meta = S["blocks"], S["requests"], S["meta"]
    A = dict(occ={}, trip={}, outv={}, total=0.0, in_usd=0.0, out_usd=0.0, n=0, comp={}, ctx_sum=0, tokens=collections.Counter(),
             buckets={}, gaps=[], wait_chain=[], models={}, weeks={}, big={}, caps={}, sess=meta["session"], entry=meta["entrypoint"],
             first_ts=meta["first_ts"], last_ts=meta["last_ts"], cwd=meta["cwd"], base=meta["base"], nreq=len(requests),
             ncomp=len(S["compactions"]), comp_pre=[c["pre"] for c in S["compactions"] if c["pre"]], out_tok=collections.Counter(),
             think_tok=0, out_tokens=0, dup={}, notif_chain=[], per_req=[])
    base = meta["base"]
    seen_lines = set()
    prev_label, chain = None, 0
    for r in requests:
        if not r["billed"] or model_filter not in r["model"] or (since and (r["ts"] or "") < since):
            continue
        u, p = r["u"], price_of(r["model"])
        in_usd, out_usd = usd_input(u, p), u["out"] * p["out"] / 1e6
        A["tokens"]["cr_usd"] += u["cr"] * p["cr"] / 1e6
        A["tokens"]["cw_usd"] += (u["cw5"] * p["cw5"] + u["cw1"] * p["cw1"] + u["inp"] * p["inp"]) / 1e6
        cost = in_usd + out_usd
        A["total"] += cost; A["in_usd"] += in_usd; A["out_usd"] += out_usd; A["n"] += 1
        A["ctx_sum"] += r["total"]
        for k in ("inp", "cr", "cw5", "cw1", "out"):
            A["tokens"][k] += u[k]
        add(A["models"], r["model"], cost, 1)
        wk = (r["ts"] or "")[:10]
        add(A["weeks"], wk, cost, 1)
        bucket = "<50k" if r["total"] < 50e3 else "<100k" if r["total"] < 100e3 else "<150k" if r["total"] < 150e3 else "<200k" if r["total"] < 200e3 else "<250k" if r["total"] < 250e3 else ">=250k"
        add(A["buckets"], bucket, cost, 1)
        # occupancy: distribute in_usd over ctx blocks + base, weighted by tok * rate
        inctx = ctx_blocks(blocks, r)
        newset = set(r["new"])
        prev_outs = set()
        if r["seq"] > 0:
            for q in range(r["seq"] - 1, max(-1, r["seq"] - 3), -1):
                if requests[q]["billed"]:
                    prev_outs = set(requests[q]["outs"]); break
        unc = u["inp"] + u["cw5"] + u["cw1"]
        unc_rate = usd_input(dict(inp=u["inp"], cr=0, cw5=u["cw5"], cw1=u["cw1"]), p) / max(1, unc)  # $/token
        cr_rate = p["cr"] / 1e6
        weights, wsum = [], 0.0
        for i in inctx:
            b = blocks[i]
            w = b["tok"] * (unc_rate if (i in newset or i in prev_outs) and unc > 0 else cr_rate)
            weights.append((b["cat"], b["kind"], w, b)); wsum += w
        wb = base * (unc_rate if r["fresh"] else cr_rate); wsum += wb
        if wsum > 0:
            scale = in_usd / wsum
            add(A["occ"], "system/base", wb * scale, base)
            for cat, kind, w, b in weights:
                key = cat if kind in ("tool_result", "thinking", "summary", "text", "user", "mail", "notif") else "calls/" + cat.split("/")[0]
                if kind == "tool_result" and cat.startswith("wait/notify"):
                    key = "wait/notify"
                add(A["occ"], key, w * scale, b["tok"])
                if kind == "tool_result":
                    A["per_req"].append((r["seq"], cat, b["tok"], w * scale)) if False else None
        # round trip
        label = trip_label(blocks, r)
        add(A["trip"], label, cost, 1)
        if label.startswith("wait/"):
            chain = chain + 1 if prev_label and prev_label.startswith("wait/") else 1
        else:
            if chain:
                A["wait_chain"].append(chain)
            chain = 0
        prev_label = label
        # output view
        outs = [blocks[i] for i in r["outs"]]
        oc = sum(b["chars"] for b in outs) or 1
        A["out_tokens"] += u["out"]
        if u.get("think"):
            A["think_tok"] += u["think"]
        for b in outs:
            key = b["cat"] if b["kind"] != "tool_use" else "call/" + b["cat"]
            add(A["outv"], key, out_usd * b["chars"] / oc, int(u["out"] * b["chars"] / oc))
        # per-block bookkeeping for caps / dedup (only on the request where a block first appears)
        for i in r["new"]:
            b = blocks[i]
            if b["kind"] != "tool_result":
                continue
            top = b["cat"].split("/")[0]
            # remaining life in this context (how many later requests see it)
            add(A["big"], b["cat"], b["tok"], 1, b["tok"] if b["tok"] > 5000 else 0, 1 if b["tok"] > 5000 else 0, b["tok"] if b["tok"] > 10000 else 0)
            if b.get("text") is not None:
                lines = [l for l in b["text"].split("\n") if len(l.strip()) > 12]
                dupn = 0
                for l in lines:
                    h = hashlib.blake2b(l.strip().encode(), digest_size=8).digest()
                    if h in seen_lines:
                        dupn += 1
                    else:
                        seen_lines.add(h)
                add(A["dup"], top, len(lines), dupn)
    return A


def merge(aggs):
    M = dict(occ={}, trip={}, outv={}, total=0.0, in_usd=0.0, out_usd=0.0, n=0, ctx_sum=0, tokens=collections.Counter(), buckets={},
             wait_chain=[], models={}, weeks={}, big={}, sessions=[], comp_pre=[], ncomp=0, think_tok=0, out_tokens=0, dup={})
    for A in aggs:
        for k in ("occ", "trip", "outv", "buckets", "models", "weeks", "big", "dup"):
            for kk, v in A[k].items():
                add(M[k], kk, *v)
        for k in ("total", "in_usd", "out_usd", "n", "ctx_sum", "ncomp", "think_tok", "out_tokens"):
            M[k] += A[k]
        M["tokens"].update(A["tokens"])
        M["wait_chain"] += A["wait_chain"]
        M["comp_pre"] += A["comp_pre"]
        M["sessions"].append(dict(sess=A["sess"], total=A["total"], n=A["n"], entry=A["entry"], first=A["first_ts"], last=A["last_ts"],
                                  cwd=A["cwd"], base=A["base"], ncomp=A["ncomp"], models=A["models"]))
    return M


def rollup(d, depth):
    out = {}
    for k, v in d.items():
        add(out, "/".join(k.split("/")[:depth]), *v)
    return out


def table(title, d, total, top=25, unit="$"):
    print(f"\n== {title} ==")
    rows = sorted(d.items(), key=lambda kv: -kv[1][0])
    for k, v in rows[:top]:
        extra = "  ".join(f"{x:,.0f}" for x in v[1:])
        print(f"  {k:<34} ${v[0]:>9,.2f}  {100 * v[0] / max(total, 1e-9):5.1f}%   {extra}")


# --------------------------------------------------------------------------
# counterfactuals (per session, replayed)

CAPS_A = {"search": 2000, "vcs": 3000, "read": 3000, "build": 2000, "test": 2000, "remote": 2000, "web": 2000}


def simulate(S, model_filter, since=None, caps=None, collapse_wait=False, compact_at=None, post_ctx=27000, comp_overhead_usd=2.5, prune=None):
    """Replay a session under a policy; return (cost, trips, compactions).

    Cost model: per request, ctx tokens charged at cache-read rate except the new
    tokens (uncached rate); output unchanged.  Compaction at threshold replaces the
    context with post_ctx tokens plus overhead (summary trip + re-reads).
    prune=(age, kinds) drops blocks of those kinds/top-level cats older than `age` requests
    when over threshold (rebuild at cache-write price + $0.92 re-read tax), compacting only
    if still over.  post_ctx/overhead are the observed Claude values (27k, ~$2.5).
    """
    blocks, requests, meta = S["blocks"], S["requests"], S["meta"]
    base = meta["base"]
    tok = {}
    for b in blocks:
        t = b["tok"]
        if caps and b["kind"] == "tool_result":
            c = caps.get(b["cat"].split("/")[0], caps.get("*", 5000))
            t = min(t, c)
        tok[b["idx"]] = t
    cost, trips, ncomp, nprune = 0.0, 0, 0, 0
    prev_label = None
    prev_outs = []
    # simulated compaction state: ctx starts at sim_start (>= real ctx_start)
    sim_start, sim_ctx_offset, removed = None, 0, set()
    last_ctx_start = None
    for r in requests:
        if not r["billed"] or model_filter not in r["model"] or (since and (r["ts"] or "") < since):
            continue
        u, p = r["u"], price_of(r["model"])
        label = trip_label(blocks, r)
        is_wait = label.startswith("wait/")
        # next request also a wait?  (collapse keeps last trip in a chain)
        if collapse_wait and is_wait and prev_label and prev_label.startswith("wait/"):
            # skip this trip: its new blocks are also dropped from context (partial outputs)
            for i in r["new"]:
                removed.add(i)
            for i in r["outs"]:
                removed.add(i)
            prev_label = label
            continue
        prev_label = label
        if r["ctx_start"] != last_ctx_start:
            last_ctx_start, sim_start, sim_ctx_offset = r["ctx_start"], r["ctx_start"], 0
        inctx = [i for i in ctx_blocks(blocks, r) if i >= sim_start and i not in removed]
        ctx_tok = base + sim_ctx_offset + sum(tok[i] for i in inctx)
        newset = set(r["new"]) | set(prev_outs)
        if compact_at and ctx_tok > compact_at:
            if prune:
                age, kinds = prune
                cand = [i for i in inctx if (blocks[i]["kind"] in kinds or blocks[i]["cat"].split("/")[0] in kinds)
                        and r["seq"] - blocks[i]["req"] > age]
                if cand:
                    nprune += 1
                    for i in cand:
                        removed.add(i)
                    inctx = [i for i in inctx if i not in removed]
                    ctx_tok = base + sim_ctx_offset + sum(tok[i] for i in inctx)
                    cost += ctx_tok * p["cw1"] / 1e6 + 0.92  # rebuild (1h cache write) + re-read tax
            if ctx_tok > compact_at:
                ncomp += 1
                cost += comp_overhead_usd
                sim_start, sim_ctx_offset = r["ctx_end"], post_ctx
                inctx = []
                ctx_tok = base + sim_ctx_offset
                newset = set()
        unc = u["inp"] + u["cw5"] + u["cw1"]
        new_tok = sum(tok[i] for i in inctx if i in newset)
        if r["fresh"]:
            new_tok = ctx_tok
        unc_rate = usd_input(dict(inp=u["inp"], cr=0, cw5=u["cw5"], cw1=u["cw1"]), p) / max(1, unc) if unc else p["inp"] / 1e6
        # real cache misses (idle > 1h rewrites) stay under every policy: keep the excess of the
        # reported uncached tokens over the uncapped size of the new blocks
        real_new = sum(blocks[i]["tok"] for i in r["new"]) + sum(blocks[i]["tok"] for i in prev_outs)
        miss_extra = max(0, unc - int(1.3 * real_new))
        new_tok = min(new_tok + miss_extra, ctx_tok) if not r["fresh"] else ctx_tok
        cost += new_tok * unc_rate + max(0, ctx_tok - new_tok) * p["cr"] / 1e6 + u["out"] * p["out"] / 1e6
        trips += 1
        prev_outs = r["outs"]
    return cost, trips, ncomp, nprune


POLICIES = [
    ("actual (sim baseline)", {}),
    ("1 caps A", dict(caps=CAPS_A)),
    ("3 blocking wait (chains -> last trip)", dict(collapse_wait=True)),
    ("4 compaction at 200k", dict(compact_at=200_000)),
    ("4 compaction at 150k", dict(compact_at=150_000)),
    ("4 compaction at 250k", dict(compact_at=250_000)),
    ("1+3", dict(caps=CAPS_A, collapse_wait=True)),
    ("1+3+4 (200k)", dict(caps=CAPS_A, collapse_wait=True, compact_at=200_000)),
    ("1+3+4 (250k)", dict(caps=CAPS_A, collapse_wait=True, compact_at=250_000)),
    ("prune outputs>50req at 200k", dict(compact_at=200_000, prune=(50, {"tool_result"}))),
    ("prune outputs>20req at 200k", dict(compact_at=200_000, prune=(20, {"tool_result"}))),
    ("prune out+mail+text>20 at 200k", dict(compact_at=200_000, prune=(20, {"tool_result", "mail", "text"}))),
    ("prune out+mail+text+calls>20 at 200k", dict(compact_at=200_000, prune=(20, {"tool_result", "mail", "text", "tool_use"}))),
    ("1 + prune out+mail+text+calls>20", dict(caps=CAPS_A, compact_at=200_000, prune=(20, {"tool_result", "mail", "text", "tool_use"}))),
    ("1 + prune out+mail+text+calls>50", dict(caps=CAPS_A, compact_at=200_000, prune=(50, {"tool_result", "mail", "text", "tool_use"}))),
]


def _sim_worker(args):
    path, model_filter, since = args
    S = parse_file(path)
    out = {}
    for name, kw in POLICIES:
        out[name] = simulate(S, model_filter, since, **kw)
    return out


def _agg_worker(args):
    path, model_filter, since = args
    return analyze(parse_file(path), model_filter, since)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="fable")
    ap.add_argument("--jobs", type=int, default=8)
    ap.add_argument("--top", type=int, default=25)
    ap.add_argument("--no-sim", action="store_true")
    ap.add_argument("--min-usd", type=float, default=0.0)
    ap.add_argument("--since", default=None, help="ISO date, e.g. 2026-08-08")
    args = ap.parse_args()
    files = sorted(glob.glob(os.path.join(CLAUDE_DIR, "projects", "*", "*.jsonl")))
    with Pool(args.jobs) as pool:
        aggs = pool.map(_agg_worker, [(f, args.model, args.since) for f in files])
    aggs = [a for a in aggs if a["n"] > 0 and a["total"] >= args.min_usd]
    M = merge(aggs)
    T = M["total"]
    tk = M["tokens"]
    print(f"Claude transcripts, model filter '{args.model}': {len(aggs)} sessions, {M['n']:,} requests, ${T:,.2f}")
    print(f"  input ${M['in_usd']:,.2f} (uncached {tk['inp']/1e6:.1f}M, cache-read {tk['cr']/1e6:.1f}M, cache-write 5m {tk['cw5']/1e6:.1f}M, 1h {tk['cw1']/1e6:.1f}M)")
    print(f"  output ${M['out_usd']:,.2f} ({tk['out']/1e6:.1f}M tokens, thinking {M['think_tok']/1e6:.1f}M where reported)")
    print(f"  bill split: cache reads (scales with trips x context) ${tk['cr_usd']:,.0f} = {100*tk['cr_usd']/T:.0f}%; "
          f"cache writes (per trip, new tokens) ${tk['cw_usd']:,.0f} = {100*tk['cw_usd']/T:.0f}%; output (per trip) ${M['out_usd']:,.0f} = {100*M['out_usd']/T:.0f}%")
    print(f"  per trip: cache read ${tk['cr_usd']/max(1,M['n']):.3f}, write ${tk['cw_usd']/max(1,M['n']):.3f} ({(tk['inp']+tk['cw5']+tk['cw1'])/max(1,M['n']):,.0f} new tok), output ${M['out_usd']/max(1,M['n']):.3f} ({tk['out']/max(1,M['n']):,.0f} tok)")
    print(f"  avg context {M['ctx_sum']/max(1,M['n']):,.0f}, $/trip {T/max(1,M['n']):.3f}, cache-hit share {100*tk['cr']/max(1,tk['inp']+tk['cr']+tk['cw5']+tk['cw1']):.1f}%")
    print(f"  compactions {M['ncomp']} (preTokens p50 {statistics.median(M['comp_pre']) if M['comp_pre'] else 0:,.0f})")
    table("cost by model", M["models"], T)
    table("cost by context bucket", M["buckets"], T)
    table("cost by day", dict(sorted(M["weeks"].items())), T, top=60)
    table("OCCUPANCY (what sits in context), top level", rollup(M["occ"], 1), M["in_usd"])
    table("OCCUPANCY, fine", M["occ"], M["in_usd"], top=args.top)
    table("ROUND-TRIP (what caused the request), top level", rollup(M["trip"], 1), T)
    table("ROUND-TRIP, fine", M["trip"], T, top=args.top)
    table("OUTPUT (what was generated)", M["outv"], M["out_usd"], top=15)
    wc = M["wait_chain"]
    if wc:
        print(f"\nwait chains: {len(wc)} chains, {sum(wc)} trips, mean {statistics.mean(wc):.1f}, p90 {sorted(wc)[int(0.9*len(wc))-1]}")
    print("\n== tool outputs by purpose: tokens, count, tokens in outputs >5k, count >5k, tokens in outputs >10k ==")
    for k, v in sorted(M["big"].items(), key=lambda kv: -kv[1][0])[:20]:
        print(f"  {k:<28} {v[0]/1e6:6.2f}M {v[1]:6,}  >5k: {v[2]/1e6:5.2f}M ({v[3]:,})  >10k: {v[4]/1e6:5.2f}M")
    print("\n== lines already seen earlier in the session (search/read/vcs outputs) ==")
    for k, v in M["dup"].items():
        print(f"  {k:<10} lines {v[0]:9,}  dup {v[1]:9,} ({100*v[1]/max(1,v[0]):.1f}%)")
    print("\n== sessions ==")
    for s in sorted(M["sessions"], key=lambda s: -s["total"])[:args.top]:
        print(f"  {s['sess'][:8]} ${s['total']:8.2f} {s['n']:5} req  comp {s['ncomp']:2}  base {s['base']:6,}  {s['entry'] or '?':6} {(s['first'] or '')[:10]} {os.path.basename(s['cwd'] or '')}")
    tops = sorted(s["total"] for s in M["sessions"])[::-1]
    print(f"  top 5 sessions {100*sum(tops[:5])/T:.0f}%, top 10 {100*sum(tops[:10])/T:.0f}%")
    if args.no_sim:
        return
    print("\n== policy simulation (all fable sessions) ==")
    with Pool(args.jobs) as pool:
        sims = pool.map(_sim_worker, [(f, args.model, args.since) for f in files])
    tot = {name: [0.0, 0, 0, 0] for name, _ in POLICIES}
    for s in sims:
        for name, v in s.items():
            for i in range(4):
                tot[name][i] += v[i]
    basecost = tot[POLICIES[0][0]][0]
    print(f"  {'policy':<40} {'cost':>10} {'saves':>7} {'x':>5} {'trips':>8} {'comp':>6} {'prune':>6}")
    for name, _ in POLICIES:
        c, tr, nc, np_ = tot[name]
        print(f"  {name:<40} ${c:9,.2f} {100*(1-c/basecost):6.1f}% {basecost/max(c,1e-9):5.2f} {tr:8,} {nc:6,} {np_:6,}")
    print(f"  (sim baseline vs actual bill: ${basecost:,.2f} vs ${T:,.2f})")


if __name__ == "__main__":
    main()
