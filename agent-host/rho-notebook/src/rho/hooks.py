"""The policy points. Each is a plain attribute holding one function,
replaced by assignment; a hook may be sync or async. `rho.default` installs
the defaults before init.py runs.

Rust calls these, on the workset thread unless noted:

- `instructions(agent) -> str`: the model's instructions, before each request.
- `on_notebook(agent, ns)`: the agent's notebook starts. Runs on that
  notebook's thread: put the model's tools in `ns` and `os.chdir` to the
  agent's directory.
- `on_compaction(agent) -> list[str]`: texts given back to the model after
  a compaction.
- `on_turn_end(agent, outcome)`: the model's turn ended. `outcome` is
  `{"reason": "end_turn" | "archived" | "failed", "error": str | None}`.

rho itself calls:

- `deliver(agent)`, async: hands the agent its chat messages for as long as
  the process runs. It starts once for each agent of the workset after
  init.py ran, and once for each agent `rho.agents.create` makes later. Only
  Python joins agents to topics, so this is also where agents join them.
- `on_slack(agent, event)`: a Slack event the workset's routes give the
  agent (`rho.slack`), or `{"type": "rho_truncated"}` for the workset's
  owner when the host lost events. It decides what the agent is given, and
  with which role.
"""


def _unset(*_args):
    raise NotImplementedError("no policy installed")


instructions = _unset
on_notebook = _unset
on_compaction = _unset
on_turn_end = _unset
deliver = _unset
on_slack = _unset

NAMES = ("instructions", "on_notebook", "on_compaction", "on_turn_end", "deliver", "on_slack")
