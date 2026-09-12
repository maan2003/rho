#!/usr/bin/env python3
"""Search/read deep breakdown over cached costx sessions. Usage: costx_rw.py [--since 30d] [--jobs 16]"""
import argparse, collections, multiprocessing, os, re, subprocess, sys
sys.path.insert(0, os.path.dirname(__file__))
import costx

OUT_KINDS = ("custom_tool_call_output", "function_call_output")
CALL_KINDS = ("custom_tool_call", "function_call")
PATH_RE = re.compile(r"(?<![\w/])(?:[\w.-]+/)*[\w.-]+\.(?:rs|py|ts|tsx|js|toml|nix|md|json|yaml|yml|go|c|h|cpp|cu|cuh|sh|txt|lock)\b")
OUT_PATH_RE = re.compile(r"^([\w./-]+\.\w{1,4})[:\-]\d+[:\-]", re.M)
RANGE_RE = re.compile(r"sed\s+-n\s+['\"]?(\d+),(\d+)p['\"]?\s+([^\s;|&'\"]+)")
CAT_RE = re.compile(r"\b(?:cat|nl(?:\s+-ba)?)\s+(?:-\w+\s+)*([^\s;|&'\"<>-][^\s;|&'\"]*)")
HEAD_RE = re.compile(r"\|\s*head\s+(?:-n\s*|-)(\d+)")
WORKDIR_RE = re.compile(r"workdir\s*:\s*[\"']([^\"']+)")
RG_RE = re.compile(r"\brg\b([^|;\n]*)")


def bucket(v, edges, labels):
    for e, l in zip(edges, labels):
        if v < e:
            return l
    return labels[-1]


def worker(stem):
    try:
        s = costx.load_session(stem)
    except Exception:
        return None
    canon, first = {}, {}
    for i, b in enumerate(s.blocks):
        k = (b["kind"], b["call_id"]) if b.get("call_id") else (b["kind"], b.get("id") or i)
        canon[i] = first.setdefault(k, i)
    call_by_id = {}
    for i, b in enumerate(s.blocks):
        if b["kind"] in CALL_KINDS and b["call_id"] not in call_by_id:
            call_by_id[b["call_id"]] = i
    occ, toks = collections.defaultdict(float), {}
    for r in s.requests:
        if not r["usage"]:
            continue
        for idx, inp, cached, o in r["attr"]:
            if inp:
                idx = canon[idx]
                if s.blocks[idx]["kind"] in OUT_KINDS:
                    occ[idx] += costx.usd(inp - cached, cached); toks[idx] = max(toks.get(idx, 0), inp)
    # all read-ish paths with the seq they were opened at (for "matched file later opened" and overlap checks)
    reads = []  # (seq, path, a, b, occ, idx)
    read_seqs = collections.defaultdict(list)  # path -> [seq]
    items = []
    for idx, c in occ.items():
        b = s.blocks[idx]
        purpose = costx.block_purpose(b)
        top = purpose.split("/")[0]
        if top not in ("search", "read"):
            continue
        ci = call_by_id.get(b["call_id"])
        cmd = s.blocks[ci]["text"] if ci is not None else ""
        items.append((idx, c, purpose, cmd, b))
        if top == "read":
            rr = [(p, int(a), int(bb)) for a, bb, p in RANGE_RE.findall(cmd)]
            rr += [(p, 1, 10**9) for p in CAT_RE.findall(cmd) if "." in os.path.basename(p)]
            for p, a, bb in rr:
                reads.append((b["seq"], p, a, bb, c / len(rr), idx)); read_seqs[os.path.basename(p)].append(b["seq"])
    S = collections.Counter(); R = collections.Counter(); repo = collections.Counter()
    rc = collections.Counter()
    for idx, c, purpose, cmd, b in items:
        for m in re.finditer(r"/home/\w+/src/([\w.-]+)", cmd):
            rc[m.group(1)] += 1
    session_repo = rc.most_common(1)[0][0] + "*" if rc else "?"
    # search analysis
    prev_search = None
    for idx, c, purpose, cmd, b in sorted(items, key=lambda x: x[4]["seq"]):
        top = purpose.split("/")[0]
        m = WORKDIR_RE.search(cmd) or re.search(r"/home/\w+/src/([\w.-]+)", cmd)
        repo[(top, os.path.basename(m.group(1).rstrip("/")) if m else session_repo)] += c
        text = b["text"]; nl = text.count("\n") or 1; sample = min(len(text), 1500)
        lines_est = b["tlen"] / max(1.0, sample / nl)
        if top == "search":
            S["occ"] += c
            S[("fine", b["cat"][4:][:30])] += c
            rgs = RG_RE.findall(cmd)
            flags = set()
            for seg in rgs:
                for f in re.findall(r"(?<!\S)(-[A-Za-z]+|--[a-z-]+)", seg):
                    flags.add(f)
            heads = [int(h) for h in HEAD_RE.findall(cmd)]
            S[("head", bucket(min(heads), [21, 51, 101, 201], ["<=20", "21-50", "51-100", "101-200", ">200"]) if heads else "none")] += c
            for f in ("-n", "-l", "--files", "-C", "-A", "-B", "-t", "-g", "--glob", "-i", "-w", "-F", "-c", "--max-count", "-m", "-S", "--no-heading"):
                if f in flags or (f in ("-C", "-A", "-B") and re.search(rf"\s{f}\s*\d", cmd)):
                    S[("flag", f)] += c
            # output composition from the sample
            paths = OUT_PATH_RE.findall(text)
            files_sample = len(set(paths)); files_est = files_sample * b["tlen"] / max(1, sample)
            S[("files", bucket(files_est, [1, 2, 6, 21, 51], ["none parsed", "1", "2-5", "6-20", "21-50", ">50"]))] += c
            S[("lines", bucket(lines_est, [21, 51, 101, 301], ["<=20", "21-50", "51-100", "101-300", ">300"]))] += c
            avg_line = sample / nl
            S[("linelen", bucket(avg_line, [80, 120, 200, 400], ["<80", "80-120", "120-200", "200-400", ">400"]))] += c
            test_lines = sum(1 for ln in text.splitlines() if re.search(r"(^|/)(tests?|test_|_test\.|spec|docs?|examples?)/", ln))
            S["test/doc lines"] += c * test_lines / nl
            # were the matched files opened afterwards in this session?
            if paths:
                opened = sum(1 for p in set(paths) if any(sq > b["seq"] for sq in read_seqs.get(os.path.basename(p), [])))
                S["files_total"] += c; S["files_opened"] += c * opened / len(set(paths))
            # refinement chain: another search within 3 requests sharing the pattern prefix or workdir+paths
            pat = re.search(r"\brg\b[^'\"]*['\"]([^'\"]{3,})['\"]", cmd)
            pat = pat.group(1) if pat else None
            if prev_search and b["seq"] - prev_search[0] <= 3 and pat and prev_search[1] and (pat[:6] == prev_search[1][:6] or pat in prev_search[1] or prev_search[1] in pat):
                S["refined_prev_occ"] += prev_search[2]; S["refined_n"] += 1
            prev_search = (b["seq"], pat, c)
        else:
            R["occ"] += c
            R[("fine", b["cat"][4:][:30])] += c
            spans = [int(bb) - int(a) for a, bb, p in RANGE_RE.findall(cmd)]
            if spans:
                R[("span", bucket(max(spans), [100, 200, 400, 800], ["<100", "100-200", "200-400", "400-800", ">800"]))] += c
                R[("nranges", bucket(len(spans), [2, 3, 5], ["1", "2", "3-4", "5+"]))] += c
            elif CAT_RE.search(cmd):
                R[("span", "whole file")] += c
            else:
                R[("span", "other")] += c
            exts = collections.Counter(os.path.splitext(p)[1] for p in PATH_RE.findall(cmd))
            if exts:
                R[("ext", exts.most_common(1)[0][0])] += c
            if re.search(r"\bnl\b|--line-number|\bcat -n\b|sed\s*=|awk.*NR", cmd):
                R["with line numbers"] += c
            R[("linelen", bucket(sample / nl, [60, 90, 120, 200], ["<60", "60-90", "90-120", "120-200", ">200"]))] += c
    # overlapping re-reads of the same path
    by_path = collections.defaultdict(list)
    for seq, p, a, bb, c, idx in reads:
        by_path[p].append((seq, a, bb, c, idx))
    for p, lst in by_path.items():
        lst.sort()
        seen = []  # list of (a,b)
        for seq, a, bb, c, idx in lst:
            span = max(1, bb - a)
            ov = 0
            for sa, sb in seen:
                lo, hi = max(a, sa), min(bb, sb)
                if hi > lo:
                    ov += hi - lo
            frac = min(1.0, ov / span)
            if seen:
                R["reread_any"] += c
            R["reread_overlap"] += c * frac
            if frac >= 0.9:
                R["reread_full"] += c
            seen.append((a, bb))
        R[("times_read", bucket(len(lst), [2, 3, 5, 10], ["1", "2", "3-4", "5-9", "10+"]))] += sum(x[3] for x in lst)
    return S, R, repo


def table(title, C, prefix, total, order=None):
    rows = [(k[1], v) for k, v in C.items() if isinstance(k, tuple) and k[0] == prefix]
    if order:
        rows.sort(key=lambda kv: order.index(kv[0]) if kv[0] in order else 99)
    else:
        rows.sort(key=lambda kv: -kv[1])
    print(f"\n== {title}")
    for k, v in rows[:14]:
        print(f"   {str(k):34} ${v:8.2f}  {v/total*100:4.1f}%")


def main():
    ap = argparse.ArgumentParser(); ap.add_argument("--since", default="30d"); ap.add_argument("--jobs", type=int, default=16); ap.add_argument("--min-requests", type=int, default=20)
    a = ap.parse_args()
    out = subprocess.run([sys.executable, os.path.join(os.path.dirname(__file__), "costx.py"), "sessions", "--since", a.since, "--min-requests", str(a.min_requests)], capture_output=True, text=True).stdout
    stems = [ln.split()[0] for ln in out.splitlines() if re.match(r"^[0-9a-f]{16}\s", ln)]
    with multiprocessing.Pool(a.jobs) as pool:
        res = [r for r in pool.imap_unordered(worker, stems, chunksize=4) if r]
    S, R, repo = collections.Counter(), collections.Counter(), collections.Counter()
    for s_, r_, rp in res:
        S.update(s_); R.update(r_); repo.update(rp)
    so, ro = S["occ"], R["occ"]
    print(f"== SEARCH outputs occupancy ${so:.2f}; READ outputs occupancy ${ro:.2f}   (all % below are of the respective total)")
    table("search: call shape (fine label)", S, "fine", so)
    table("search: head -N cap used in the call (min N)", S, "head", so, ["none", ">200", "101-200", "51-100", "21-50", "<=20"])
    table("search: rg flags present (a call can have several)", S, "flag", so)
    table("search: distinct files per output (estimated; none parsed = sample had no path:line: lines)", S, "files", so, ["none parsed", "1", "2-5", "6-20", "21-50", ">50"])
    table("search: match lines per output (estimated)", S, "lines", so, ["<=20", "21-50", "51-100", "101-300", ">300"])
    table("search: avg line length of output", S, "linelen", so, ["<80", "80-120", "120-200", "200-400", ">400"])
    print(f"\n== search: matched files that the agent later opened (sed/cat) in the same session: {S['files_opened']/max(1e-9,S['files_total'])*100:.0f}% of files (occupancy-weighted, from output samples)")
    print(f"== search: lines under tests/docs/examples dirs: ${S['test/doc lines']:.2f} ({S['test/doc lines']/so*100:.1f}%)")
    print(f"== search: refinement chains (another search within 3 requests with a related pattern): {S['refined_n']} times; superseded outputs occupy ${S['refined_prev_occ']:.2f} ({S['refined_prev_occ']/so*100:.1f}%)")
    table("read: call shape (fine label)", R, "fine", ro)
    table("read: largest sed range span in the call (lines)", R, "span", ro, ["<100", "100-200", "200-400", "400-800", ">800", "whole file", "other"])
    table("read: number of ranges per call", R, "nranges", ro, ["1", "2", "3-4", "5+"])
    table("read: file extension", R, "ext", ro)
    table("read: avg line length", R, "linelen", ro, ["<60", "60-90", "90-120", "120-200", ">200"])
    table("read: how many times the same path was read in the session (occupancy of all its reads)", R, "times_read", ro, ["1", "2", "3-4", "5-9", "10+"])
    print(f"\n== read: with line numbers (nl/cat -n): ${R['with line numbers']:.2f} ({R['with line numbers']/ro*100:.0f}%)")
    print(f"== read: re-reads of a path already read: ${R['reread_any']:.2f} ({R['reread_any']/ro*100:.0f}%); overlapping-lines share ${R['reread_overlap']:.2f} ({R['reread_overlap']/ro*100:.0f}%); >=90% overlap (pure repeat) ${R['reread_full']:.2f} ({R['reread_full']/ro*100:.0f}%)")
    print("\n== search+read occupancy by repo (workdir basename)")
    for (top, name), v in sorted(repo.items(), key=lambda kv: -kv[1])[:16]:
        print(f"   {top:7} {name:40} ${v:8.2f}")


if __name__ == "__main__":
    main()
