#!/usr/bin/env python3
"""Cost attribution explorer for Rho GPT (Responses API) sessions.

Reads ~/.local/state/rho/debug/provider-requests/<stem>-<seq>-{request,response}.json
and reconstructs per request which block cost what. Since 2026-08-28 the
provider returns `usage.attribution` with exact per-item tokens; older
responses are estimated from text size (calibrated chars/token per kind) and
scaled to the reported totals.

Views (each sums to 100% of its own bill, do not add them):
  occupancy   input cost of a block category sitting in context
  roundtrip   input cost of a request, charged to the block that caused it
  output      output cost by the category of the produced block

Usage:
  costx.py sessions [--since 14d] [--model sol] [--min-requests N]
  costx.py analyze [STEM...] [--since 14d | --recent N] [--model sol] [--top K] [--jobs J]
  costx.py show STEM [--from A --to B] [--width W]
  costx.py req STEM SEQ
  costx.py blocks STEM [--top K]
  costx.py grep STEM PATTERN
"""

import argparse
import collections
import gzip
import json
import multiprocessing
import os
import re
import shlex
import sys
import time

DIR = os.path.expanduser("~/.local/state/rho/debug/provider-requests")
CACHE = "/tmp/costx-cache"
CACHE_VERSION = 12
TEXT_KEEP = 1500  # chars of block text kept in the cache (grep/show snippets)

# $/M tokens. Matches rho-gui bucket_cost_usd fallback used for gpt.
PRICE = {"input": 5.0, "cached": 0.5, "output": 30.0}

# chars per token, calibrated on attributed sessions (2026-09-03)
EST_RATIO = {
    "custom_tool_call_output": 3.67, "function_call_output": 3.2, "custom_tool_call": 3.5,
    "function_call": 2.3, "message:user": 4.0, "message:developer": 4.56,
    "message:assistant": 4.76, "additional_tools": 4.5,
}
EST_FLAT = {"compaction": 1500, "reasoning": 60, "compaction_trigger": 10}


def usd(uncached, cached=0, output=0):
    return (uncached * PRICE["input"] + cached * PRICE["cached"] + output * PRICE["output"]) / 1e6


# --------------------------------------------------------------------------
# shell command classification -> (fine label, purpose "top/sub")

STR_RE = r"\"(?:[^\"\\]|\\.)*\"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`"
ANY_STR_RE = re.compile(STR_RE)
CMD_EXPR_RE = re.compile(r"\bcmd\s*:\s*((?:" + STR_RE + r"|[A-Za-z_$][\w$]*|\s*\+\s*)+)")
CONST_RE = re.compile(r"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(" + STR_RE + ")")
TOOLS_RE = re.compile(r"\btools\.([A-Za-z_][A-Za-z0-9_]*)\s*\(")
CHARS_RE = re.compile(r"\bchars\s*:\s*(" + STR_RE + ")")
SESSION_ARG_RE = re.compile(r"\bsession_id\s*:\s*\"?(\d+)")
CELL_ARG_RE = re.compile(r"\"cell_id\"\s*:\s*\"?([\w-]+)")
CELL_OUT_RE = re.compile(r"Script running with cell ID\s*\"?([\w-]+)")
SESSION_OUT_RE = re.compile(r"\"session_id\"\s*:\s*(\d+)|SESSION:(\d+)")

VCS_DIFF = {"diff", "show", "file", "interdiff", "evolog", "cat"}
VCS_LOG = {"log", "status", "st", "bookmark", "op", "workspace", "root", "rev-parse", "branch", "remote", "config", "resolve"}
CARGO_BUILD = {"build", "check", "fmt", "clippy", "doc", "metadata", "tree", "update", "fetch", "install", "run", "expand", "vendor"}
CARGO_TEST = {"test", "nextest", "insta", "t"}
WAIT_WORDS = {"sleep", "while", "for", "until", "break", "exit", "if", "test", "true", "false", "read", "kill", "wait"}
SKIP_WORDS = {"cd", "printf", "echo", "then", "do", "fi", "done", "else", "export", "set", "local", "shift", "elif", "esac", "case", "in", "return"}
SSH_OPTS_WITH_ARG = {"-i", "-o", "-J", "-F", "-p", "-S", "-l", "-L", "-R", "-D", "-W", "-b", "-c", "-e", "-m", "-O", "-Q", "-w", "-E", "-I", "-B"}
RG_OPTS_WITH_ARG = {"-g", "--glob", "-t", "--type", "-T", "--type-not", "-A", "-B", "-C", "-m", "--max-count", "--iglob", "-e", "--regexp", "-f", "--file", "-r", "--replace", "--max-depth", "-d", "--sort", "--color", "--context", "-M", "--max-columns"}
PURPOSE_PRIORITY = ["test", "build", "remote", "script", "web", "proc", "vcs", "edit", "search", "read", "comms", "image", "misc", "text", "wait"]


def js_string(lit):
    body = lit[1:-1]
    if lit[0] == '"':
        try:
            return json.loads(lit)
        except Exception:
            pass
    return re.sub(r"\\(.)", lambda m: {"n": "\n", "t": "\t"}.get(m.group(1), m.group(1)), body)


def js_expr_string(expr, consts):
    """Evaluate `a + "b" + `c${d}`` style concatenations with known string consts."""
    out = []
    for tok in re.findall(STR_RE + r"|[A-Za-z_$][\w$]*", expr):
        if tok[0] in "\"'`":
            val = js_string(tok)
            if tok[0] == "`":
                val = re.sub(r"\$\{([A-Za-z_$][\w$]*)\}", lambda m: consts.get(m.group(1), m.group(0)), val)
            out.append(val)
        elif tok in consts:
            out.append(consts[tok])
    return "".join(out)


def split_shell(cmd):
    """Split on ; | || && and newlines outside quotes; ignores $(...) nesting."""
    segs, cur, q, i = [], [], None, 0
    while i < len(cmd):
        c = cmd[i]
        if q:
            cur.append(c)
            if c == "\\" and q != "'" and i + 1 < len(cmd):
                cur.append(cmd[i + 1]); i += 1
            elif c == q:
                q = None
        elif c in "'\"":
            q = c; cur.append(c)
        elif c == "\\" and i + 1 < len(cmd):
            cur.append(cmd[i + 1]); i += 1
        elif c in ";\n|&" and not (c in "&|" and cur and cur[-1] in "<>"):
            if c in "|&" and i + 1 < len(cmd) and cmd[i + 1] == c:
                i += 1
            segs.append("".join(cur)); cur = []
        else:
            cur.append(c)
        i += 1
    segs.append("".join(cur))
    return [x for x in segs if x.strip()]


def rg_purpose(words):
    listing, pattern, i = False, None, 1
    while i < len(words):
        a = words[i]
        if a in ("--files", "-l", "--files-with-matches", "-c", "--count", "--count-matches", "--files-without-match"):
            listing = True
        elif a in ("-e", "--regexp"):
            pattern = words[i + 1] if i + 1 < len(words) else pattern
            i += 1
        elif a in RG_OPTS_WITH_ARG:
            i += 1
        elif a.startswith("-") and len(a) > 1:
            pass
        elif pattern is None:
            pattern = a
        i += 1
    if listing:
        return "search/files"
    if pattern is None:
        return "search/text"
    alts = [x.strip("()^$\\b") for x in pattern.split("|")]
    if all(re.fullmatch(r"[\w:.]+", x) for x in alts if x) or re.search(
            r"\b(fn|struct|enum|impl|trait|pub|mod|type|const|static|use|let|async|macro_rules|derive)\b", pattern):
        return "search/symbol"
    return "search/text"


def classify_segment(seg, raw_cmd):
    """One top-level shell segment -> (fine, purpose) or None."""
    try:
        words = shlex.split(seg, posix=True)
    except ValueError:
        words = seg.strip().split()
    while words and ("=" in words[0] and not words[0].startswith("-") or words[0] in {"sudo", "env", "nohup", "time", "exec", "(", "!", "{", "setsid", "command"}):
        words = words[1:]
    if words and words[0] == "timeout":
        words = words[2:]
    if not words:
        return None
    w = os.path.basename(words[0].lstrip("({").rstrip(")}"))
    if w in SKIP_WORDS or not re.fullmatch(r"[A-Za-z][A-Za-z0-9_.+-]*", w):
        return None
    sub = words[1] if len(words) > 1 and re.fullmatch(r"[a-z][a-z0-9-]*", words[1]) else ""
    if w in ("rg", "grep", "ag"):
        return (w, rg_purpose(words))
    if w == "sed":
        if any(a.startswith("-i") for a in words[1:3]):
            return ("sed-i", "edit/sed")
        return ("sed", "read/range")
    if w in ("cat", "nl", "head", "tail", "bat", "od", "tac", "less", "more"):
        return (w, "read/file")
    if w in ("find", "fd", "ls", "tree", "locate"):
        return (w, "search/files")
    if w in ("wc", "stat", "sort", "uniq", "cut", "tr", "awk", "jq", "sha256sum", "readlink", "realpath", "du", "df", "which", "pwd", "diff", "cmp", "comm", "column", "md5sum", "file", "strings", "basename", "dirname", "date", "seq", "xargs", "tee", "yes", "expr", "bc", "hostname", "whoami", "id", "uname", "nproc"):
        return (w, "search/other")
    if w in ("jj", "git", "gh"):
        if sub in VCS_DIFF:
            return (f"{w}:{sub}", "vcs/diff")
        if sub in VCS_LOG:
            return (f"{w}:{sub}", "vcs/log")
        return (f"{w}:{sub or '?'}", "vcs/write")
    if w == "cargo":
        if sub in CARGO_TEST:
            return (f"cargo:{sub}", "test/cargo")
        return (f"cargo:{sub or '?'}", f"build/{sub or 'other'}" if sub in CARGO_BUILD else "build/other")
    if w in ("just", "nix", "nix-build", "nix-shell", "treefmt", "rustfmt", "rustc", "make", "npm", "rustup", "wasm-pack", "trunk", "nixos-rebuild", "cc", "gcc", "clang", "ld", "mold"):
        return (f"{w}:{sub}" if sub else w, f"build/{w}")
    if w in ("python3", "python", "perl", "ruby", "node", "bash", "sh", "zsh"):
        edits = re.search(r"write_text\(|\.write\(|open\([^)]*['\"][wa]|re\.sub\(|replace\(|-pi|-i\b", raw_cmd)
        return (w, "script/edit" if edits else "script/analyze")
    if w in ("apply_patch", "patch"):
        return (w, "edit/patch")
    if w in WAIT_WORDS:
        return (w, "wait/sleep")
    if w == "ssh":
        return classify_ssh(words)
    if w in ("rsync", "scp", "sftp"):
        return (w, "remote/copy")
    if w in ("tailscale", "ping", "nc", "curl", "wget", "dig", "ip", "ss", "networkctl", "iw", "nmcli"):
        return (w, "remote/net")
    if w in ("rho", "rho-daemon", "rho-gui", "rho-cli"):
        return (f"{w}:{sub}" if sub else w, "proc/rho")
    if w in ("ps", "kill", "pkill", "pgrep", "fuser", "lsof", "systemctl", "journalctl", "swaymsg", "hyprctl", "loginctl"):
        return (w, "proc/manage")
    if w in ("rm", "cp", "mv", "mkdir", "touch", "chmod", "chown", "ln", "rmdir", "sync", "mktemp", "tar", "gzip", "unzip", "zip", "install", "dd", "truncate"):
        return (w, "proc/fs")
    if w in ("agent-browser", "chromium", "firefox", "xdotool", "wtype", "grim", "slurp", "wl-copy", "wl-paste"):
        return (w, "web/browser")
    return (w, f"misc/{w}")


def classify_ssh(words):
    i, host, rest = 1, None, []
    while i < len(words):
        a = words[i]
        if a in SSH_OPTS_WITH_ARG:
            i += 2
            continue
        if a.startswith("-"):
            i += 1
            continue
        host, rest = a, words[i + 1:]
        break
    if host is None:
        return ("ssh", "remote/other")
    hl = "phone" if re.search(r"172\.16\.42\.\d+|root@", host) else host.split("@")[-1]
    remote = " ".join(rest)
    subs = classify_commands(remote, remote) if remote else []
    if not subs:
        return (f"ssh[{hl}]", "remote/shell")
    purposes = sorted({p.split("/")[0] for _, p in subs})
    purposes = [p for p in purposes if p != "wait"] or purposes
    fine = f"ssh[{hl}]:" + "+".join(sorted({f for f, _ in subs}))[:40]
    return (fine, "remote/" + ("+".join(purposes) if len(purposes) <= 2 else "mixed"))


def classify_commands(cmd, raw_cmd):
    """Whole shell string -> list of (fine, purpose)."""
    body = re.sub(r"<<-?\s*'?\"?(\w+)'?\"?.*?\n\1\b", "<<HEREDOC", cmd, flags=re.S)
    out = []
    for seg in split_shell(body):
        r = classify_segment(seg, raw_cmd)
        if r:
            out.append(r)
    return out


def categorize_call(kind, name, src):
    """Tool call -> dict(fine, purposes:set, ntools, parallel, poll, session_id, cell_id)."""
    d = dict(fine="", purposes=set(), ntools=0, parallel=False, poll=None, session_id=None, cell_id=None)
    if kind == "function_call":
        d["fine"], d["ntools"] = name, 1
        if name == "wait":
            m = CELL_ARG_RE.search(src)
            d["cell_id"] = m.group(1) if m else None
            d["poll"] = "wait"
            d["purposes"] = {"wait/cell"}
        elif name in ("message_agent", "ask_advisor", "wait_agent", "spawn_engineer", "interrupt_engineer", "list_agents"):
            d["purposes"] = {f"comms/{name}"}
        else:
            d["purposes"] = {f"misc/{name}"}
        return d
    nested = TOOLS_RE.findall(src)
    d["parallel"] = "Promise.all" in src
    d["ntools"] = len(nested)
    if not nested:
        d["fine"], d["purposes"] = "exec:text-only", {"text/text"}
        return d
    consts = {m.group(1): js_string(m.group(2)) for m in CONST_RE.finditer(src)}
    cmd_strings = [js_expr_string(m.group(1), consts) for m in CMD_EXPR_RE.finditer(src)]
    cmd_strings = [c for c in cmd_strings if c.strip()]
    if not cmd_strings and "exec_command" in nested:
        cmd_strings = [js_string(l) for l in ANY_STR_RE.findall(src)]
        cmd_strings = [c for c in cmd_strings if classify_commands(c, c)]
    fines = []
    cmd_iter = iter(cmd_strings)
    for tool in nested:
        if tool == "exec_command":
            cmds = [c for c in [next(cmd_iter, None)] if c is not None]
            if nested.count("exec_command") == 1:
                cmds += list(cmd_iter)
            subs = []
            for c in cmds:
                subs += classify_commands(c, c)
            if subs:
                fines.append("+".join(sorted({f for f, _ in subs})))
                d["purposes"].update(p for _, p in subs)
            else:
                fines.append("exec_command:?")
                d["purposes"].add("misc/unparsed")
        elif tool == "write_stdin":
            m = CHARS_RE.search(src)
            chars = js_string(m.group(1)) if m else "?"
            sm = SESSION_ARG_RE.search(src)
            d["session_id"] = sm.group(1) if sm else None
            if chars == "":
                fines.append("write_stdin:poll")
                d["poll"] = "poll"
                d["purposes"].add("wait/poll")
            else:
                fines.append("write_stdin")
                d["purposes"].add("proc/stdin")
        elif tool == "apply_patch":
            fines.append("apply_patch")
            d["purposes"].add("edit/patch")
        elif tool in ("message_agent", "ask_advisor", "wait_agent", "spawn_engineer"):
            fines.append(tool)
            d["purposes"].add(f"comms/{tool}")
        elif tool == "view_image":
            fines.append(tool)
            d["purposes"].add("image/view")
        elif tool.startswith("web"):
            fines.append(tool)
            d["purposes"].add("web/run")
        else:
            fines.append(tool)
            d["purposes"].add(f"misc/{tool}")
    d["fine"] = "+".join(fines)
    only_wait = all(p.startswith("wait/") for p in d["purposes"])
    if only_wait and d["poll"] is None:
        d["poll"] = "sleep"
    if not only_wait:
        d["purposes"] = {p for p in d["purposes"] if not p.startswith("wait/")}
    return d


def primary_purpose(purposes):
    """Pick one purpose label from a set, by priority of top-level."""
    tops = sorted(purposes, key=lambda p: PURPOSE_PRIORITY.index(p.split("/")[0]) if p.split("/")[0] in PURPOSE_PRIORITY else 99)
    return tops[0] if tops else "misc/?"


# --------------------------------------------------------------------------
# session loading

PREFIX_KIND = {
    "at": {"additional_tools"}, "msg": {"message", "compaction_trigger"}, "rs": {"reasoning"},
    "ctc": {"custom_tool_call"}, "ctco": {"custom_tool_call_output"},
    "fc": {"function_call", "function_call_output"}, "fco": {"function_call_output"},
    "cmp": {"compaction"},
}


def load_json(path):
    with open(path) as f:
        return json.load(f)


_INDEX = None


def session_index():
    """stem -> [request count, latest mtime]; one directory scan per process."""
    global _INDEX
    if _INDEX is None:
        _INDEX = list_sessions()
    return _INDEX


def list_sessions():
    stems = {}
    with os.scandir(DIR) as it:
        for e in it:
            n = e.name
            if not n.endswith("-request.json"):
                continue
            s = stems.setdefault(n[:16], [0, 0.0])
            s[0] += 1
            s[1] = max(s[1], e.stat().st_mtime)
    return stems


def session_meta(stem):
    p = f"{DIR}/{stem}-0001-request.json"
    if not os.path.exists(p):
        return ("?", "?")
    d = load_json(p)
    role = "?"
    for i in d["body"]["input"]:
        if i.get("role") == "developer" and i.get("type") == "message":
            role = i["content"][0]["text"][:50].replace("\n", " ")
    return (d.get("model") or d["body"].get("model") or "?", role)


def item_kind(item):
    t = item.get("type")
    if t is None and item.get("role") == "user":
        return "message"
    return t


def item_text(item):
    t = item_kind(item)
    if t == "message":
        parts = item.get("content") or []
        if isinstance(parts, str):
            return parts
        return "\n".join(p.get("text", "") for p in parts if isinstance(p, dict))
    if t == "additional_tools":
        return json.dumps(item.get("tools"))
    if t in ("custom_tool_call_output", "function_call_output"):
        o = item.get("output")
        return o if isinstance(o, str) else json.dumps(o)
    if t == "custom_tool_call":
        return item.get("input", "")
    if t == "function_call":
        return f"{item.get('name')}({item.get('arguments', '')})"
    if t == "reasoning":
        return "\n".join(s.get("text", "") for s in item.get("summary", []))
    if t in ("compaction", "compaction_trigger"):
        return f"<{t}>"
    return json.dumps(item)[:2000]


def estimate_tokens(block):
    kind = block["kind"]
    if kind in EST_FLAT:
        return EST_FLAT[kind]
    key = f"message:{block['role']}" if kind == "message" else kind
    return max(1, int(block["tlen"] / EST_RATIO.get(key, 4.0)))


class Session:
    def __init__(self, stem):
        self.stem, self.blocks, self.requests, self.meta = stem, [], [], {}

    def to_json(self):
        return {"stem": self.stem, "blocks": self.blocks, "requests": self.requests, "meta": self.meta}

    @staticmethod
    def from_json(d):
        s = Session(d["stem"])
        s.blocks, s.requests, s.meta = d["blocks"], d["requests"], d.get("meta", {})
        return s


class Origins:
    """Maps running sessions/cells back to the call that started them."""

    def __init__(self):
        self.by_call_id = {}   # call_id -> (fine, purpose)
        self.session = {}      # shell session id -> (fine, purpose)
        self.cell = {}         # exec cell id -> (fine, purpose)
        self.last_nonwait = None


def make_block(item, seq, org, role_default=None):
    kind = item_kind(item)
    role = item.get("role", role_default)
    text = item_text(item)
    fine, purpose, ntools, parallel, poll_out, tpurpose = "", "", 0, False, False, None
    if kind in ("custom_tool_call", "function_call"):
        d = categorize_call(kind, item.get("name"), item.get("input") or item.get("arguments") or "")
        fine, ntools, parallel = d["fine"], d["ntools"], d["parallel"]
        purpose = primary_purpose(d["purposes"])
        if d["poll"]:
            origin = None
            if d["poll"] == "poll" and d["session_id"]:
                origin = org.session.get(d["session_id"])
            elif d["poll"] == "wait" and d["cell_id"]:
                origin = org.cell.get(d["cell_id"])
            if origin is None:
                origin = org.last_nonwait
            if origin:
                ofine, opurpose = origin
                purpose = f"wait/{opurpose.replace('/', '.')}"
                fine = f"{fine}<-{ofine[:40]}"
            else:
                purpose = "wait/unknown"
        else:
            org.last_nonwait = (fine, purpose)
        org.by_call_id[item.get("call_id")] = (fine, purpose)
        cat, pcat = "call:" + fine, "call:" + purpose
    elif kind in ("custom_tool_call_output", "function_call_output"):
        fine, purpose = org.by_call_id.get(item.get("call_id"), ("?", "misc/?"))
        tpurpose = purpose  # trip purpose stays wait/...; occupancy uses the origin
        if purpose.startswith("wait/") and purpose != "wait/unknown":
            poll_out = True
            purpose = purpose[5:].replace(".", "/")  # content belongs to what was waited on
        for m in SESSION_OUT_RE.finditer(text):
            sid = m.group(1) or m.group(2)
            if sid not in org.session and not purpose.startswith("wait/"):
                org.session[sid] = (fine, purpose)
        m = CELL_OUT_RE.search(text)
        if m and m.group(1) not in org.cell and not purpose.startswith("wait/"):
            org.cell[m.group(1)] = (fine, purpose)
        cat, pcat = "out:" + fine, "out:" + purpose
    elif kind == "additional_tools":
        cat = pcat = "tools-schema"
    elif kind == "message":
        cat = pcat = {"developer": "system-prompt", "user": "user-msg", "assistant": "assistant-msg"}.get(role, "message")
    else:
        cat = pcat = kind
    return {
        "kind": kind, "role": role, "cat": cat, "pcat": pcat, "text": text[:TEXT_KEEP], "tlen": len(text),
        "seq": seq, "id": item.get("id"), "ntools": ntools, "parallel": parallel, "call_id": item.get("call_id"),
        "poll_out": poll_out, "truncated": "Warning: truncated output" in text[:200], "tpurpose": tpurpose,
        "orig_tokens": int((re.search(r"original token count: (\d+)", text[:200]) or [0, 0])[1]),
    }


def trigger_label(trigger, fresh, seq):
    if fresh:
        if "compaction" in trigger:
            # a real compaction is followed only by the items that triggered it; a resume replays
            # the whole history (hundreds of outputs) after an old compaction item
            after = len(trigger) - 1 - max(i for i, t in enumerate(trigger) if t == "compaction")
            return "compaction" if after <= 5 else "resume"
        return "user" if seq == 1 and len(trigger) <= 1 else "resume"
    return "|".join(sorted(set(trigger))) or "?"  # "|" because purposes may contain "+"


def parse_session(stem, verbose=False):
    s = Session(stem)
    s.meta["model"], s.meta["role"] = session_meta(stem)
    s.meta["mtime"] = session_index().get(stem, [0, 0])[1]
    count = session_index().get(stem, [0, 0])[0]
    seqs, seq, misses = [], 1, 0
    while len(seqs) < count and misses < 50:
        if os.path.exists(f"{DIR}/{stem}-{seq:04d}-request.json"):
            seqs.append(seq)
            misses = 0
        else:
            misses += 1
        seq += 1
    context, org, known_tokens = [], Origins(), {}
    last_completed, prev_context_set = None, set()
    for seq in seqs:
        rq_path = f"{DIR}/{stem}-{seq:04d}-request.json"
        rs_path = f"{DIR}/{stem}-{seq:04d}-response.json"
        rq = load_json(rq_path)
        body = rq["body"]
        fresh = not body.get("previous_response_id")
        req_mtime = os.stat(rq_path).st_mtime
        if fresh:
            context = []
        new_blocks, trigger = [], []
        for item in body.get("input", []):
            b = make_block(item, seq, org)
            if b["kind"] in ("custom_tool_call_output", "function_call_output"):
                trigger.append(b["tpurpose"])
            elif b["kind"] == "message" and b["role"] == "user":
                trigger.append("user")
            elif b["kind"] == "compaction":
                trigger.append("compaction")
            s.blocks.append(b)
            idx = len(s.blocks) - 1
            new_blocks.append(idx)
            context.append(idx)
        req = {
            "seq": seq, "fresh": fresh, "req_time": req_mtime, "created": None, "completed": None,
            "error": None, "new": new_blocks, "outputs": [], "attr": [], "usage": None,
            "trigger": trigger_label(trigger, fresh, seq), "aligned": False, "estimated": False,
            "gap": (req_mtime - last_completed) if last_completed else None,
            "ctx_blocks": len(context), "cache_miss": 0,
        }
        s.requests.append(req)
        if not os.path.exists(rs_path):
            req["error"] = "missing response"
            continue
        rs = load_json(rs_path)
        if rs.get("error"):
            req["error"] = str(rs["error"])[:200]
        completed, done_items = None, []
        for e in rs.get("raw_events", []):
            t = e.get("type")
            if t == "response.output_item.done":
                done_items.append(e["item"])
            elif t == "response.completed":
                completed = e["response"]
        if completed is None:
            continue
        if not done_items:
            done_items = completed.get("output", [])
        req["created"] = completed.get("created_at")
        req["completed"] = completed.get("completed_at")
        last_completed = req["completed"] or req["created"]
        usage = completed.get("usage") or {}
        u = req["usage"] = {
            "input": usage.get("input_tokens", 0),
            "cached": (usage.get("input_tokens_details") or {}).get("cached_tokens", 0),
            "output": usage.get("output_tokens", 0),
            "reasoning": (usage.get("output_tokens_details") or {}).get("reasoning_tokens", 0),
        }
        out_blocks = []
        for item in done_items:
            b = make_block(item, seq, org, role_default="assistant")
            s.blocks.append(b)
            out_blocks.append(len(s.blocks) - 1)
        req["outputs"] = out_blocks
        expected = context + out_blocks
        attr_items = list(((usage.get("attribution") or {}).get("items") or {}).items())
        aligned = bool(attr_items) and len(attr_items) == len(expected)
        if aligned:
            for (aid, _), idx in zip(attr_items, expected):
                if s.blocks[idx]["kind"] not in PREFIX_KIND.get(aid.split("_")[0], set()):
                    aligned = False
                    break
        if aligned:
            req["aligned"] = True
            for (aid, a), idx in zip(attr_items, expected):
                if s.blocks[idx]["id"] is None:
                    s.blocks[idx]["id"] = aid
                inp, cached, out = a.get("input_tokens", 0), a.get("cached_tokens", 0), a.get("output_tokens", 0)
                req["attr"].append([idx, inp, cached, out])
                if inp or out:
                    known_tokens[idx] = inp or out
        else:
            if attr_items and verbose:
                print(f"  misaligned seq {seq}: attr {len(attr_items)} vs ctx {len(expected)}", file=sys.stderr)
            req["estimated"] = True
            rs_items = [i for i in out_blocks if s.blocks[i]["kind"] == "reasoning"]
            other = [i for i in out_blocks if s.blocks[i]["kind"] != "reasoning"]
            for i in rs_items:
                known_tokens[i] = max(1, u["reasoning"] // max(1, len(rs_items)))
            other_chars = sum(s.blocks[i]["tlen"] for i in other) or 1
            for i in other:
                known_tokens[i] = max(1, int((u["output"] - u["reasoning"]) * s.blocks[i]["tlen"] / other_chars))
            est = [known_tokens.get(i) or estimate_tokens(s.blocks[i]) for i in context]
            scale = u["input"] / (sum(est) or 1)
            toks = [max(1, int(t * scale)) for t in est]
            uncached_pool = max(0, u["input"] - u["cached"])
            unc = [0] * len(context)
            order = [k for k, i in enumerate(context) if i not in prev_context_set]
            order += [k for k in reversed(range(len(context))) if context[k] in prev_context_set]
            for k in order:
                take = min(toks[k], uncached_pool)
                unc[k] = take
                uncached_pool -= take
                if uncached_pool <= 0:
                    break
            for k, i in enumerate(context):
                req["attr"].append([i, toks[k], toks[k] - unc[k], 0])
            for i in out_blocks:
                req["attr"].append([i, 0, 0, known_tokens[i]])
        miss = sum(inp - cached for idx, inp, cached, _ in req["attr"] if idx in prev_context_set and inp > cached)
        req["cache_miss"] = miss if miss > 512 else 0
        prev_context_set = set(context)
        context = context + out_blocks
    return s


def cache_path(stem):
    n = session_index().get(stem, [0, 0])[0]
    return f"{CACHE}/{stem}-{n}-v{CACHE_VERSION}.json.gz"


def load_session(stem, refresh=False, verbose=False):
    os.makedirs(CACHE, exist_ok=True)
    cp = cache_path(stem)
    if not refresh and os.path.exists(cp):
        with gzip.open(cp, "rt") as f:
            return Session.from_json(json.load(f))
    t0 = time.time()
    s = parse_session(stem, verbose)
    with gzip.open(cp, "wt") as f:
        json.dump(s.to_json(), f)
    if verbose:
        print(f"  parsed {stem}: {len(s.requests)} requests in {time.time()-t0:.1f}s", file=sys.stderr)
    return s


# --------------------------------------------------------------------------
# aggregation (per session in workers, merged in the parent)


def block_purpose(b):
    p = b["pcat"]
    for prefix in ("out:", "call:"):
        if p.startswith(prefix):
            return p[len(prefix):]
    return p


def trip_purpose(trigger):
    if trigger in ("user", "compaction", "resume", "?"):
        return trigger
    ps = {p for p in trigger.split("|") if p and p != "user"}
    if not ps:
        return "user"
    return primary_purpose(ps)


def add(d, k, *vals):
    cur = d.get(k)
    if cur is None:
        d[k] = list(vals)
    else:
        for i, v in enumerate(vals):
            cur[i] += v


def hosts_of(cat):
    out = set()
    for m in re.finditer(r"ssh\[([^\]]+)\]", cat):
        out.update(h.strip() for h in m.group(1).split(",") if h.strip())
    return out


def aggregate_session(stem):
    s = load_session(stem)
    A = dict(occ={}, occ_fine={}, rt={}, rt_fine={}, out={}, out_fine={}, miss={}, tot={},
             top_blocks=[], reasoning={}, gaps={}, trunc={}, host={}, host_occ={}, remote_fine={}, sess=None)
    tot = collections.Counter()
    block_occ = collections.defaultdict(float)
    last_user_seq = 0
    for r in s.requests:
        tot["requests"] += 1
        tot["errors"] += bool(r["error"])
        tot["fresh"] += bool(r["fresh"])
        u = r["usage"]
        if not u:
            continue
        for i in r["new"]:
            if s.blocks[i]["cat"] == "user-msg":
                last_user_seq = max(last_user_seq, s.blocks[i]["seq"])
        uncached = u["input"] - u["cached"]
        in_usd, out_usd = usd(uncached, u["cached"]), usd(0, 0, u["output"])
        tot["usd"] += in_usd + out_usd
        tot["input_usd"] += in_usd
        tot["output_usd"] += out_usd
        tot["tokens_in"] += u["input"]
        tot["tokens_cached"] += u["cached"]
        tot["tokens_out"] += u["output"]
        tot["reasoning_out"] += u["reasoning"]
        tot["aligned"] += bool(r["aligned"])
        tot["estimated"] += bool(r["estimated"])
        tp = trip_purpose(r["trigger"])
        if tp.startswith("wait"):
            tot["wait_usd"] += in_usd
        new_set = set(r["new"])
        new_tokens = 0
        rs_tokens = rs_items = rs_old_usd = 0.0
        for idx, inp, cached, o in r["attr"]:
            b = s.blocks[idx]
            if inp:
                c = usd(inp - cached, cached)
                p = block_purpose(b)
                add(A["occ"], p, c, inp - cached, cached)
                add(A["occ_fine"], b["cat"], c, inp - cached, cached)
                block_occ[idx] += c
                if b.get("poll_out"):
                    tot["poll_out_usd"] += c
                if b.get("truncated"):
                    tot["truncated_usd"] += c
                    add(A["trunc"], p, c, 0, 0)
                for h in hosts_of(b["cat"]):
                    add(A["host_occ"], h, c)
                if b["kind"] == "reasoning":
                    rs_tokens += inp
                    rs_items += 1
                    if b["seq"] < last_user_seq:
                        rs_old_usd += c
            if idx in new_set:
                new_tokens += inp
            if o:
                add(A["out"], block_purpose(b), usd(0, 0, o), o)
                add(A["out_fine"], b["cat"], usd(0, 0, o), o)
        add(A["reasoning"], "all", rs_tokens, rs_items, u["input"], 1)
        tot["reasoning_old_usd"] += rs_old_usd
        for i in ([] if r["fresh"] else r["new"]):  # fresh contexts replay history; not "new" work
            nb = s.blocks[i]
            if nb.get("truncated"):
                tot["truncated_outputs"] += 1
                add(A["trunc"], block_purpose(nb), 0, 1, nb.get("orig_tokens", 0))
            if nb["cat"].startswith("out:"):
                hs = hosts_of(nb["cat"])
                for h in hs:
                    add(A["host"], f"{h} {'via wait' if tp.startswith('wait') else 'direct'}", in_usd, 1)
                if hs and nb["pcat"].startswith("out:remote") and not nb.get("poll_out"):
                    add(A["remote_fine"], re.sub(r"ssh\[[^\]]+\]:?", "", nb["cat"][4:])[:60] or "(shell)", in_usd, 1)
        if r["cache_miss"]:
            add(A["miss"], tp, usd(r["cache_miss"]), 1)
            tot["miss_usd"] += usd(r["cache_miss"])
        add(A["rt"], tp, in_usd, 1, new_tokens)
        add(A["rt_fine"], r["trigger"], in_usd, 1, new_tokens)
        if r["gap"] is not None:
            gb = "<5s" if r["gap"] < 5 else "5-30s" if r["gap"] < 30 else "30s-5m" if r["gap"] < 300 else ">5m"
            add(A["gaps"], f"{tp.split('/')[0]} {gb}", in_usd, 1)
        calls = [s.blocks[i] for i in r["outputs"] if s.blocks[i]["kind"] in ("custom_tool_call", "function_call")]
        if calls:
            n = sum(b["ntools"] for b in calls)
            tot["responses_with_calls"] += 1
            tot["nested_calls"] += n
            tot["single_nested"] += n == 1
            tot["parallel_promise"] += any(b["parallel"] for b in calls)
    A["tot"] = dict(tot)
    A["top_blocks"] = [(v, s.stem, idx, s.blocks[idx]["cat"], s.blocks[idx]["seq"], s.blocks[idx]["text"][:80].replace("\n", " "))
                       for idx, v in sorted(block_occ.items(), key=lambda kv: -kv[1])[:40]]
    A["sess"] = (s.stem, s.meta, tot["usd"], tot["requests"], tot["tokens_in"], tot["wait_usd"], tot["estimated"])
    return A


def merge(aggs):
    M = dict(occ={}, occ_fine={}, rt={}, rt_fine={}, out={}, out_fine={}, miss={}, tot=collections.Counter(),
             top_blocks=[], reasoning={}, gaps={}, trunc={}, host={}, host_occ={}, remote_fine={}, sessions=[])
    for A in aggs:
        for key in ("occ", "occ_fine", "rt", "rt_fine", "out", "out_fine", "miss", "reasoning", "gaps", "trunc", "host", "host_occ", "remote_fine"):
            for k, v in A[key].items():
                add(M[key], k, *v)
        M["tot"].update(A["tot"])
        M["top_blocks"] += A["top_blocks"]
        M["sessions"].append(A["sess"])
    M["top_blocks"].sort(reverse=True)
    return M


def rollup(d, depth):
    """Sum purpose labels to `depth` levels ('wait/test.cargo' -> 'wait' at depth 1)."""
    out = {}
    for k, v in d.items():
        key = "/".join(k.split("/")[:depth])
        add(out, key, *v)
    return out


def print_purpose_table(title, d, total, top, cols, fmt):
    print(f"\n== {title}")
    print("   " + "  ".join(f"{c:>{w}}" for c, w in cols) + "  purpose")
    top1 = rollup(d, 1)
    top2 = rollup(d, 2)
    for k1, v1 in sorted(top1.items(), key=lambda kv: -kv[1][0])[:top]:
        print("   " + "  ".join(f"{x:>{w}}" for x, (_, w) in zip(fmt(v1, total), cols)) + f"  {k1}")
        subs = [(k, v) for k, v in top2.items() if k.split("/")[0] == k1 and "/" in k]
        if len(subs) > 1 or (subs and subs[0][0] != k1):
            for k2, v2 in sorted(subs, key=lambda kv: -kv[1][0])[:8]:
                print("   " + "  ".join(f"{x:>{w}}" for x, (_, w) in zip(fmt(v2, total), cols)) + f"      {k2.split('/', 1)[1]}")


def print_table(title, rows, cols, top):
    print(f"\n== {title}")
    print("   " + "  ".join(f"{c:>{w}}" for c, w in cols[1:]) + "  " + cols[0][0])
    for row in rows[:top]:
        print("   " + "  ".join(f"{v:>{w}}" for v, (_, w) in zip(row[1:], cols[1:])) + "  " + str(row[0])[:100])


def report(M, top=25):
    T = M["tot"]
    total = T["usd"] or 1e-9
    n_sessions = len(M["sessions"])
    print(f"== totals: ${T['usd']:.2f}  input ${T['input_usd']:.2f}  output ${T['output_usd']:.2f}   ({n_sessions} sessions)")
    print(f"   requests {T['requests']}  exact-attributed {T['aligned']}  estimated {T['estimated']}  errors {T['errors']}  fresh-contexts {T['fresh']}")
    hit = T["tokens_cached"] / max(1, T["tokens_in"])
    print(f"   input tokens {T['tokens_in']/1e6:.1f}M  cached {hit*100:.1f}%  avg context {T['tokens_in']/max(1,T['requests'])/1e3:.0f}k  output tokens {T['tokens_out']/1e3:.0f}k (reasoning {T['reasoning_out']/1e3:.0f}k)")
    print(f"   cache-miss re-reads ${T['miss_usd']:.2f} ({T['miss_usd']/total*100:.1f}%)")
    if T["responses_with_calls"]:
        print(f"   tool responses {T['responses_with_calls']}: single nested call {T['single_nested']/T['responses_with_calls']*100:.0f}%, Promise.all {T['parallel_promise']} ({T['parallel_promise']/T['responses_with_calls']*100:.1f}%), avg nested/response {T['nested_calls']/T['responses_with_calls']:.2f}")
    r = M["reasoning"].get("all", [0, 0, 0, 1])
    print(f"   reasoning items in context: avg {r[1]/max(1,r[3]):.0f} items / {r[0]/max(1,r[3])/1e3:.1f}k tokens per request = {r[0]/max(1,r[2])*100:.1f}% of context; from turns before the latest user message: ${T['reasoning_old_usd']:.2f} ({T['reasoning_old_usd']/total*100:.1f}%)")

    in_total = T["input_usd"] or 1e-9
    out_total = T["output_usd"] or 1e-9
    print_purpose_table("OCCUPANCY: input cost of blocks sitting in context, by purpose (sums to input bill)", M["occ"], in_total, top,
                        [("usd", 9), ("share", 6), ("tokens", 8)],
                        lambda v, t: (f"${v[0]:.2f}", f"{v[0]/t*100:.1f}%", f"{(v[1]+v[2])/1e6:.0f}M"))
    print_purpose_table("ROUND-TRIP: input cost of each request, charged to what triggered it (sums to input bill)", M["rt"], in_total, top,
                        [("usd", 9), ("share", 6), ("trips", 7), ("$/trip", 7), ("new-tok", 7)],
                        lambda v, t: (f"${v[0]:.2f}", f"{v[0]/t*100:.1f}%", f"{v[1]:.0f}", f"${v[0]/max(1,v[1]):.3f}", f"{v[2]/max(1,v[1]):.0f}"))
    print_purpose_table("OUTPUT: output cost by purpose of the produced block (sums to output bill)", M["out"], out_total, top,
                        [("usd", 9), ("share", 6), ("tokens", 8)],
                        lambda v, t: (f"${v[0]:.2f}", f"{v[0]/t*100:.1f}%", f"{v[1]/1e3:.0f}k"))

    rows = sorted(((k, f"${v[0]:.2f}", f"{v[0]/in_total*100:.1f}%", f"{(v[1]+v[2])/1e6:.0f}M") for k, v in M["occ_fine"].items()), key=lambda r: -float(r[1][1:]))
    print_table("occupancy by fine label", rows, [("label", 0), ("usd", 9), ("share", 6), ("tokens", 7)], top)
    rows = sorted(((k, f"${v[0]:.2f}", f"{v[0]/in_total*100:.1f}%", v[1], f"${v[0]/max(1,v[1]):.3f}", f"{v[2]/max(1,v[1]):.0f}") for k, v in M["rt_fine"].items()), key=lambda r: -float(r[1][1:]))
    print_table("round-trip by fine trigger", rows, [("trigger", 0), ("usd", 9), ("share", 6), ("trips", 6), ("$/trip", 7), ("new-tok", 7)], top)
    rows = sorted(((k, f"${v[0]:.2f}", f"{v[0]/in_total*100:.1f}%", v[1]) for k, v in M["gaps"].items()), key=lambda r: -float(r[1][1:]))
    print_table("round-trip cost by trigger purpose and gap since previous response", rows, [("purpose gap", 0), ("usd", 9), ("share", 6), ("trips", 6)], 20)
    rows = sorted(((k, f"${v[0]:.2f}", v[1]) for k, v in M["miss"].items()), key=lambda r: -float(r[1][1:]))
    print_table("cache-miss re-read cost by trigger purpose", rows, [("purpose", 0), ("usd", 9), ("n", 5)], 8)
    rows = sorted(((k, f"${v[0]:.2f}", f"{v[0]/in_total*100:.1f}%", v[1], f"{v[2]/max(1,v[1])/1000:.0f}k") for k, v in M["trunc"].items()), key=lambda r: -float(r[1][1:]))
    print_table("truncated outputs (>10k tokens kept) by purpose: occupancy cost, count, avg original size", rows, [("purpose", 0), ("usd", 9), ("share", 6), ("n", 6), ("orig", 6)], 15)
    rows = sorted(((k, f"${v[0]:.2f}", f"{v[0]/in_total*100:.1f}%", v[1], f"${M['host_occ'].get(k.split(' ')[0], [0])[0]:.2f}") for k, v in M["host"].items()), key=lambda r: -float(r[1][1:]))
    print_table("remote (ssh) round-trips by host; last col = occupancy of that host's outputs", rows, [("host", 0), ("rt usd", 9), ("share", 6), ("trips", 6), ("occ usd", 9)], 15)
    rows = sorted(((k, f"${v[0]:.2f}", f"{v[0]/in_total*100:.1f}%", v[1]) for k, v in M["remote_fine"].items()), key=lambda r: -float(r[1][1:]))
    print_table("remote (ssh) direct round-trips by remote command list", rows, [("remote commands", 0), ("usd", 9), ("share", 6), ("trips", 6)], 25)

    print(f"\n== top individual blocks by occupancy cost")
    for v, stem, idx, cat, seq, snippet in M["top_blocks"][:top]:
        print(f"   ${v:7.2f}  {stem[:6]}#{idx:<6} seq{seq:<5} {cat[:44]:<44} {snippet[:60]}")

    print(f"\n== sessions (top {top} by cost of {n_sessions})")
    print(f"   {'usd':>8} {'req':>5} {'avgctx':>7} {'wait%':>5} {'est%':>4}  stem  when  role")
    for stem, meta, cost, req, tok, wait_usd, est in sorted(M["sessions"], key=lambda x: -x[2])[:top]:
        when = time.strftime("%m-%d", time.localtime(meta.get("mtime", 0)))
        print(f"   ${cost:7.2f} {req:5d} {tok/max(1,req)/1e3:6.0f}k {wait_usd/max(1e-9,cost)*100:4.0f}% {est/max(1,req)*100:3.0f}%  {stem} {when} {meta.get('role','?')[:38]}")

    occ1, rt1 = rollup(M["occ"], 1), rollup(M["rt"], 1)
    g = lambda d, k: d.get(k, [0, 0, 0])[0]
    print(f"\n== counterfactual upper bounds (first-order, do not add across rows)")
    print(f"   no wait/poll/sleep round trips (blocking wait server-side):    ${g(rt1,'wait'):.2f} ({g(rt1,'wait')/total*100:.0f}%)")
    print(f"   evict outputs returned by wait/poll trips (content filed under origin): ${T['poll_out_usd']:.2f} ({T['poll_out_usd']/total*100:.0f}%)")
    print(f"   truncated outputs (hit max_output_tokens): {T['truncated_outputs']} blocks, occupancy ${T['truncated_usd']:.2f} ({T['truncated_usd']/total*100:.0f}%)")
    print(f"   drop reasoning items from turns before the latest user msg:   ${T['reasoning_old_usd']:.2f} ({T['reasoning_old_usd']/total*100:.0f}%)")
    print(f"   drop all reasoning items from context:                        ${g(occ1,'reasoning'):.2f} ({g(occ1,'reasoning')/total*100:.0f}%)")
    for p in ("read", "search", "remote", "vcs", "test", "build", "web"):
        print(f"   halve {p:<7} tool output size / evict old ones:              ${g(occ1,p)/2:.2f} ({g(occ1,p)/2/total*100:.0f}%)")
    print(f"   batch edits two per trip:                                      ${g(rt1,'edit')/2:.2f} ({g(rt1,'edit')/2/total*100:.0f}%)")
    print(f"   batch remote (ssh) commands two per trip:                      ${g(rt1,'remote')/2:.2f} ({g(rt1,'remote')/2/total*100:.0f}%)")
    sp = g(occ1, "system-prompt") + g(occ1, "tools-schema")
    print(f"   system prompt + tools schema occupancy (all of it):            ${sp:.2f} ({sp/total*100:.0f}%)")
    print(f"   compaction summaries occupancy:                                ${g(occ1,'compaction'):.2f} ({g(occ1,'compaction')/total*100:.0f}%)")
    per_trip_1k = 1000 * PRICE["cached"] / 1e6
    print(f"   context size: every 10k tokens of context costs ${per_trip_1k*10:.4f}/trip -> ${per_trip_1k*10*T['requests']:.2f} over these {T['requests']} trips")


def trunc(text, width):
    text = text.replace("\n", "⏎ ")
    return text if len(text) <= width else text[: width - 1] + "…"


def show(s, a=None, b=None, width=110):
    for r in s.requests:
        if a is not None and r["seq"] < a:
            continue
        if b is not None and r["seq"] > b:
            break
        u = r["usage"] or {"input": 0, "cached": 0, "output": 0, "reasoning": 0}
        uncached = u["input"] - u["cached"]
        cost = usd(uncached, u["cached"], u["output"])
        gap = f"gap {r['gap']:.0f}s" if r["gap"] is not None else ""
        lat = f"lat {r['completed']-r['created']}s" if r["completed"] and r["created"] else ""
        flags = ("FRESH " if r["fresh"] else "") + ("ERR " if r["error"] else "") + ("EST " if r["estimated"] else "")
        miss = f"miss {r.get('cache_miss',0)}" if r.get("cache_miss") else ""
        print(f"--- seq {r['seq']}  ${cost:.3f}  in {u['input']} (uncached {uncached}) out {u['output']} (reason {u['reasoning']})  {gap} {lat} {miss} {flags}<= {r['trigger']}")
        attr = {x[0]: x for x in r["attr"]}
        for idx in r["new"]:
            bl = s.blocks[idx]
            t = attr.get(idx)
            print(f"  IN  {(str(t[1])+'t') if t else '?':>7} {bl['pcat'][:26]:<26} {bl['cat'][:30]:<30} {trunc(bl['text'], width)}")
        for idx in r["outputs"]:
            bl = s.blocks[idx]
            t = attr.get(idx)
            print(f"  OUT {(str(t[3])+'t') if t else '?':>7} {bl['pcat'][:26]:<26} {bl['cat'][:30]:<30} {trunc(bl['text'], width)}")
        if r["error"]:
            print(f"  ERROR {r['error']}")


def parse_since(text):
    m = re.fullmatch(r"(\d+)([dhw])", text)
    if not m:
        raise SystemExit("--since expects like 14d, 12h, 2w")
    return int(m.group(1)) * {"d": 86400, "h": 3600, "w": 7 * 86400}[m.group(2)]


def pick_sessions(model, min_requests, since=None, recent=0, exclude=()):
    idx = session_index()
    now = time.time()
    out = []
    for stem, (cnt, mt) in sorted(idx.items(), key=lambda kv: -kv[1][1]):
        if recent and len(out) >= recent:
            break
        if since and mt < now - since:
            break
        if cnt < min_requests or stem in exclude:
            continue
        m, _ = session_meta(stem)
        if model in m:
            out.append(stem)
    return out


def _agg_worker(stem):
    try:
        return (stem, aggregate_session(stem), None)
    except Exception as e:
        return (stem, None, repr(e))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("sessions")
    p.add_argument("--since", default="14d")
    p.add_argument("--model", default="sol")
    p.add_argument("--min-requests", type=int, default=5)
    p = sub.add_parser("analyze")
    p.add_argument("stems", nargs="*")
    p.add_argument("--since", default=None)
    p.add_argument("--recent", type=int, default=0)
    p.add_argument("--model", default="sol")
    p.add_argument("--min-requests", type=int, default=20)
    p.add_argument("--top", type=int, default=25)
    p.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    p.add_argument("--refresh", action="store_true")
    for name in ("show", "req", "blocks", "grep"):
        p = sub.add_parser(name)
        p.add_argument("stem")
        if name == "show":
            p.add_argument("--from", dest="a", type=int)
            p.add_argument("--to", dest="b", type=int)
        if name == "req":
            p.add_argument("seq", type=int)
        if name == "grep":
            p.add_argument("pattern")
        p.add_argument("--top", type=int, default=30)
        p.add_argument("--width", type=int, default=110 if name != "req" else 2000)
    args = ap.parse_args()

    if args.cmd == "sessions":
        idx = session_index()
        for stem in pick_sessions(args.model, args.min_requests, since=parse_since(args.since)):
            n, mt = idx[stem]
            model, role = session_meta(stem)
            print(f"{stem} {n:5d} {time.strftime('%m-%d %H:%M', time.localtime(mt))} {model:<13} {role}")
        return
    if args.cmd == "analyze":
        stems = list(args.stems)
        if args.since or args.recent:
            stems += pick_sessions(args.model, args.min_requests, since=parse_since(args.since) if args.since else None,
                                   recent=args.recent, exclude=set(stems))
        if args.refresh:
            for st in stems:
                if os.path.exists(cache_path(st)):
                    os.remove(cache_path(st))
        session_index()
        t0 = time.time()
        aggs = []
        # biggest sessions first so the pool tail is short
        stems.sort(key=lambda st: -session_index().get(st, [0, 0])[0])
        with multiprocessing.Pool(args.jobs) as pool:
            for i, (stem, A, err) in enumerate(pool.imap_unordered(_agg_worker, stems)):
                if err:
                    print(f"  failed {stem}: {err}", file=sys.stderr)
                else:
                    aggs.append(A)
                if (i + 1) % 25 == 0:
                    print(f"  {i+1}/{len(stems)} sessions ({time.time()-t0:.0f}s)", file=sys.stderr)
        print(f"  {len(aggs)} sessions aggregated in {time.time()-t0:.0f}s", file=sys.stderr)
        report(merge(aggs), top=args.top)
        return

    s = load_session(args.stem, verbose=True)
    if args.cmd == "show":
        show(s, args.a, args.b, args.width)
    elif args.cmd == "req":
        show(s, args.seq, args.seq, args.width)
    elif args.cmd == "blocks":
        first = {}
        for r in s.requests:
            for idx, inp, cached, o in r["attr"]:
                if idx not in first and (inp or o):
                    first[idx] = inp or o
        for idx, tok in sorted(first.items(), key=lambda kv: -kv[1])[: args.top]:
            b = s.blocks[idx]
            print(f"{tok:>7}t #{idx:<5} seq{b['seq']:<5} {b['pcat'][:24]:<24} {b['cat'][:30]:<30} {trunc(b['text'], args.width)}")
    elif args.cmd == "grep":
        pat = re.compile(args.pattern, re.I)
        for idx, b in enumerate(s.blocks):
            m = pat.search(b["text"])
            if m:
                start = max(0, m.start() - 60)
                print(f"#{idx:<5} seq{b['seq']:<5} {b['pcat'][:24]:<24} {trunc(b['text'][start:start+args.width], args.width)}")


if __name__ == "__main__":
    main()
