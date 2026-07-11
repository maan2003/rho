// Session bootstrap for the rho code-mode runtime. Evaluated once per
// session (plain script, not REPL mode), then invoked with the nested tool
// metadata: `__rhoInit([{ name, global_name, description, kind }, ...])`.
//
// Cell attribution: every op call must know which exec cell it belongs to,
// including after `await` suspension points. V8 promise hooks propagate the
// current cell across reactions: promises created while a cell is current are
// tagged with it, and before/after hooks restore the tag around every
// reaction. `__rhoBeginCell(id)` is evaluated right before each cell's
// REPL-mode evaluation is dispatched, so the synchronous prefix is attributed
// too.
"use strict";

globalThis.__rhoInit = (toolList) => {
  delete globalThis.__rhoInit;
  const ops = Deno.core.ops;

  let current = 0; // 0 = not inside any cell
  const owner = new WeakMap();
  const stack = [];
  ops.op_set_promise_hooks(
    (promise, parent) => {
      const cell = parent !== undefined ? (owner.get(parent) ?? current) : current;
      if (cell !== 0 && cell !== undefined) owner.set(promise, cell);
    },
    (promise) => {
      stack.push(current);
      const cell = owner.get(promise);
      if (cell !== undefined) current = cell;
    },
    () => {
      current = stack.length > 0 ? stack.pop() : 0;
    },
    () => {},
  );

  globalThis.__rhoBeginCell = (id) => {
    current = id;
  };

  const stringify = (value) => {
    if (typeof value === "string") return value;
    if (value === undefined) return "undefined";
    const encoded = JSON.stringify(value);
    return encoded === undefined ? String(value) : encoded;
  };

  const tools = Object.create(null);
  const allTools = [];
  for (const tool of toolList) {
    allTools.push({ name: tool.global_name, description: tool.description });
    tools[tool.global_name] = (input) => {
      let args;
      if (tool.kind === "function") {
        if (input === undefined) {
          args = "{}";
        } else if (typeof input === "object" && input !== null && !Array.isArray(input)) {
          args = JSON.stringify(input);
        } else {
          return Promise.reject(
            new Error(`tool \`${tool.name}\` expects a JSON object for arguments`),
          );
        }
      } else {
        if (typeof input !== "string") {
          return Promise.reject(new Error(`tool \`${tool.name}\` expects a string input`));
        }
        args = input;
      }
      return ops.op_call_tool(current, tool.name, args).then((value) => JSON.parse(value));
    };
  }

  let nextTimer = 1;
  const liveTimers = new Set();

  Object.assign(globalThis, {
    tools,
    ALL_TOOLS: allTools,
    text: (value) => ops.op_text(current, stringify(value)),
    notify: (value) => ops.op_notify(current, stringify(value)),
    yield_control: () => ops.op_yield(current),
    exit: () => {
      ops.op_exit(current);
      throw { __rhoExit: true };
    },
    setTimeout: (callback, delayMs = 0) => {
      const id = nextTimer++;
      liveTimers.add(id);
      ops.op_sleep(current, Math.max(0, Number(delayMs) || 0)).then(
        (fired) => {
          if (fired && liveTimers.delete(id)) callback();
        },
        () => liveTimers.delete(id),
      );
      return id;
    },
    clearTimeout: (id) => {
      liveTimers.delete(id);
    },
  });

  for (const name of ["console", "Atomics", "SharedArrayBuffer", "WebAssembly"]) {
    delete globalThis[name];
  }
};
