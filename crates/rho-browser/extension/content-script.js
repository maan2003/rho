(() => {
  "use strict";

  const HINT_CHARS = "fjdklsahgurieowcnmv";
  const MODE_ID = "__rho_vim_mode";

  let hints;
  let hintGeneration = 0;
  let mode = "normal";
  let count = 0;
  let prefixG = 0;
  let nextHintGeneration = 0;
  const keyDispositions = new Map();

  function activeElement(root = document) {
    let element = root.activeElement;
    while (element?.shadowRoot?.activeElement) {
      element = element.shadowRoot.activeElement;
    }
    return element;
  }

  function isTypingElement(element) {
    if (!element) return false;
    if (element.isContentEditable) return true;
    if (element.matches?.("textarea, select")) return true;
    return element.matches?.(
      'input:not([type]), input[type="text"], input[type="search"], input[type="tel"], input[type="url"], input[type="email"], input[type="password"], input[type="number"]',
    );
  }

  function isAdjustableElement(element) {
    return element?.matches?.(
      "input, button, video, audio, embed, object",
    );
  }

  function showBadge(label) {
    document.getElementById(MODE_ID)?.remove();
    const badge = document.createElement("div");
    badge.id = MODE_ID;
    badge.textContent = label;
    Object.assign(badge.style, {
      all: "initial",
      position: "fixed",
      right: "12px",
      bottom: "12px",
      zIndex: "2147483647",
      padding: "4px 8px",
      color: "#fff",
      background: "#b33",
      borderRadius: "3px",
      font: "bold 12px monospace",
      pointerEvents: "none",
    });
    document.documentElement.appendChild(badge);
  }

  function setModeIndicator(next) {
    if (next === "ignore") {
      showBadge("IGNORE");
    } else {
      document.getElementById(MODE_ID)?.remove();
    }
  }

  function walkRoots(root = document, roots = []) {
    roots.push(root);
    for (const element of root.querySelectorAll("*")) {
      if (element.shadowRoot) walkRoots(element.shadowRoot, roots);
    }
    return roots;
  }

  function actionableElements() {
    const selector = [
      "a[href]",
      "button",
      "input:not([type='hidden'])",
      "select",
      "textarea",
      "[contenteditable]:not([contenteditable='false'])",
      "[role='button']",
      "[role='link']",
      "[onclick]",
      "[tabindex]:not([tabindex='-1'])",
    ].join(",");
    const seen = new Set();
    const elements = [];
    for (const root of walkRoots()) {
      for (const element of root.querySelectorAll(selector)) {
        if (seen.has(element)) continue;
        seen.add(element);
        const style = getComputedStyle(element);
        if (style.visibility === "hidden" || style.display === "none") continue;
        const rects = [...element.getClientRects()];
        const rect = rects.find(
          (candidate) =>
            candidate.width > 0 &&
            candidate.height > 0 &&
            candidate.bottom > 0 &&
            candidate.right > 0 &&
            candidate.top < innerHeight &&
            candidate.left < innerWidth,
        );
        if (!rect) continue;
        const x = Math.max(0, Math.min(innerWidth - 1, rect.left + 1));
        const y = Math.max(0, Math.min(innerHeight - 1, rect.top + rect.height / 2));
        const covering =
          root.elementFromPoint?.(x, y) ?? element.ownerDocument.elementFromPoint(x, y);
        if (covering && covering !== element && !element.contains(covering)) continue;
        elements.push({
          element,
          rect,
          weight: Math.max(1, rect.width * rect.height),
        });
      }
    }
    return elements;
  }

  class Branch {
    constructor(children, weight) {
      this.children = children;
      this.weight = weight;
    }

    assign(alphabet, callback, prefix = "") {
      [...this.children].reverse().forEach((node, index) => {
        const word = prefix + alphabet[index];
        if (node instanceof Branch) node.assign(alphabet, callback, word);
        else callback(node, word);
      });
    }
  }

  // Adapted from VimFx's MIT-licensed n-ary Huffman implementation. It gives
  // likely targets short labels instead of fixed-width base-N numbers.
  function hintTree(elements, branches) {
    if (elements.length === 0) return new Branch([], 0);
    if (elements.length === 1) return new Branch([elements[0]], elements[0].weight);
    const sorted = [...elements].sort((a, b) => b.weight - a.weight);
    const pointsCount = Math.ceil((sorted.length - 1) / (branches - 1));
    const padding = 1 + (branches - 1) * pointsCount - sorted.length;
    const points = new Array(pointsCount);
    let latest = 0;
    let pointIndex = 0;
    let elementIndex = sorted.length - 1;
    if (padding > 0) {
      const children = [];
      let weight = 0;
      for (let i = 0; i < branches - padding; i += 1) {
        const element = sorted[elementIndex--];
        children.push(element);
        weight += element.weight;
      }
      points[0] = new Branch(children, weight);
      latest = 1;
    }
    let nextElement = elementIndex >= 0 ? sorted[elementIndex] : null;
    while (latest < pointsCount) {
      const children = [];
      let weight = 0;
      let nextPoint = points[pointIndex];
      for (let i = 0; i < branches; i += 1) {
        let lowest;
        if (!nextElement || (nextPoint && nextPoint.weight <= nextElement.weight)) {
          lowest = nextPoint;
          pointIndex += 1;
          nextPoint = points[pointIndex];
        } else {
          lowest = nextElement;
          elementIndex -= 1;
          nextElement = elementIndex >= 0 ? sorted[elementIndex] : null;
        }
        children.push(lowest);
        weight += lowest.weight;
      }
      points[latest++] = new Branch(children, weight);
    }
    return points[pointsCount - 1];
  }

  function clearHints() {
    hints?.host.remove();
    hints = undefined;
    hintGeneration = 0;
    if (mode === "hints") mode = "normal";
  }

  function activateHint(item) {
    clearHints();
    if (isTypingElement(item.element) || item.element.matches("select")) {
      item.element.focus();
      item.element.select?.();
    } else if (typeof item.element.click === "function") {
      item.element.click();
    } else {
      item.element.dispatchEvent(
        new MouseEvent("click", { bubbles: true, cancelable: true, composed: true }),
      );
    }
    return { done: true };
  }

  function updateHints() {
    if (!hints) return { done: true };
    const matches = hints.items.filter((item) => item.hint.startsWith(hints.input));
    for (const item of hints.items) {
      item.marker.style.display = matches.includes(item) ? "block" : "none";
      item.marker.textContent = item.hint;
      if (hints.input) {
        const matched = document.createElement("b");
        matched.textContent = hints.input;
        item.marker.textContent = "";
        item.marker.append(matched, item.hint.slice(hints.input.length));
      }
    }
    if (matches.length === 1 && matches[0].hint === hints.input) {
      return activateHint(matches[0]);
    }
    if (matches.length === 0) {
      clearHints();
      return { done: true };
    }
    return { done: false };
  }

  function enterHints(generation) {
    const items = actionableElements();
    if (items.length === 0) return { done: true };
    clearHints();
    hintGeneration = generation;
    hintTree(items, HINT_CHARS.length).assign(HINT_CHARS, (item, hint) => {
      item.hint = hint;
    });

    const host = document.createElement("div");
    host.id = "__rho_vim_hints";
    Object.assign(host.style, {
      all: "initial",
      position: "fixed",
      inset: "0",
      zIndex: "2147483647",
      pointerEvents: "none",
    });
    const shadow = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = `
      span {
        position: fixed;
        box-sizing: border-box;
        padding: 1px 3px;
        border: 1px solid #9b6a00;
        border-radius: 2px;
        color: #241a00;
        background: #ffd76a;
        box-shadow: 0 1px 2px #0008;
        font: bold 12px/1.2 monospace;
        text-transform: uppercase;
      }
      b { color: #b00020; }
    `;
    shadow.append(style);
    for (const item of items) {
      const marker = document.createElement("span");
      marker.textContent = item.hint;
      marker.style.left = `${Math.max(0, Math.round(item.rect.left))}px`;
      marker.style.top = `${Math.max(0, Math.round(item.rect.top))}px`;
      item.marker = marker;
      shadow.append(marker);
    }
    document.documentElement.appendChild(host);
    hints = { host, items, input: "" };
    return { done: false };
  }

  function handleHintKey(key, generation) {
    if (!hints || hintGeneration !== generation) return { done: true };
    if (key === "backspace") {
      hints.input = hints.input.slice(0, -1);
      return updateHints();
    }
    if (key.length === 1 && HINT_CHARS.includes(key.toLowerCase())) {
      hints.input += key.toLowerCase();
      return updateHints();
    }
    return { done: false };
  }

  function handleCommand(message) {
    const amount = Math.max(1, Math.min(Number(message.value) || 1, 9999));
    switch (message.command) {
      case "scroll-left":
        window.scrollBy({ left: -80 * amount, behavior: "instant" });
        return {};
      case "scroll-down":
        window.scrollBy({ top: 80 * amount, behavior: "instant" });
        return {};
      case "scroll-up":
        window.scrollBy({ top: -80 * amount, behavior: "instant" });
        return {};
      case "scroll-right":
        window.scrollBy({ left: 80 * amount, behavior: "instant" });
        return {};
      case "half-down":
        window.scrollBy({ top: (innerHeight / 2) * amount, behavior: "instant" });
        return {};
      case "half-up":
        window.scrollBy({ top: -(innerHeight / 2) * amount, behavior: "instant" });
        return {};
      case "page-down":
        window.scrollBy({ top: innerHeight * amount, behavior: "instant" });
        return {};
      case "page-up":
        window.scrollBy({ top: -innerHeight * amount, behavior: "instant" });
        return {};
      case "top":
        window.scrollTo({ top: 0, behavior: "instant" });
        return {};
      case "bottom":
        window.scrollTo({ top: document.documentElement.scrollHeight, behavior: "instant" });
        return {};
      case "focus-input": {
        const inputs = actionableElements().filter(({ element }) => isTypingElement(element));
        const input = inputs.at(-1)?.element;
        input?.focus();
        input?.select?.();
        return {};
      }
      default:
        return {};
    }
  }

  function stopEvent(event) {
    event.preventDefault();
    event.stopImmediatePropagation();
  }

  function consume(event, repeatCommand) {
    keyDispositions.set(event.code, { consumed: true, repeatCommand });
    stopEvent(event);
  }

  function runCommand(command, amount) {
    if (command === "back" || command === "forward" || command === "reload") {
      chrome.runtime
        .sendMessage({ type: "rho-browser-command", command })
        .catch(() => {});
    } else {
      handleCommand({ command, value: String(amount) });
    }
  }

  function onKeyDown(event) {
    if (!event.isTrusted) return;
    const disposition = keyDispositions.get(event.code);
    if (event.repeat && disposition) {
      if (disposition.consumed) {
        if (disposition.repeatCommand) runCommand(disposition.repeatCommand, 1);
        stopEvent(event);
      }
      return;
    }
    keyDispositions.set(event.code, { consumed: false });

    const key = event.key.toLowerCase();
    const plain = !event.ctrlKey && !event.altKey && !event.metaKey;

    if (mode === "ignore") {
      if (plain && event.shiftKey && key === "escape") {
        mode = "normal";
        setModeIndicator("normal");
        consume(event);
      }
      return;
    }

    if (mode === "hints") {
      if (!plain) return;
      if (key === "escape") {
        clearHints();
        consume(event);
        return;
      }
      if (key !== "backspace" && (key.length !== 1 || !HINT_CHARS.includes(key))) {
        return;
      }
      const result = handleHintKey(key, hintGeneration);
      if (result.done) mode = "normal";
      consume(event);
      return;
    }

    const focused = activeElement();
    if (!plain || isTypingElement(focused) || isAdjustableElement(focused)) {
      count = 0;
      prefixG = 0;
      return;
    }

    if (prefixG && performance.now() - prefixG > 1000) {
      prefixG = 0;
      count = 0;
    }
    if (prefixG) {
      prefixG = 0;
      const amount = Math.max(1, count);
      count = 0;
      if (!event.shiftKey && key === "g") {
        runCommand("top", amount);
      } else if (!event.shiftKey && key === "i") {
        runCommand("focus-input", amount);
      } else {
        return;
      }
      consume(event);
      return;
    }

    if (!event.shiftKey && /^[0-9]$/.test(key) && (key !== "0" || count > 0)) {
      count = Math.min(9999, count * 10 + Number(key));
      consume(event);
      return;
    }
    if (!event.shiftKey && key === "g") {
      prefixG = performance.now();
      consume(event);
      return;
    }

    const amount = Math.max(1, count);
    count = 0;
    let command;
    if (event.shiftKey) {
      command = { " ": "page-up", g: "bottom", h: "back", l: "forward" }[key];
    } else {
      command = {
        h: "scroll-left",
        j: "scroll-down",
        k: "scroll-up",
        l: "scroll-right",
        d: "half-down",
        u: "half-up",
        " ": "page-down",
        r: "reload",
      }[key];
    }
    if (command) {
      runCommand(command, amount);
    } else if (!event.shiftKey && key === "i") {
      mode = "ignore";
      setModeIndicator("ignore");
    } else if (!event.shiftKey && key === "f") {
      nextHintGeneration = (nextHintGeneration + 1) || 1;
      const result = enterHints(nextHintGeneration);
      mode = result.done ? "normal" : "hints";
    } else {
      return;
    }
    const repeatCommand = command !== "back" && command !== "forward" && command !== "reload"
      ? command
      : undefined;
    consume(event, repeatCommand);
  }

  addEventListener("keydown", onKeyDown, true);
  addEventListener("keyup", (event) => {
    if (!event.isTrusted) return;
    const disposition = keyDispositions.get(event.code);
    keyDispositions.delete(event.code);
    if (disposition?.consumed) stopEvent(event);
  }, true);
  addEventListener("blur", (event) => {
    if (event.isTrusted) keyDispositions.clear();
  }, true);
  for (const type of ["pagehide", "resize", "scroll"]) {
    addEventListener(type, (event) => {
      if (event.isTrusted) clearHints();
    }, true);
  }
})();
