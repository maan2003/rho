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

- `on_agent(agent)`: the agent exists: once for each agent of the workset
  after init.py ran, and once for each agent `rho.agents.create` makes
  later.
"""


def _unset(*_args):
    raise NotImplementedError("no policy installed")


instructions = _unset
on_notebook = _unset
on_compaction = _unset
on_turn_end = _unset
on_agent = _unset

NAMES = ("instructions", "on_notebook", "on_compaction", "on_turn_end", "on_agent")
