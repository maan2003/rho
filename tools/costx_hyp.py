#!/usr/bin/env python3
"""Hypothesis checks over cached costx sessions (macro-level cost structure).
Usage: costx_hyp.py [--since 30d] [--min-requests 1] [--jobs 16]
"""
import argparse, collections, multiprocessing, os, re, subprocess, sys
sys.path.insert(0, os.path.dirname(__file__))
import costx

OUT_KINDS = ("custom_tool_call_output", "function_call_output")
CALL_KINDS = ("custom_tool_call", "function_call")
PATH_RE = re.compile(r"(?<![\w/])(?:[\w.-]+/)+[\w.-]+\.\w{1,5}\b")
CTX_EDGES = [50_000, 100_000, 150_000, 200_000]
CTX_LABELS = ["<50k", "50-100k", "100-150k", "150-200k", ">200k"]


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
    C = collections.Counter()
    sysp = next((b["text"] for b in s.blocks if b["cat"] == "system-prompt"), "")
    role = "advisor" if "You are the Advisor" in sysp else "engineer"
    day = None
    canon, first = {}, {}
    for i, b in enumerate(s.blocks):
        k = (b["kind"], b["call_id"]) if b.get("call_id") else (b["kind"], b.get("id") or i)
        canon[i] = first.setdefault(k, i)
    occ_out = collections.defaultdict(float)
    path_occ = collections.defaultdict(float)  # path -> occupancy of read/search outputs mentioning it
    path_first_seq = {}
    calls = collections.Counter()
    comp_seqs = []
    t_first = t_last = None
    for r in s.requests:
        if day is None:
            import datetime
            day = datetime.date.fromtimestamp(r["req_time"]).isoformat()
        t_first = t_first or r["req_time"]; t_last = r["completed"] or r["req_time"]
        u = r["usage"]
        C["requests"] += 1
        if r["trigger"] == "compaction":
            comp_seqs.append(r["seq"])
        if not u:
            continue
        in_usd = costx.usd(u["input"] - u["cached"], u["cached"]); out_usd = costx.usd(0, 0, u["output"])
        C["usd"] += in_usd + out_usd; C["in_usd"] += in_usd
        if r["error"]:
            C["err_usd"] += in_usd + out_usd; C["err_n"] += 1
        C[("ctx", bucket(u["input"], CTX_EDGES, CTX_LABELS))] += in_usd
        tp = costx.trip_purpose(r["trigger"])
        if r["seq"] <= 10:
            C["first10_trip_usd"] += in_usd
        if r["seq"] <= 3:
            C["first3_trip_usd"] += in_usd
        for idx, inp, cached, o in r["attr"]:
            if not inp:
                continue
            idx = canon[idx]; b = s.blocks[idx]
            c = costx.usd(inp - cached, cached)
            p = costx.block_purpose(b).split("/")[0]
            C[("occ", p)] += c
            if b["kind"] in OUT_KINDS:
                occ_out[idx] += c
                if b["seq"] <= 10:
                    C["first10_out_occ"] += c
        for i in r["outputs"]:
            b = s.blocks[i]
            if b["kind"] in CALL_KINDS:
                for name in ("ask_advisor", "spawn_engineer", "wait_agent"):
                    calls[name] += b["text"].count("tools." + name + "(")
                for m in re.finditer(r"message_agent\(\{[^}]*agent_id\s*:\s*[\"'](\w+)-", b["text"]):
                    calls["message_agent->" + m.group(1)] += 1
    # paths read/searched, for cross-session and post-compaction re-read checks
    call_by_id = {}
    for i, b in enumerate(s.blocks):
        if b["kind"] in CALL_KINDS and b["call_id"] not in call_by_id:
            call_by_id[b["call_id"]] = i
    reread_post_comp = 0.0
    for idx, c in occ_out.items():
        b = s.blocks[idx]
        top = costx.block_purpose(b).split("/")[0]
        if top not in ("read", "search", "vcs"):
            continue
        ci = call_by_id.get(b["call_id"])
        if ci is None:
            continue
        paths = set(PATH_RE.findall(s.blocks[ci]["text"]))
        paths = {p for p in paths if not p.startswith("/tmp") and "node_modules" not in p}
        if not paths:
            continue
        seq = b["seq"]
        seen_before = [p for p in paths if p in path_first_seq and path_first_seq[p] < seq]
        # re-read within 15 requests after a compaction of a path already read before the compaction
        if comp_seqs and any(cs <= seq <= cs + 15 for cs in comp_seqs) and seen_before:
            reread_post_comp += c
        for p in paths:
            path_occ[p] += c / len(paths)
            path_first_seq.setdefault(p, seq)
    C["reread_post_comp"] = reread_post_comp
    C["compactions"] = len(comp_seqs)
    return dict(stem=stem, role=role, day=day, C=C, calls=calls, path_occ=dict(path_occ),
                sysp_len=len(sysp), first_user=next((b["text"][:120] for b in s.blocks if b["cat"] == "user-msg"), ""),
                life_s=(t_last - t_first) if t_first and t_last else 0, avg_ctx=(C["requests"] and None))


def report(R):
    R = [r for r in R if r]
    total = sum(r["C"]["usd"] for r in R)
    by_role = collections.defaultdict(list)
    for r in R:
        by_role[r["role"]].append(r)
    print(f"== {len(R)} sessions, ${total:.2f}")
    print("\n== H1: advisors are spawned per question (ask_advisor = spawn_child), never reused")
    for role, rs in by_role.items():
        usd = sum(r["C"]["usd"] for r in rs); req = sum(r["C"]["requests"] for r in rs)
        reqs = sorted(r["C"]["requests"] for r in rs)
        med = reqs[len(reqs) // 2]
        print(f"   {role:9} sessions {len(rs):5d}  ${usd:9.2f} ({usd/total*100:4.1f}%)  requests {req:7d}  median req/session {med:4d}  "
              f"$/session {usd/len(rs):6.2f}  first-10-trips ${sum(r['C']['first10_trip_usd'] for r in rs):.2f}  first-10 outputs occupancy ${sum(r['C']['first10_out_occ'] for r in rs):.2f}")
    calls = collections.Counter()
    for r in by_role["engineer"]:
        calls.update(r["calls"])
    print("   engineer calls: " + "  ".join(f"{k} {v}" for k, v in calls.most_common()))
    adv = by_role["advisor"]
    print(f"   advisor sessions by request count: " + "  ".join(f"{lab} {n}" for lab, n in collections.Counter(bucket(r['C']['requests'], [5, 20, 50, 100], ['<5', '5-20', '20-50', '50-100', '>100']) for r in adv).most_common()))
    occ = collections.Counter()
    for r in adv:
        for k, v in r["C"].items():
            if isinstance(k, tuple) and k[0] == "occ":
                occ[k[1]] += v
    tot_occ = sum(occ.values())
    print("   advisor occupancy by purpose: " + "  ".join(f"{k} {v/tot_occ*100:.0f}%" for k, v in occ.most_common(8)))
    print("\n== H2: the same files are read by several sessions the same day (cross-session re-orientation)")
    per_day = collections.defaultdict(lambda: collections.defaultdict(list))
    for r in R:
        for p, c in r["path_occ"].items():
            per_day[r["day"]][p].append((r["stem"], c))
    dup_occ = 0.0; dup_paths = 0; tot_path_occ = 0.0
    for day, paths in per_day.items():
        for p, lst in paths.items():
            tot_path_occ += sum(c for _, c in lst)
            if len(lst) > 1:
                dup_paths += 1
                dup_occ += sum(c for _, c in lst) - max(c for _, c in lst)
    print(f"   read/search output occupancy attributable to paths: ${tot_path_occ:.2f}; paths read in 2+ sessions same day: {dup_paths}; occupancy beyond the first session ${dup_occ:.2f} ({dup_occ/total*100:.1f}% of bill)")
    print("\n== H3: cost by context size at request time (compaction threshold effect)")
    ctx = collections.Counter()
    for r in R:
        for k, v in r["C"].items():
            if isinstance(k, tuple) and k[0] == "ctx":
                ctx[k[1]] += v
    tin = sum(ctx.values())
    print("   " + "  ".join(f"{l} ${ctx[l]:.0f} ({ctx[l]/tin*100:.0f}%)" for l in CTX_LABELS))
    print("\n== H4: post-compaction re-reads (read/search of a path already read, within 15 requests after compaction)")
    rr = sum(r["C"]["reread_post_comp"] for r in R); nc = sum(r["C"]["compactions"] for r in R)
    print(f"   compactions {nc}; occupancy of such re-read outputs ${rr:.2f} ({rr/total*100:.1f}%)")
    print("\n== H5: session orientation (first 10 requests)")
    f10t = sum(r["C"]["first10_trip_usd"] for r in R); f10o = sum(r["C"]["first10_out_occ"] for r in R)
    print(f"   first-10-request trips ${f10t:.2f} ({f10t/total*100:.1f}%); outputs produced in first 10 requests occupy ${f10o:.2f} ({f10o/total*100:.1f}%) over the session")
    print("\n== H6: errored requests")
    e = sum(r["C"]["err_usd"] for r in R); en = sum(r["C"]["err_n"] for r in R)
    print(f"   {en} errored requests cost ${e:.2f} ({e/total*100:.1f}%)")
    print("\n== H7: cost concentration")
    rs = sorted(R, key=lambda r: -r["C"]["usd"])
    for n in (5, 20, 50):
        print(f"   top {n} sessions: ${sum(r['C']['usd'] for r in rs[:n]):.2f} ({sum(r['C']['usd'] for r in rs[:n])/total*100:.0f}%)")
    print("\n== advisor sessions: top 8 by cost")
    for r in sorted(adv, key=lambda r: -r["C"]["usd"])[:8]:
        print(f"   ${r['C']['usd']:7.2f} req {r['C']['requests']:4d} life {r['life_s']/60:5.0f}m  {r['stem'][:8]}  {r['first_user'][:90]!r}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--since", default="30d"); ap.add_argument("--min-requests", type=int, default=1); ap.add_argument("--jobs", type=int, default=16)
    a = ap.parse_args()
    out = subprocess.run([sys.executable, os.path.join(os.path.dirname(__file__), "costx.py"), "sessions", "--since", a.since, "--min-requests", str(a.min_requests)], capture_output=True, text=True).stdout
    stems = [ln.split()[0] for ln in out.splitlines() if re.match(r"^[0-9a-f]{16}\s", ln)]
    print(f"{len(stems)} sessions", file=sys.stderr)
    with multiprocessing.Pool(a.jobs) as pool:
        R = list(pool.imap_unordered(worker, stems, chunksize=4))
    report(R)


if __name__ == "__main__":
    main()
