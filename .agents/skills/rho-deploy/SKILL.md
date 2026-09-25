---
name: rho-deploy
description: Use after landing a rho change on main that the running agent host should pick up, to ask the deployer to deploy it and to read its reply.
---

# Requesting a rho deploy

The agent host you run on is deployed by a separate deployer agent on its own
agent host, which a deploy never restarts. Ask it with:

```sh
rho-deploy '<what landed and what to watch for>'
```

Use `rho-deploy`, not `rho debug send-message`: the `rho` beside you belongs
to the host being replaced and may not speak to the deployer's host.
`rho-deploy` uses the deployer's own `rho`.

## What happens

- Push to `origin/main` first; the deployer builds its head, coalescing every
  pending request into one deploy.
- End your turn after asking. The deploy restarts the host you run on: it
  lets requests in flight finish (up to 50s), then stops, and wakes the agents
  that were still at work. Running commands and Python state do not survive.
- The deployer's reply wakes you: either the rev now running, or that it
  rolled back and why, with the log lines it judged by. After a rollback, the
  fix is yours; land it and ask again.
- A change to the protocol epoch (`IROH_ALPN` in `rho_rpc::protocol`) also
  needs a GUI release, which the user does; the reply says so. Tell the user.
