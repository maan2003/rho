"""The agents of this workset."""

import _rho

#: The values the default policy gives the `preset` prop.
PRESETS = ("engineer", "child", "advisor")


class Agent:
    """An agent of this workset, by id. Every read goes to `_rho`, which
    owns the spec."""

    __slots__ = ("id",)

    def __init__(self, id: str):
        if id not in _rho.agents():
            raise KeyError(f"no agent {id} in this workset")
        object.__setattr__(self, "id", id)

    def __setattr__(self, name, value):
        raise AttributeError("an Agent is only its id")

    def __eq__(self, other):
        return isinstance(other, Agent) and other.id == self.id

    def __hash__(self):
        return hash(self.id)

    def __repr__(self):
        return f"rho.agent({self.id!r})"

    @property
    def spec(self) -> dict:
        return _rho.agents()[self.id]

    @property
    def runtime(self) -> dict:
        return self.spec["runtime"]

    @property
    def props(self) -> dict:
        return self.spec["props"]

    def set_spec(self, runtime: dict, props: dict) -> None:
        """Replace the whole spec."""
        _rho.set_spec(self.id, {"runtime": runtime, "props": props})

    def set_runtime(self, runtime: dict) -> None:
        """Replace the runtime; the props stay."""
        self.set_spec(runtime, self.props)

    def set_props(self, props: dict) -> None:
        """Replace the props; the runtime stays."""
        self.set_spec(self.runtime, props)

    def input(self, text: str, *, wake: str, mail: int | None = None) -> None:
        """Queue `text` for the model; returns once its `Input` row is
        durable. `mail` names the mail it hands over."""
        _rho.input(self.id, text, wake=wake, mail=mail)

    def cancel(self) -> None:
        _rho.cancel(self.id)

    def compact(self) -> None:
        _rho.compact(self.id)

    def rewind(self, turns: int) -> None:
        _rho.rewind(self.id, turns)

    def new_cache_key(self) -> None:
        _rho.new_cache_key(self.id)


def create(runtime: dict, props: dict) -> Agent:
    """Create an agent and start its loop. `hooks.on_agent` runs for it on
    the workset loop."""
    agent = Agent(_rho.create_agent({"runtime": runtime, "props": props}))
    from . import _boot

    _boot.announce_soon(agent.id)
    return agent


def all() -> list[Agent]:
    return [Agent(id) for id in _rho.agents()]
