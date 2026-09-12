#!/usr/bin/env python3
"""Full-text composition of search/read outputs (reads the raw provider request files).
Usage: costx_full.py [--since 30d] [--jobs 16] [--min-requests 20]"""
import argparse, collections, json, multiprocessing, os, re, subprocess, sys
sys.path.insert(0, os.path.dirname(__file__))
import costx

OUT_KINDS = ("custom_tool_call_output", "function_call_output")
CALL_KINDS = ("custom_tool_call", "function_call")
MATCH_RE = re.compile(r"^([\w@./+-]+\.\w{1,6}):(\d+):")
CTX_RE = re.compile(r"^([\w@./+-]+\.\w{1,6})-(\d+)-")
PATHONLY_RE = re.compile(r"^[\w@./+-]+\.\w{1,6}$")
PATH_RE = re.compile(r"(?<![\w/])(?:[\w.-]+/)*[\w.-]+\.(?:rs|py|ts|tsx|js|toml|nix|md|json|yaml|yml|go|c|h|cpp|cu|cuh|sh|txt)\b")
COMMENT_RE = re.compile(r"^\s*(//|#|\*|/\*|\*/|///|<!--)|^\s*$")


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
    occ = collections.defaultdict(float)
    for r in s.requests:
        if not r["usage"]:
            continue
        for idx, inp, cached, o in r["attr"]:
            if inp:
                idx = canon[idx]
                if s.blocks[idx]["kind"] in OUT_KINDS:
                    occ[idx] += costx.usd(inp - cached, cached)
    targets = collections.defaultdict(list)  # seq -> [(idx, c, top)]
    opened = collections.defaultdict(list)   # basename -> [seq] of read calls
    for idx, c in occ.items():
        b = s.blocks[idx]; top = costx.block_purpose(b).split("/")[0]
        if top in ("search", "read", "vcs"):
            targets[b["seq"]].append((idx, c, top))
        ci = call_by_id.get(b["call_id"])
        if top == "read" and ci is not None:
            for p in PATH_RE.findall(s.blocks[ci]["text"]):
                opened[os.path.basename(p)].append(b["seq"])
    S, R = collections.Counter(), collections.Counter()
    seen_lines = set()
    for seq in sorted(targets):
        try:
            body = json.load(open(f"{costx.DIR}/{stem}-{seq:04d}-request.json"))["body"]
        except Exception:
            continue
        by_call = {it.get("call_id"): it for it in body.get("input", []) if it.get("type") in OUT_KINDS}
        for idx, c, top in targets[seq]:
            b = s.blocks[idx]; it = by_call.get(b["call_id"])
            if it is None:
                continue
            text = it.get("output"); text = text if isinstance(text, str) else json.dumps(text)
            lines = [ln for ln in text.split("\n") if ln.strip()]
            L = max(1, len(lines)); chars = max(1, len(text))
            keyed = [ln.strip() for ln in lines if len(ln.strip()) >= 12]
            dup = sum(1 for ln in keyed if ln in seen_lines)
            U = collections.Counter()
            for ln in lines:
                if MATCH_RE.match(ln): U["rg match line"] += len(ln)
                elif CTX_RE.match(ln): U["rg context line (-A/-B/-C)"] += len(ln)
                elif PATHONLY_RE.match(ln.strip()): U["path-only line (rg -l/--files, find, ls)"] += len(ln)
                elif re.match(r"^\s*\d+[\t:]", ln): U["numbered file content (nl / cat -n)"] += len(ln)
                elif re.match(r"^(---|===|###|Script completed|Wall time|Output:|Warning: truncated|Total output lines)", ln): U["headers / wrapper"] += len(ln)
                elif COMMENT_RE.match(ln): U["comment-only line"] += len(ln)
                else: U["plain file content / other"] += len(ln)
            tot_u = max(1, sum(U.values()))
            for k, v in U.items():
                S[("utype", k)] += c * v / tot_u
            S["all_occ"] += c
            S["all_dup"] += c * dup / max(1, len(keyed))
            per_file_all = collections.Counter(m.group(1) for ln in lines for m in [MATCH_RE.match(ln)] if m)
            for k in (3, 5, 10):
                S[("perfile_all", k)] += c * sum(max(0, v - k) for v in per_file_all.values()) / L
            dup = sum(1 for ln in keyed if ln in seen_lines)
            long_extra = sum(max(0, len(ln) - 200) for ln in lines)
            if top == "vcs":
                S["vcs_occ"] += c; S["vcs_dup"] += c * dup / max(1, len(keyed))
                S["vcs_dup_diffbody"] += c * sum(1 for ln in keyed if ln[:1] in "+- " and ln[1:].strip() in seen_lines) / max(1, len(keyed))
                seen_lines.update(ln[1:].strip() for ln in keyed if ln[:1] in "+-" and len(ln) > 13)
                seen_lines.update(keyed); continue
            if top == "search":
                S["occ"] += c
                per_file = collections.Counter(); ctx_lines = 0; pathonly = 0; prefix_chars = 0
                for ln in lines:
                    m = MATCH_RE.match(ln)
                    if m:
                        per_file[m.group(1)] += 1; prefix_chars += m.end()
                    elif CTX_RE.match(ln):
                        ctx_lines += 1
                    elif PATHONLY_RE.match(ln.strip()):
                        pathonly += 1
                M = sum(per_file.values())
                kind = "rg match lines" if M >= 0.3 * L else ("path list" if pathonly >= 0.5 * L else "other/mixed")
                S[("kind", kind)] += c
                if kind == "rg match lines":
                    S["rg_occ"] += c
                    S[("files", "1" if len(per_file) == 1 else "2-5" if len(per_file) <= 5 else "6-20" if len(per_file) <= 20 else "21-50" if len(per_file) <= 50 else ">50")] += c
                    S[("matches", "<=20" if M <= 20 else "21-50" if M <= 50 else "51-100" if M <= 100 else "101-300" if M <= 300 else ">300")] += c
                    for k in (3, 5, 10, 20):
                        S[("perfile", k)] += c * sum(max(0, v - k) for v in per_file.values()) / L
                    for K in (50, 100, 200):
                        S[("global", K)] += c * max(0, M - K) / L
                    S["ctx_lines"] += c * ctx_lines / L
                    S["prefix_chars"] += c * prefix_chars / chars
                    S["top_file_share"] += c * max(per_file.values()) / M
                    never = sum(v for f, v in per_file.items() if not any(sq > seq for sq in opened.get(os.path.basename(f), [])))
                    S["never_opened"] += c * never / M
                    S["long_extra"] += c * long_extra / chars
                    S["dup_lines"] += c * dup / max(1, len(keyed))
                    test = sum(v for f, v in per_file.items() if re.search(r"(^|/)(tests?|testing|docs?|examples?|benches?|fixtures?)/|_test\.|\.test\.|/test_", f))
                    S["test_doc"] += c * test / M
            else:
                R["occ"] += c
                R["comment_blank"] += c * sum(1 for ln in text.split("\n") if COMMENT_RE.match(ln)) / max(1, text.count("\n") + 1)
                R["long_extra"] += c * long_extra / chars
                R["dup_lines"] += c * dup / max(1, len(keyed))
                R[("lines", "<=100" if L <= 100 else "101-200" if L <= 200 else "201-400" if L <= 400 else "401-800" if L <= 800 else ">800")] += c
            seen_lines.update(keyed)
    return S, R


def main():
    ap = argparse.ArgumentParser(); ap.add_argument("--since", default="30d"); ap.add_argument("--jobs", type=int, default=16); ap.add_argument("--min-requests", type=int, default=20)
    a = ap.parse_args()
    out = subprocess.run([sys.executable, os.path.join(os.path.dirname(__file__), "costx.py"), "sessions", "--since", a.since, "--min-requests", str(a.min_requests)], capture_output=True, text=True).stdout
    stems = [ln.split()[0] for ln in out.splitlines() if re.match(r"^[0-9a-f]{16}\s", ln)]
    S, R = collections.Counter(), collections.Counter()
    with multiprocessing.Pool(a.jobs) as pool:
        for r in pool.imap_unordered(worker, stems, chunksize=4):
            if r:
                S.update(r[0]); R.update(r[1])
    so, ro, rg = S["occ"], R["occ"], S["rg_occ"]
    print(f"== SEARCH outputs ${so:.2f} (full text); READ outputs ${ro:.2f}")
    print("\n== search output kind: " + "  ".join(f"{k[1]} ${v:.2f} ({v/so*100:.0f}%)" for k, v in S.items() if isinstance(k, tuple) and k[0] == "kind"))
    print(f"\n== rg-match outputs ${rg:.2f}: exact composition (shares of rg-match occupancy)")
    for lab, key in (("distinct files", "files"), ("match lines", "matches")):
        print(f"   {lab}: " + "  ".join(f"{k[1]} {v/rg*100:.0f}%" for k, v in sorted(S.items(), key=lambda kv: -kv[1]) if isinstance(k, tuple) and k[0] == key))
    print("   cap matches per file: " + "  ".join(f"k={k[1]} saves ${v:.2f} ({v/rg*100:.0f}%)" for k, v in sorted(S.items(), key=lambda kv: kv[0][1] if isinstance(kv[0], tuple) and kv[0][0]=='perfile' else 0) if isinstance(k, tuple) and k[0] == "perfile"))
    print("   global match cap: " + "  ".join(f"K={k[1]} saves ${v:.2f} ({v/rg*100:.0f}%)" for k, v in sorted(S.items(), key=lambda kv: kv[0][1] if isinstance(kv[0], tuple) and kv[0][0]=='global' else 0) if isinstance(k, tuple) and k[0] == "global"))
    for lab, key in (("context (-A/-B/-C) lines", "ctx_lines"), ("path:line: prefix chars", "prefix_chars"), ("matches in the single biggest file", "top_file_share"),
                     ("matches in files never opened later in the session", "never_opened"), ("chars beyond col 200", "long_extra"), ("lines already present in an earlier read/search output", "dup_lines"), ("matches under tests/docs/examples/benches", "test_doc")):
        print(f"   {lab:58} ${S[key]:8.2f} ({S[key]/rg*100:4.1f}%)")
    print(f"\n== vcs outputs ${S['vcs_occ']:.2f}: lines already in context ${S['vcs_dup']:.2f} ({S['vcs_dup']/max(1e-9,S['vcs_occ'])*100:.0f}%); diff body lines (+/-/context) already in context ${S['vcs_dup_diffbody']:.2f} ({S['vcs_dup_diffbody']/max(1e-9,S['vcs_occ'])*100:.0f}%)")
    ao = S["all_occ"]
    print(f"\n== ALL search+read outputs ${ao:.2f}: what the text is (occupancy-weighted char share)")
    for k, v in sorted(((k[1], v) for k, v in S.items() if isinstance(k, tuple) and k[0] == "utype"), key=lambda kv: -kv[1]):
        print(f"   {k:48} ${v:8.2f} ({v/ao*100:4.1f}%)")
    print(f"   lines already present in an earlier read/search output   ${S['all_dup']:8.2f} ({S['all_dup']/ao*100:4.1f}%)")
    print("   cap rg matches per file (all outputs): " + "  ".join(f"k={k[1]} ${v:.2f} ({v/ao*100:.1f}%)" for k, v in sorted(S.items(), key=lambda kv: kv[0][1] if isinstance(kv[0], tuple) and kv[0][0]=='perfile_all' else 0) if isinstance(k, tuple) and k[0] == "perfile_all"))
    print(f"\n== read outputs ${ro:.2f}: exact composition")
    print("   lines per output: " + "  ".join(f"{k[1]} {v/ro*100:.0f}%" for k, v in sorted(R.items(), key=lambda kv: -kv[1]) if isinstance(k, tuple) and k[0] == "lines"))
    for lab, key in (("blank or comment-only lines", "comment_blank"), ("chars beyond col 200", "long_extra"), ("lines already present in an earlier read/search output", "dup_lines")):
        print(f"   {lab:58} ${R[key]:8.2f} ({R[key]/ro*100:4.1f}%)")


if __name__ == "__main__":
    main()
