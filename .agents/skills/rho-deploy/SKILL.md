---
name: rho-deploy
description: Use after landing a rho change on main that the running agent host or the user's phone GUI should pick up, to ask the deployer to deploy it and to read its reply.
---

# Requesting a rho deploy

The agent host you run on, and the rho GUI on the user's phone, are deployed
by a separate deployer agent on its own agent host, which a deploy never
restarts. Ask it with:

```sh
rho-deploy 'host: <what landed and what to watch for>'
```

Start the note with what to deploy:

- `host:` the running agent host should behave differently: code under
  `agent-host/`, or a shared crate in `crates/` whose change the host uses.
- `gui:` the GUI should look or behave differently: `rho-gui` and the
  crates it draws or connects with. A GUI release is not needed for code
  only the host runs.
- `both:` a change to both sides, and always when you bump the protocol
  epoch (`IROH_ALPN` in `rho_rpc::protocol`). The deployer releases the GUI
  first, since a GUI and a host on different epochs cannot talk.

Skip the request for changes neither side runs: docs, tests, CI, tooling.

Use `rho-deploy`, not `rho debug send-message`: the `rho` beside you belongs
to the host being replaced and may not speak to the deployer's host.
`rho-deploy` uses the deployer's own `rho`.

## What happens

- Push to `origin/main` first; the deployer builds its head, coalescing every
  pending request into one deploy.
- For `gui:` the reply comes when the new GUI is installed on the phone;
  the user reopens rho when ready. Nothing on your host restarts.
- For `host:` and `both:`, end your turn after asking. The deployer
  usually re-executes the host you run on in place: it finishes requests in
  flight, then swaps its binary, and your workset keeps running commands and
  Python state. Your workset keeps its old worker code until it restarts.
  A protocol change needs a full restart instead: it kills every workset,
  with its running commands and Python state, and wakes no one by itself.
- The deployer's reply wakes you: either the rev now running, or that it
  rolled back and why, with the log lines it judged by. After a rollback, the
  fix is yours; land it and ask again.
