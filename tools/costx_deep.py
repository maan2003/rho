#!/usr/bin/env python3
"""Occupancy deep-dive for tool outputs: what tricks would shrink context, and by how much.

Simulates first-order savings on the cached sessions produced by costx.py:
  - per-purpose output token caps (e.g. cap rg output at 2k tokens)
  - rg match-line caps
  - evicting outputs older than N requests
  - deduplicating repeated identical calls / repeated reads of the same files
  - junk paths (target/, node_modules, lockfiles) and long-line outputs
Usage: costx_deep.py [--since 30d] [--min-requests 20] [--jobs 16] [STEM...]
"""
import argparse, collections, multiprocessing, os, re, subprocess, sys
sys.path.insert(0, os.path.dirname(__file__))
import costx

OUT_KINDS = ("custom_tool_call_output", "function_call_output")
CALL_KINDS = ("custom_tool_call", "function_call")
TOKEN_CAPS = [1000, 2000, 3000, 5000]
LINE_CAPS = [30, 60, 100, 200]
AGE_CAPS = [20, 50, 100, 200]
POLICIES = {
    "caps A (search 2k, vcs 3k, read 3k, build/test/remote/web 2k, rest 5k)": {"search": 2000, "vcs": 3000, "read": 3000, "build": 2000, "test": 2000, "remote": 2000, "web": 2000, "*": 5000},
    "caps B (search 3k, vcs 4k, read 4k, others 3k)": {"search": 3000, "vcs": 4000, "read": 4000, "*": 3000},
    "no caps (eviction only)": {},
}
JUNK_RE = re.compile(r"(^|/)(target|node_modules|\.git|dist|build|\.cache|result)/|Cargo\.lock|flake\.lock|package-lock\.json|/nix/store/", re.M)
PATH_RE = re.compile(r"(?<![\w/])(?:[\w.-]+/)+[\w.-]+|\b[\w-]+\.(?:rs|py|ts|tsx|js|toml|nix|md|json|yaml|yml|go|c|h|cpp|sh)\b")
RANGE_RE = re.compile(r"sed\s+-n\s+['\"]?(\d+),(\d+)p")


def bucket(v, edges, labels):
    for e, l in zip(edges, labels):
        if v < e:
            return l
    return labels[-1]


def call_features(cmd):
    f = set()
    words = set(re.findall(r"(?<![\w-])-[A-Za-z]+|--[a-z-]+", cmd))
    if re.search(r"\brg\b", cmd):
        f.add("rg")
        if words & {"-A", "-B", "-C", "--context", "--after-context", "--before-context"} or re.search(r"\s-[ABC]\d", cmd):
            f.add("rg-context")
        if words & {"-l", "--files", "--files-with-matches", "-c", "--count"}:
            f.add("rg-list")
        if not re.search(r"\|\s*(head|tail|wc|sort|uniq|cut|awk)\b", cmd) and not (words & {"-m", "--max-count", "--max-columns", "-M"}):
            f.add("rg-uncapped")
    if re.search(r"\bcat\s+(?!<<)", cmd) and not re.search(r"\|\s*(head|tail|sed|grep|rg)\b", cmd):
        f.add("cat-whole")
    if re.search(r"\bjj\s+diff\b", cmd) and "--stat" not in cmd and not re.search(r"jj\s+diff[^|;\n]*\s\S+\.\w+", cmd):
        f.add("jj-diff-whole")
    if re.search(r"\bjj\s+log\b", cmd) and not (words & {"-n", "--limit"}) and not re.search(r"jj\s+log[^|;\n]*\s-r\b", cmd):
        f.add("jj-log-unlimited")
    m = re.search(r"max_tokens\W+(\d+)", cmd)
    if cmd.startswith("wait(") and m:
        f.add("wait-max>10k" if int(m.group(1)) > 10000 else "wait-max<=10k")
    m = re.search(r"max_output_tokens\W+(\d+)", cmd[:200])
    if m and int(m.group(1)) > 10000:
        f.add("exec-pragma>10k")
    spans = [int(b) - int(a) for a, b in RANGE_RE.findall(cmd)]
    if spans:
        f.add("range>=300" if max(spans) >= 300 else "range<300")
    if re.search(r"\|\s*head\b", cmd):
        f.add("head-capped")
    return f


def worker(stem):
    try:
        s = costx.load_session(stem)
    except Exception as e:
        return None
    C = collections.Counter()
    occ = collections.defaultdict(float)
    occ_old = {50: collections.defaultdict(float), 100: collections.defaultdict(float)}
    toks = {}
    age = collections.Counter()
    call_by_id = {}
    for i, b in enumerate(s.blocks):
        if b["kind"] in CALL_KINDS and b["call_id"] not in call_by_id:
            call_by_id[b["call_id"]] = i
    # resume replays history as new input items; fold copies onto the first block with the same call id
    canon, first = {}, {}
    for i, b in enumerate(s.blocks):
        k = (b["kind"], b["call_id"]) if b.get("call_id") else (b["kind"], b.get("id") or i)
        canon[i] = first.setdefault(k, i)
    for r in s.requests:
        if not r["usage"]:
            continue
        for idx, inp, cached, o in r["attr"]:
            if not inp:
                continue
            idx = canon[idx]
            b = s.blocks[idx]
            c = costx.usd(inp - cached, cached)
            C["all_occ"] += c
            if b["kind"] not in OUT_KINDS:
                continue
            C["out_occ"] += c
            occ[idx] += c
            toks[idx] = max(toks.get(idx, 0), inp)
            p = costx.block_purpose(b).split("/")[0]
            a = r["seq"] - b["seq"]
            age[(p, bucket(a, AGE_CAPS, ["<20", "20-50", "50-100", "100-200", ">200"]))] += c
            for n in AGE_CAPS:
                if a > n:
                    C[("age>", p, n)] += c
                    if n in occ_old:
                        occ_old[n][idx] += c
    seen_calls = collections.Counter()
    last_out = {}
    seen_paths = collections.Counter()
    per_purpose = collections.defaultdict(collections.Counter)
    tops = []
    for idx, c in occ.items():
        b = s.blocks[idx]
        purpose = costx.block_purpose(b)
        top = purpose.split("/")[0]
        t = toks[idx]
        text = b["text"]
        nl = text.count("\n") or 1
        avg_line = min(len(text), 1500) / nl
        lines = max(1, b["tlen"] / max(avg_line, 1))
        P = per_purpose[purpose]
        P["occ"] += c; P["n"] += 1; P["tokens"] += t
        P["size " + bucket(t, [500, 2000, 5000, 10000], ["<500", "500-2k", "2k-5k", "5k-10k", ">10k"])] += c
        for cap in TOKEN_CAPS:
            if t > cap:
                P[("cap", cap)] += c * (1 - cap / t)
        if JUNK_RE.search(text):
            junk_lines = sum(1 for ln in text.splitlines() if JUNK_RE.search(ln))
            P["junk"] += c * junk_lines / nl
        if avg_line > 250:
            P["longline"] += c
        ci = call_by_id.get(b["call_id"])
        cmd = s.blocks[ci]["text"] if ci is not None else ""
        feats = call_features(cmd) if cmd else set()
        if "+" in b["cat"]:
            feats.add("multi-command call")
        for f in feats:
            P["feat " + f] += c
            if f == "wait-max>10k":
                for cap in (5000, 10000):
                    if t > cap:
                        P[("waitcap", cap)] += c * (1 - cap / t)
        if "rg" in feats:
            for cap in LINE_CAPS:
                if lines > cap:
                    P[("rgcap", cap)] += c * (1 - cap / lines)
        key = (b["cat"], re.sub(r"\s+", " ", cmd)[:400])
        if cmd and not b.get("poll_out"):
            if seen_calls[key]:
                P["dup-exact" if top in ("read", "search", "vcs") else "rerun"] += c
                if last_out.get(key) == (text, b["tlen"]):
                    P["dup-same-output"] += c
            seen_calls[key] += 1
            last_out[key] = (text, b["tlen"])
            if top in ("read", "search", "vcs"):
                paths = tuple(sorted(set(PATH_RE.findall(cmd))))
                if paths:
                    pk = (top, paths)
                    if seen_paths[pk]:
                        P["dup-paths"] += c
                    seen_paths[pk] += 1
        for name, caps in POLICIES.items():
            cap = caps.get(top, caps.get("*"))
            eff = min(t, cap) if cap else t
            C[("policy", name, "caps")] += c * (1 - eff / t)
            for n in (50, 100):
                C[("policy", name, n)] += c * (1 - eff / t) + occ_old[n].get(idx, 0.0) * eff / t
        tops.append((c, t, stem, idx, purpose, b["cat"][4:][:40], re.sub(r"\s+", " ", cmd)[:110]))
    tops.sort(reverse=True)
    return C, age, per_purpose, tops[:15]


def merge(results):
    C = collections.Counter(); age = collections.Counter(); PP = collections.defaultdict(collections.Counter); tops = []
    for r in results:
        if r is None:
            continue
        c, a, pp, t = r
        C.update(c); age.update(a)
        for k, v in pp.items():
            PP[k].update(v)
        tops += t
    tops.sort(reverse=True)
    return C, age, PP, tops


def rollup(PP, depth):
    out = collections.defaultdict(collections.Counter)
    for k, v in PP.items():
        out["/".join(k.split("/")[:depth])].update(v)
    return out


def report(C, age, PP, tops, top=12):
    total, out_total = C["all_occ"], C["out_occ"]
    pct = lambda v: f"{v/total*100:5.1f}%"
    print(f"== tool-output occupancy ${out_total:.2f} of total input ${total:.2f} ({out_total/total*100:.0f}%); all $ below are occupancy, % of total input bill")
    P1 = rollup(PP, 1); P2 = rollup(PP, 2)
    order1 = sorted(P1, key=lambda k: -P1[k]["occ"])
    print("\n== size distribution of outputs (occupancy $ by output token size)")
    print(f"   {'purpose':14} {'occ':>9} {'n':>7} {'avg tok':>8} {'<500':>8} {'500-2k':>8} {'2k-5k':>8} {'5k-10k':>8} {'>10k':>8}")
    for k in order1[:top]:
        v = P1[k]
        print(f"   {k:14} ${v['occ']:8.2f} {v['n']:7d} {v['tokens']/max(1,v['n']):8.0f} " + " ".join(f"${v['size '+s]:7.2f}" for s in ["<500", "500-2k", "2k-5k", "5k-10k", ">10k"]))
    print("\n== per-purpose output token cap: occupancy saved (first order, later outputs shrink too)")
    print(f"   {'purpose':20} {'occ':>9} " + " ".join(f"{'cap '+str(c):>10}" for c in TOKEN_CAPS))
    for k in sorted(P2, key=lambda k: -P2[k]["occ"])[:22]:
        v = P2[k]
        print(f"   {k:20} ${v['occ']:8.2f} " + " ".join(f"${v[('cap',c)]:6.2f}{v[('cap',c)]/total*100:3.0f}%" for c in TOKEN_CAPS))
    tot_caps = {c: sum(v[("cap", c)] for v in PP.values()) for c in TOKEN_CAPS}
    print(f"   {'ALL outputs':20} ${out_total:8.2f} " + " ".join(f"${tot_caps[c]:6.2f}{tot_caps[c]/total*100:3.0f}%" for c in TOKEN_CAPS))
    print("\n== rg match-line cap (outputs of calls containing rg): occupancy saved")
    rg = collections.Counter()
    for k, v in PP.items():
        if v["feat rg"]:
            rg["occ"] += v["feat rg"]
            for c in LINE_CAPS:
                rg[c] += v[("rgcap", c)]
    print(f"   rg outputs occupancy ${rg['occ']:.2f} ({pct(rg['occ'])}); " + "  ".join(f"cap {c} lines -> ${rg[c]:.2f} ({rg[c]/total*100:.1f}%)" for c in LINE_CAPS))
    print("\n== call-shape features: occupancy of outputs produced by calls with that shape")
    feats = collections.Counter()
    for v in PP.values():
        for k, x in v.items():
            if isinstance(k, str) and k.startswith("feat "):
                feats[k[5:]] += x
    for k, x in feats.most_common():
        print(f"   {k:18} ${x:8.2f} {pct(x)}")
    print("\n== duplicates / junk / long lines")
    wc = {cap: sum(v[("waitcap", cap)] for v in PP.values()) for cap in (5000, 10000)}
    print(f"   wait(max_tokens>10k) outputs capped at 10k -> ${wc[10000]:.2f} ({wc[10000]/total*100:.1f}%), at 5k -> ${wc[5000]:.2f} ({wc[5000]/total*100:.1f}%)")
    for key, label in [("dup-same-output", "repeated call whose output was byte-identical (pure waste)"), ("dup-exact", "identical read/search/vcs call repeated (2nd+ output)"), ("rerun", "identical build/test/other call re-run (2nd+ output)"), ("dup-paths", "same file set read/searched again (2nd+ output)"), ("junk", "lines under target/ node_modules .git lockfiles /nix/store"), ("longline", "outputs with avg line > 250 chars")]:
        by = sorted(((P1[k][key], k) for k in P1), reverse=True)[:5]
        tot_k = sum(v for v, _ in ((P1[k][key], k) for k in P1))
        print(f"   {label:52} ${tot_k:8.2f} {pct(tot_k)}   " + "  ".join(f"{k} ${v:.0f}" for v, k in by if v > 0))
    print("\n== evict tool outputs older than N requests: occupancy saved")
    print(f"   {'purpose':14} " + " ".join(f"{'>'+str(n)+' req':>12}" for n in AGE_CAPS))
    for k in order1[:top]:
        print(f"   {k:14} " + " ".join(f"${C[('age>',k,n)]:6.2f}{C[('age>',k,n)]/total*100:4.0f}%" for n in AGE_CAPS))
    allage = {n: sum(v for k, v in C.items() if isinstance(k, tuple) and k[0] == "age>" and k[2] == n) for n in AGE_CAPS}
    print(f"   {'ALL outputs':14} " + " ".join(f"${allage[n]:6.2f}{allage[n]/total*100:4.0f}%" for n in AGE_CAPS))
    print("\n== combined policies: per-purpose output caps, optionally + evict outputs older than N requests")
    for name in POLICIES:
        v = {k: C[("policy", name, k)] for k in ("caps", 50, 100)}
        print(f"   {name:70} caps only ${v['caps']:8.2f} ({v['caps']/total*100:4.1f}%)   +evict>100 ${v[100]:8.2f} ({v[100]/total*100:4.1f}%)   +evict>50 ${v[50]:8.2f} ({v[50]/total*100:4.1f}%)")
    print("\n== top output blocks by occupancy")
    for c, t, stem, idx, purpose, fine, cmd in tops[:25]:
        print(f"   ${c:5.2f} {t:6d}tok {stem[:6]}#{idx:<6} {purpose:18} {fine:22} {cmd}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("stems", nargs="*")
    ap.add_argument("--since", default="30d")
    ap.add_argument("--min-requests", type=int, default=20)
    ap.add_argument("--jobs", type=int, default=16)
    a = ap.parse_args()
    stems = a.stems
    if not stems:
        out = subprocess.run([sys.executable, os.path.join(os.path.dirname(__file__), "costx.py"), "sessions", "--since", a.since, "--min-requests", str(a.min_requests)], capture_output=True, text=True).stdout
        stems = [ln.split()[0] for ln in out.splitlines() if re.match(r"^[0-9a-f]{16}\s", ln)]
    print(f"{len(stems)} sessions", file=sys.stderr)
    with multiprocessing.Pool(a.jobs) as pool:
        results = list(pool.imap_unordered(worker, stems, chunksize=4))
    report(*merge(results))


if __name__ == "__main__":
    main()
