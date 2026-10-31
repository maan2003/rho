"""A workset process in a test: `_main` on its own thread over a fresh
fake `_rho`."""

import asyncio
import os
import tempfile
import threading
import time
import unittest

from . import fake_rho

import rho
from rho import _boot, hooks, workset


def wait_for(predicate, what, timeout=5.0):
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out waiting for {what}")
        time.sleep(0.005)


class Workset:
    """`_main` running with `init` as init.py (None: no file)."""

    def __init__(self, init=None, seed=None):
        fake_rho.reset()
        if seed is not None:
            seed(fake_rho.STATE)
        _boot._announced.clear()
        for name in hooks.NAMES:
            setattr(hooks, name, hooks._unset)
        workset.namespace.clear()
        workset.namespace["__name__"] = "__rho_workset__"
        self.dir = tempfile.TemporaryDirectory()
        path = None
        if init is not None:
            path = os.path.join(self.dir.name, "init.py")
            with open(path, "w") as file:
                file.write(init)
        self.ready = []
        self.thread = threading.Thread(target=rho._main, args=(path, self.ready.append), daemon=True)
        self.thread.start()
        wait_for(lambda: self.ready, "ready")

    @property
    def state(self):
        return fake_rho.STATE

    def stop(self):
        loop = _boot._loop

        def shutdown():
            for task in asyncio.all_tasks(loop):
                task.cancel()
            loop.call_soon(loop.stop)

        loop.call_soon_threadsafe(shutdown)
        self.thread.join(5)
        loop.close()
        self.dir.cleanup()


class WorksetCase(unittest.TestCase):
    init = None

    def seed(self, state):
        pass

    def setUp(self):
        self.ws = Workset(self.init, self.seed)
        self.addCleanup(self.ws.stop)

    @property
    def state(self):
        return fake_rho.STATE


def native(model="sol", effort="high"):
    return {"kind": "native", "model": model, "effort": effort, "fast": False}


def claude(model="opus", effort="medium"):
    return {"kind": "claude", "model": model, "effort": effort}


def add_agent(state, runtime, **props):
    id = f"agent{next(state.ids)}"
    state.agents[id] = {"runtime": runtime, "props": props}
    return id
