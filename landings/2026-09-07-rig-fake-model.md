# A rig's model traffic is fake too

`rig up` used to isolate the copied store, Slack, browser, and Wayland session
but left the daemon's inference endpoints at their production defaults. It now
starts `rho-fake-model` first, waits for its JSON readiness record, writes a
synthetic OpenAI sign-in only under the rig's own `state/rho/auth.d`, and starts
the daemon with both provider base URLs pointed at that process. The same rig
environment sets `CLAUDE_CONFIG_DIR` below the rig root. `rig down` stops the
model with the rest of the session. No credential or Claude state from the user
is read or copied.

`rig probe` creates a fixture jj workspace inside the rig, sends one native
agent turn through the rig daemon, observes the final journal reply, and
requires the fake's own metrics to report a completed turn. It deliberately
replays from the pre-creation journal head after the fast fake has answered,
because `Follow` has no acknowledgement; sequence de-duplication makes that
replay exact.

The clean debug proof was one complete `rig new` → `rig up --no-gui` →
`rig probe` → `rig down` session:

```text
RIG_READY tree_commit=a7d51ff2cdec rho_qa_sha256=829eeac41a007d409645ec22141fc03f997e69b6bb5f46ec750d3ed5a71877de fake_sha256=f4070daa022f3aac8c8ad6fee50b9cafb6a2a7ae5bfaa83d094ec88fa7dcf24b daemon_sha256=7fd9f82f8d106882508b324ea1c70129bbf317208ee166f023592e0de5d26398 rig=fake-e2e session=1
RIG_PROOF agent=PrefixId("x7zeh41qsf2d") replies=2 fake_completed_turns=3 journal_head=7
```

The large-result finding in the preceding fake-model change is deliberate on
one side and data loss on the other. `rho-code-mode` gives direct `exec` output
a documented default model-facing budget of 10,000 approximate tokens (40,000
bytes), then adds the 47-byte status envelope. That cap is right for inference
context. Today formatting drains the cell's only full output before truncating
it, however, and the truncated `ToolOutput` is both fed and persisted. The full
result survives nowhere on the host. Keeping the full record while deriving
the bounded model view is a separate follow-up.
