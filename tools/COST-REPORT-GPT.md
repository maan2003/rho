# Rho GPT cost report and recommendations

Scope: gpt-5.6-sol Rho sessions (engineers and advisors), 30 days to 2026-09-05,
1,311 sessions, 232,578 requests, $21,344 at $5/M uncached, $0.50/M cached,
$30/M output. Source: the raw provider request/response logs under
`~/.local/state/rho/debug/provider-requests`. Per-block token attribution is
exact from `usage.attribution` after 2026-08-28 (34% of requests) and
size-estimated before that (about 6% error on tool outputs).

Tools: `tools/costx.py` (views and counterfactuals), `tools/costx_deep.py`
(output caps, eviction), `tools/costx_hyp.py` (macro hypotheses),
`tools/costx_rw.py` and `tools/costx_full.py` (search/read composition).
Reports: `/tmp/costx-30d.txt`, `/tmp/costx-deep-30d.txt`, `/tmp/costx-hyp-30d.txt`.

## 1. How the bill decomposes

Cost is trips x context. 98.2% of input tokens are cache hits, average context
144k, so every request costs about $0.084 regardless of what it does. Input is
$19,435 of $21,344; output $1,958 (reasoning is 38% of output).

Three views, each summing to 100% of its own bill:

- Occupancy (what sits in context): search 22%, vcs 12.4%, read 11.6%, remote 9.9%,
  build 9.2%, reasoning 6.3%, edit 3.3%, system prompt + tools schema 4.8%.
  Tool outputs are 71% of all input cost.
- Round-trip (what caused the request): wait/poll 34.7% (80,969 trips, 37k of
  them under 5 s apart), search 12.1%, remote 8.6%, vcs 7.3%, read 6.1%,
  comms/message_agent 5.9%, edit 5.1%.
- Output: reasoning 38%, edit/patch bodies 11%, remote 7%, search 7%.

Cost is concentrated: top 5 sessions 26%, top 20 sessions 50%. Spawned engineers
57%, interactive engineers 34%, advisors 9%.

## 2. Hypotheses checked

| # | hypothesis | verdict | number |
|---|---|---|---|
| H1 | too many advisors, never reused | true mechanically, small | advisors $1,963 (9.2%), $3.62 each; ask_advisor always spawns |
| H2 | same files re-read across sessions | partly | $2,178 (10%) beyond the first reader per day, upper bound |
| H3 | compaction fires too late | confirmed, top finding | trigger is 280k (agent-side, Responses Lite has no server compaction); 44% of bill from requests over 200k |
| H4 | post-compaction re-reading | confirmed | 2.3 extra read trips and 22k re-read tokens per compaction, about $0.69 each |
| H5 | orientation dumps at session start | confirmed | outputs from the first 10 requests occupy 10.6% |
| H6 | errored requests cost money | no | errored requests carry no usage |
| H7 | cost concentrated | yes | top 20 sessions 50% |
| H8 | long sessions cost more per trip | no | $/trip flat across session quartiles |
| H9 | reasoning kept for all turns | knob, small | 6.3% of context, 3.6% from before the latest user turn |
| - | cache writes billed at 1.25x | not charged | cache_write_tokens is 0 on all 232k responses; worst case +3.6% |

Real compactions are 686, not 2,352: the rest are session resumes replaying
history that contains an old compaction item (about 2,000 resumes, mostly cache
hits, under $500). Post-compaction context is 8-11k (system prompt 37%, tools
37%, 2k summary).

## 3. Recommended steps (agreed)

Joint simulation, 30 days, overlaps accounted for:

| package | saves | cheaper | trips |
|---|---|---|---|
| 1 output caps | 26.7% | 1.36x | 232k |
| 3 blocking wait | 22.5% | 1.29x | 179k |
| 4 compaction at 200k | 27.8% | 1.39x | 232k |
| 1+3 | 43.4% | 1.77x | 179k |
| 1+3+4 | 47.0% | 1.89x | 179k |
| 1+3+4 at 250k | 43.5% | 1.77x | 179k |

At worst-case 1.25x pricing for every uncached token: 1+3+4 = 46.6%.

### Step 1: per-command output caps inside exec (26.7%)

Cap each nested exec_command result by what it ran: search 2k tokens, vcs 3k,
read 3k, build/test/remote/web 2k, everything else 5k. Half of output occupancy
comes from multi-command scripts, so the cap must apply per nested call, not per
exec result. Also clamp `wait` max_tokens and the exec `max_output_tokens`
pragma to 10k; agents raise them to 30-100k for the session-start dumps
($631 + $1,075 of occupancy).

Supporting numbers: outputs over 5k tokens are 54% of search occupancy and 64%
of vcs; 5,863 outputs hit the 10k cap with originals averaging 12k, so the cap
barely bites; rg outputs with no head or -m are $1,394.

### Step 3: blocking wait (22.5% alone, 43.4% with step 1)

Make `wait` and stdin polls block server-side until the cell finishes or a real
timeout, returning partial output on timeout (the same contract as OpenAI's
hosted shell tool: model-chosen timeout_ms, max_output_length). This collapses
53k intermediate poll trips and keeps their partial outputs out of context.
Biggest origins: build.nix $1,157, test.cargo $641, remote polling $765,
message_agent $362, proc.rho $312.

### Step 4: compaction trigger at 200k (+3.6 points after 1+3)

Lower `auto_compact_token_limit` for gpt-5.6 models from 280k to 200k
(`crates/rho-inference/src/responses/session.rs`). Alone it is worth 27.8%, but
with caps in place contexts rarely reach 200k, so the marginal value is small.
250k adds nothing. Per-role thresholds add nothing over a flat value.
Raising the threshold with each compaction throws the savings away (rising
schedules net 8.8%) because long sessions with many compactions carry the bill.

### Step 6 (small, agreed): rg per-file match cap

Keep at most 5 matches per file and 100 match lines per rg: 2-3% on top of
step 1. 43% of rg matches pile into one file and 52% are in files never opened
afterwards.

### Step 7 (agreed, design open): range short-circuit for reads

36% of read occupancy is a 90%+ overlapping re-read of a range already shown;
paths read 10+ times are 39%. The shell tool can keep (file, range, hash) of
what it returned and answer overlapping unchanged reads with the new lines plus
"lines A-B unchanged, shown at turn N". Worth up to $773 (3.6%) exactly, and it
is the cheap part of the 11.4% "already in context" pool below.

Expected total for 1+3+4+6+7: about 50%, i.e. 2x cheaper.

## 4. Debatable options

### Prune instead of compact

When context hits the trigger, drop old tool outputs, keep messages, calls and
reasoning, rebuild the context, and compact only if still over the trigger.
Re-read tax charged at the measured post-compaction rate ($0.92 per event) plus
a $0.40 uncached rebuild.

| policy at 200k | saves | prunes | compactions left | avg ctx |
|---|---|---|---|---|
| compact (reference) | 27.9% | 0 | 1,557 | 84k |
| prune outputs older than 50 req | 10.8% | 1,617 | 1,147 | 93k |
| prune all but last 30 outputs | 9.0% | 1,714 | 167 | 107k |
| caps + prune older than 50 req | 28.0% | 524 | 314 | 90k |
| caps + prune all but last 30 outputs | 27.7% | 527 | 40 | 94k |

Pruning without caps is weak because each event costs about $1.30 and leaves
95-107k of messages, calls and reasoning behind. Caps + prune keeps 95% of
compactions away at a cost of about 5 points versus caps + compaction at 200k.
Whether the model works better with its reasoning and messages intact is not
measured here; if it wastes fewer trips the gap closes.

### Line-level context dedup

$2,428 (11.4%) of search+read+vcs output lines were already present in an
earlier output of the same session (read outputs 42%, vcs 24%, search 13%).
A hash of emitted lines could replace runs of already-seen lines with a
pointer, but the earlier copy may be stale, so it needs "unchanged since turn N"
semantics. Ceiling 11%, realistic maybe half.

### Evict stale outputs (age > 100 requests)

+1.9 points on top of 1+3 after paying rebuilds every 50 requests; +6.4 points
at age > 50. Each rebuild is a cache miss on everything after the first evicted
block. Ruled out for now in favour of prune-at-threshold.

### Diff and log format

Worth 1-2%. vcs/diff is $1,022 but only 12% of it is changed lines; 28% is
context lines and 57% is file content via `jj file show`. vcs/log is $995,
91% bodies (`jj status` 59%, `-p` 27%).

## 5. Rejected or low value

- Advisor reuse: 9% of spend, 3.5% re-reading the parent's files.
- Reasoning knobs: old reasoning 3.6%, output reasoning 9% of the bill.
- Errored requests: free.
- Per-role compaction thresholds: no gain over a flat value.
- Remote ssh batching: situational (16-18% in one week, 0-4% in others).

## 6. Caveats

- Savings are first-order replays of past sessions; quality effects (more
  compactions, capped outputs) are not modeled.
- 66% of requests are size-estimated (before attribution existed); about 6%
  error on tool outputs.
- The wait counterfactual keeps one trip per chain; earlier "31%" assumed all
  wait trips vanish.
- All prices are Rho's fallback rates; the endpoint reported zero cache writes.

## 7. Reproduce

```
python3 tools/costx.py analyze --since 30d --min-requests 20 --top 30 --jobs 16
python3 tools/costx_deep.py --since 30d
python3 tools/costx_hyp.py --since 30d
python3 tools/costx_rw.py --since 30d
python3 tools/costx_full.py --since 30d
python3 tools/costx.py show STEM --from 100 --to 140    # transcript viewer
```

## 8. Follow-ups found after the report (Sept 5)

- Add a real pty to exec_command. `crates/rho-tool-shell/src/lib.rs` uses `Stdio::piped()`, so rg falls back to per-line `path:line:content` output (heading mode is tty-only), and any tool that checks isatty behaves as if scripted. Codex allocates a pty for its exec tool. Interim: `RIPGREP_CONFIG_PATH` with `--heading`.
- `write_stdin` polls are the wait problem in disguise. Over 3 weeks: 90,845 empty-`chars` write_stdin calls, 37,918 of them with `yield_time_ms: 1000`. Pure-poll trips are 18.4% of meter points (1 s polls 8.1%, 30 s+ polls 8.8%, 10 s polls 1.8%); 76% of those trips return under 400 chars. The `wait` tool text says "at least 30000 ms", but `write_stdin`'s `yield_time_ms` has no documented default or floor, so the model polls every second. Fix alongside step 3: floor `yield_time_ms` on empty-chars write_stdin at 30 s (or block until output/exit), and clamp its `max_output_tokens` to 10k.
- Build/test commands: exec calls that start builds are dominated by `nix develop -c ...` (2,753), `make` (2,498), `cargo test` (2,189), `just` (1,394), `nix build` (1,257). Heavy concentration: one session holds 56% of all `nix build` calls, one holds 95% of `nix develop > make > gcc`. `cargo fmt` alone produced 1.25M output tokens (cap it: exit code plus first 50 lines).

### Todo list as of Sept 5 (ordered by points per effort)

1. Agent2 as the default loop: exec cell as ToolSession, child processes attached to the exec call, `None`-until-exit haste, default check-in 120 s+, `wake_after_ms` on exec, empty-chars write_stdin blocks until output/exit (300 s). Poll chains ~21% of points. Interim until it lands: Codex clamps (empty polls 5–300 s, exec 250 ms–30 s, 1 s grace).
2. Per-family output caps at the drain, applied per chained shell command (75% of single exec calls chain 2+ commands), plus exec-object-as-text rendering. ~23% alone, overlaps with 1.
3. Compaction at 200k; evict tool outputs older than 50 requests when projecting the request input (rebuild cache prefix in batches). +5–6 points.
4. Drop luna: 9–10% of points.
5. Scout contexts: run lookup chains of 3+ in a fresh 20–30k sol context, return a short answer. 69% of lookup trips are in such chains (~20% of points); net ~12–14%.
6. Rho tool socket + forks: cargo (facts: first error, phase, done; digest; rustfmt/direnv/progress noise gone), rg (heading, per-file cap, first line + line numbers, top-hit region attached; ~13% of today's size), jj (context 1). Multi-file/range `read`.
7. Batching hint in the exec description ("independent lookups in one script"): 64% of lookup follow-ups are plannable, realistic ~5% of trips.
8. pty in the shell tool.
9. Per-process output logging (chunk timestamps, tail shape) to tune haste rules.
10. Effort: find which roles run xhigh; default medium. Terra inherits 1–3.
11. Giant sessions (turns >100 trips hold 46% of trips): scope spawned engineers smaller or force a handoff.

Not a lever: mail patience. Simulated 1 s burst / 2 s patience merges 0.3% of mail-caused requests (median gap between messages to the same agent is 111 s). Comms cost is the message count (~4% of points), fix on the sending side.

### Done Sept 5 (first cut of todo 1)

- `rho-tool-shell`: `exec_command` now waits for the command by default (300 s ceiling, explicit values clamped to 250 ms..300 s). An empty `write_stdin` blocks until the process prints more or exits (floor 30 s, ceiling 300 s, returns 200 ms after the first new output); a real write yields after 10 s (at most 30 s). Schema text says so.
- `rho-code-mode`: `exec` and `wait` default to 300 s (`wait` floor stays 30 s); yields of 10 s or more get 1 s of grace so a nested command and the cell do not yield at the same instant.
- `rho-agent` loop (formerly `rho-agent2`, now the only Rho runtime): `wait` is a core-owned tool (`{"seconds": N}`, 1..3600), answered at the next drain, and sets `ModelAsked::Wait`; the default check-in is 120 s (was 10 s) and partial output waits 300 s (was 60 s).
- `rho-agent-tools` (formerly `rho-agent2-tools`): the real tools as loop sources. `exec_command` stays attached to its process for life (silent until exit; `Soon` on crash/test-verdict/server-up/prompt lines; later output arrives as `ToolUpdate`s on the same call); `write_stdin` only types, never polls; `apply_patch`, `view_image`, `web__run` and the collaboration tools are one-shot futures; code mode's `exec` cell is a session too (`notify`/`yield_control` mark it `Soon`, completion `Ended`), and code mode's own cell `wait` is not offered.
- The old agent-1 loop is gone: every Rho agent runs on this loop, with presentation (titles/activity), usage accounting, rewind and role changes wired into it. `AgentRuntime::Rho2` and the `rho-agent2.*` tables are gone (never used outside the experiment). The `wait_agent` collaboration tool is gone too; the core `wait` replaces it. Gemini (`eng-gemini`) agents cannot be created: the loop does not speak the reduced Antigravity transcript protocol.
