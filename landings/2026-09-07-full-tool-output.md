# Full tool output survives on the host

Code-mode's 10,000-token model-facing limit remains unchanged. A code-mode
result now carries two deliberately different views: `output` is the existing
bounded status/wall-time rendering replayed to inference, while `full_output`
is the unwrapped complete text persisted in the tool result. Old records decode
with no `full_output` and continue to expose their original `output`.

The daemon's Detail path chooses the complete record when present, so expanding
a tool result shows what the tool actually said. Provider adapters continue to
read only the bounded `output`; a request serialization test fixes that
boundary. Both the standalone code-mode session and the daemon's production
`rho-agent-tools` code-mode session preserve the complete body before applying
the model budget. Later contributions from yielded cells retain the same second
view on their `ToolUpdate`, and Detail exposes those update records too.

The exact heavy-tail proof ran in a loopback-only network namespace:

```text
PROOF_READY tree_commit=131a88e25649b76458df76b9f8fd9a86433ef00a proof_sha256=a09d7621ec63dd6cfe5d2e212e67bc78726f95aad5cf24f6359b089b7712d7ab fake_sha256=93f185e6d784ba71c305efc65547b396d07b6d6357e565e42bf03b7482d25906 scenario=huge-tool-output seed=0
scenario=huge-tool-output agents=1 duration_s=10 replies=2 failures=0 retrying_failures=0 tool_calls=100 compacted=0 clarifying=0 results=100 result_bytes=404700 result_p50=227 result_mean=4047 result_p90=13097 result_max=170448 model_result_max=40077 turns_per_sec=2.13 fake_completed_turns=3 fake_bytes=3231189 sent_replied_p50_ms=105 sent_replied_p99_ms=1073 fake_vmrss_kib=29196 daemon_vmrss_kib=140660 journal_head=8
```

Thus the host record is the requested population exactly: p50 227 bytes, mean
4,047, p90 13,097, max 170,448. The largest text replayed to the model remains
40,077 bytes including code-mode's envelope and truncation notice. Persisting
the second view adds 404,700 logical payload bytes for these 100 results: an
average storage delta of 4,047 bytes per result, plus the optional-field framing
in the record encoding. That duplication is intentional: replay must preserve
the exact historical model view while the reader must retain the exact tool
record.

Detail is still transported as one UI protocol frame. The proven 170,448-byte
maximum is safely below that protocol's 64 MiB frame limit; outputs beyond the
frame limit remain durable but would need a separate chunked-reader change to
be displayed whole.
