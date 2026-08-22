(() => {
  "use strict";

  // VimFx v0.27.5 defaults. VimFx separates the final two characters into a
  // secondary alphabet; the Chromium agent currently uses the combined set.
  const HINT_CHARS = "fjdkslaghrueiwoncmv";
  const MODE_ID = "__rho_vim_mode";
  const INPUT_TIMEOUT_MS = 2000;
  const REPEAT_TIMEOUT_MS = 65;
  const HINT_TEXT_TIMEOUT_MS = 400;

  let hints;
  let hintGeneration = 0;
  let nextHintGeneration = 0;
  let mode = "normal";
  let ignoreRemaining;
  let currentKeyTree;
  let count = "";
  let lastInputTime = 0;
  let lastScrollRepeat = 0;
  let lastFocusedTextInput;
  let hasFocusedTextInput = false;
  let focusInputs;
  let hintRefreshPending = false;
  let ignoreKeyEventsUntil = 0;
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

  function isActivatableElement(element) {
    return element?.matches?.(
      'a, button, input[type="button"], input[type="submit"], input[type="reset"], input[type="image"]',
    );
  }

  function isAdjustableElement(element) {
    return element?.matches?.(
      'input[type="checkbox"], input[type="radio"], input[type="file"], input[type="color"], input[type="date"], input[type="time"], input[type="datetime-local"], input[type="month"], input[type="week"], video, audio, embed, object',
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

  function setMode(next) {
    mode = next;
    if (next === "ignore" && ignoreRemaining === undefined) {
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

  function deepParent(element) {
    return element?.parentElement ?? element?.getRootNode?.().host ?? null;
  }

  function elementRect(element) {
    const style = getComputedStyle(element);
    if (style.visibility === "hidden" || style.display === "none") return undefined;
    return [...element.getClientRects()].find(
      (rect) =>
        rect.width > 0 &&
        rect.height > 0 &&
        rect.bottom > 0 &&
        rect.right > 0 &&
        rect.top < innerHeight &&
        rect.left < innerWidth,
    );
  }

  function isScrollable(element, axis) {
    if (!element) return false;
    if (element === document.scrollingElement) {
      return axis === "x"
        ? element.scrollWidth > element.clientWidth
        : element.scrollHeight > element.clientHeight;
    }
    const style = getComputedStyle(element);
    const overflow = axis === "x" ? style.overflowX : style.overflowY;
    if (!/(auto|scroll|overlay)/.test(overflow)) return false;
    return axis === "x"
      ? element.scrollWidth > element.clientWidth
      : element.scrollHeight > element.clientHeight;
  }

  function containsDeep(parent, child) {
    for (let element = child; element; element = deepParent(element)) {
      if (element === parent) return true;
    }
    return false;
  }

  // Port of VimFx's ScrollableElements. ResizeObserver replaces Firefox's
  // former overflow/underflow events and keeps the default target current as
  // layouts and application shells change.
  class ScrollableElements {
    constructor() {
      this.elements = new Set();
      this.largest = null;
      this.resizeObserver = new ResizeObserver((entries) => {
        for (const entry of entries) this.addChecked(entry.target);
      });
      this.mutationObservers = [];
      const initialize = () => this.observeRoot(document);
      if (document.documentElement) initialize();
      else addEventListener("DOMContentLoaded", initialize, { once: true });
    }

    observeRoot(root) {
      const observeTree = (element) => {
        this.resizeObserver.observe(element);
        this.addChecked(element);
        if (element.shadowRoot) this.observeRoot(element.shadowRoot);
        for (const child of element.querySelectorAll?.("*") ?? []) {
          this.resizeObserver.observe(child);
          this.addChecked(child);
          if (child.shadowRoot) this.observeRoot(child.shadowRoot);
        }
      };
      if (root.documentElement) observeTree(root.documentElement);
      else for (const child of root.children) observeTree(child);

      const observer = new MutationObserver((mutations) => {
        for (const mutation of mutations) {
          for (const node of mutation.addedNodes) {
            if (node.nodeType === Node.ELEMENT_NODE) observeTree(node);
          }
          for (const node of mutation.removedNodes) {
            if (node.nodeType === Node.ELEMENT_NODE) this.delete(node);
          }
        }
      });
      observer.observe(root, { childList: true, subtree: true });
      this.mutationObservers.push(observer);
    }

    isScrollable(element) {
      return Boolean(element?.isConnected) && (
        element.scrollHeight - element.clientHeight >= 5
        || element.scrollWidth - element.clientWidth >= 5
      );
    }

    addChecked(element) {
      if (!this.isScrollable(element)) {
        this.delete(element);
        return;
      }
      const style = getComputedStyle(element);
      if ((style.overflowX === "hidden" && style.overflowY === "hidden")
          || element.clientWidth * element.clientHeight < 25) {
        this.delete(element);
        return;
      }
      this.elements.add(element);
      if (this.isLargest(element)) this.largest = element;
    }

    delete(element) {
      this.elements.delete(element);
      if (this.largest === element) this.updateLargest();
    }

    has(element) {
      return this.elements.has(element);
    }

    isLargest(element) {
      if (!this.largest) return true;
      if (containsDeep(element, this.largest)) return true;
      if (containsDeep(this.largest, element)) return false;
      return element.clientWidth * element.clientHeight
        > this.largest.clientWidth * this.largest.clientHeight;
    }

    updateLargest() {
      this.largest = null;
      for (const element of this.elements) {
        if (!this.isScrollable(element)) this.elements.delete(element);
        else if (this.isLargest(element)) this.largest = element;
      }
    }

    filterSuitableDefault() {
      if (!this.isScrollable(this.largest)) this.updateLargest();
      return this.largest ?? document.scrollingElement;
    }
  }

  const scrollableElements = new ScrollableElements();

  function scrollTarget() {
    for (let element = activeElement(); element; element = deepParent(element)) {
      if (scrollableElements.has(element)) return element;
    }
    return scrollableElements.filterSuitableDefault();
  }

  function scrollViewportSize(element, axis) {
    if (element === document.scrollingElement) {
      return axis === "x" ? innerWidth : innerHeight;
    }
    return axis === "x" ? element.clientWidth : element.clientHeight;
  }

  function runScroll(command, amount, repeat) {
    if (repeat) {
      const now = performance.now();
      if (now - lastScrollRepeat < REPEAT_TIMEOUT_MS) return;
      lastScrollRepeat = now;
    }

    const horizontal = command.includes("left") || command.includes("right");
    const axis = horizontal ? "x" : "y";
    const direction = command.includes("left") || command.includes("up") || command === "top"
      ? -1
      : 1;
    const element = scrollTarget();
    if (!element) return;
    // VimFx temporarily sets Firefox's native spring constant here.
    // VIMFX PARITY TODO(rhoPrivate.vim):
    // prefs.root.set("layout.css.scroll-behavior.spring-constant", springConstant);
    // Until Brave exposes equivalent compositor tuning, use its standard
    // smooth scrolling rather than attempting a JavaScript animation loop.
    const behavior = "smooth";

    if (["top", "bottom", "far-left", "far-right"].includes(command)) {
      const options = { behavior };
      if (command === "top") options.top = 0;
      if (command === "bottom") options.top = element.scrollHeight;
      if (command === "far-left") options.left = 0;
      if (command === "far-right") options.left = element.scrollWidth;
      element.scrollTo(options);
      return;
    }

    const boost = repeat ? (horizontal ? 6 : 3) : 1;
    let distance;
    if (command.startsWith("half-")) {
      distance = Math.max(0, scrollViewportSize(element, axis) / 2 - 20);
    } else if (command.startsWith("page-")) {
      distance = Math.max(0, scrollViewportSize(element, axis) - 40);
    } else {
      distance = 80 * boost;
    }
    element.scrollBy({
      [axis === "x" ? "left" : "top"]: direction * distance * amount,
      behavior,
    });
  }

  function visibleTypingElements() {
    const elements = [];
    let order = 0;
    for (const root of walkRoots()) {
      for (const element of root.querySelectorAll(
        'input:not([type]), input[type="text"], input[type="search"], input[type="tel"], input[type="url"], input[type="email"], input[type="password"], input[type="number"], textarea, [contenteditable]:not([contenteditable="false"])',
      )) {
        if (!elementRect(element)) continue;
        elements.push({ element, tabIndex: element.tabIndex, order: order++ });
      }
    }
    elements.sort((a, b) => a.tabIndex - b.tabIndex || a.order - b.order);
    return elements.map(({ element }) => element);
  }

  function focusTextInput(requestedCount) {
    const inputs = visibleTypingElements();
    if (lastFocusedTextInput && !lastFocusedTextInput.isConnected) {
      lastFocusedTextInput = undefined;
    }
    if (lastFocusedTextInput && !inputs.includes(lastFocusedTextInput)) {
      inputs.push(lastFocusedTextInput);
    }
    if (inputs.length === 0) return;
    let index;
    if (requestedCount !== undefined) {
      index = Math.min(requestedCount, inputs.length) - 1;
    } else if (lastFocusedTextInput && inputs.includes(lastFocusedTextInput)) {
      index = inputs.indexOf(lastFocusedTextInput);
    } else {
      index = 0;
    }
    const input = inputs[index];
    const select = requestedCount !== undefined || !hasFocusedTextInput;
    input.focus();
    if (select) input.select?.();
    lastFocusedTextInput = input;
    focusInputs = inputs;
  }

  function moveInputFocus(direction) {
    if (!focusInputs || focusInputs.length <= 1) return false;
    const index = focusInputs.indexOf(activeElement());
    if (index < 0) return false;
    const input = focusInputs[(index + direction + focusInputs.length) % focusInputs.length];
    input.focus();
    input.select?.();
    lastFocusedTextInput = input;
    return true;
  }

  function escapeNormalMode() {
    const focused = activeElement();
    const hadFocusedControl = focused
      && focused !== document.body
      && focused !== document.documentElement;
    focused?.blur?.();
    getSelection()?.removeAllRanges();
    if (document.fullscreenElement) document.exitFullscreen?.();
    // VimFx lets Escape continue to the page when no control was focused, so
    // website dialogs and loading can still be cancelled. When it blurred a
    // control, it suppresses Escape to avoid also closing the surrounding UI.
    return Boolean(hadFocusedControl);
  }

  function elementText(element) {
    return [
      element.innerText,
      element.value,
      element.getAttribute?.("aria-label"),
      element.getAttribute?.("title"),
      element.getAttribute?.("alt"),
    ].filter(Boolean).join(" ").toLowerCase();
  }

  const CLICKABLE_ARIA_ROLES = new Set([
    "link", "button", "tab", "checkbox", "radio", "combobox", "option",
    "slider", "textbox", "menuitem", "menuitemcheckbox", "menuitemradio",
  ]);

  function isActionableElement(element) {
    const role = element.getAttribute?.("role");
    if (element.matches?.("a[href], button, input:not([type='hidden']), textarea, select")) {
      return true;
    }
    if (isTypingElement(element)) return true;
    if (CLICKABLE_ARIA_ROLES.has(role)) return true;
    if (element.hasAttribute?.("aria-controls")
        || element.hasAttribute?.("aria-pressed")
        || element.hasAttribute?.("aria-checked")
        || (element.hasAttribute?.("aria-haspopup") && role !== "menu")) {
      return true;
    }
    if (element.tabIndex >= 0 || element.hasAttribute?.("onclick")
        || element.hasAttribute?.("onmousedown") || element.hasAttribute?.("onmouseup")) {
      return true;
    }
    if (scrollableElements.has(element) && element !== scrollableElements.largest) return true;
    if (element.matches?.("label") && element.control && !elementRect(element.control)) return true;
    return false;
  }

  function actionableElements() {
    const selector = [
      "a[href]", "button", "input:not([type='hidden'])", "select", "textarea", "label",
      "[contenteditable]:not([contenteditable='false'])", "[role]", "[onclick]", "[onmousedown]",
      "[onmouseup]", "[aria-controls]", "[aria-pressed]", "[aria-checked]", "[aria-haspopup]",
      "[tabindex]:not([tabindex='-1'])",
    ].join(",");
    const seen = new Set();
    const items = [];
    // VimFx can inspect dynamically registered event listeners through
    // nsIEventListenerService. Chromium content scripts cannot.
    // VIMFX PARITY TODO(rhoPrivate.vim):
    // if (hasEventListeners(element, "click")) type = "clickable";
    const consider = (root, element) => {
      if (seen.has(element)) return;
      seen.add(element);
      if (!isActionableElement(element)) return;
      const rect = elementRect(element);
      if (!rect) return;
      const x = Math.max(0, Math.min(innerWidth - 1, rect.left + 1));
      const y = Math.max(0, Math.min(innerHeight - 1, rect.top + rect.height / 2));
      const covering = root.elementFromPoint?.(x, y)
        ?? element.ownerDocument.elementFromPoint(x, y);
      if (covering && covering !== element && !element.contains(covering)) return;
      items.push({
        element,
        rect,
        text: elementText(element),
        scrollable: isScrollable(element, "x") || isScrollable(element, "y"),
        weight: Math.max(1, rect.width * rect.height),
      });
    };
    for (const root of walkRoots()) {
      for (const element of root.querySelectorAll(selector)) {
        consider(root, element);
      }
      for (const element of root.querySelectorAll("*")) {
        if (scrollableElements.has(element)) consider(root, element);
      }
    }
    return items;
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

  // Ported from VimFx's MIT-licensed n-ary Huffman implementation. It gives
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

  function assignHints(items) {
    hintTree(items, HINT_CHARS.length).assign(HINT_CHARS, (item, hint) => {
      item.hint = hint;
      item.baseHint ??= hint;
    });
  }

  function clearHints() {
    hints?.host.remove();
    hints = undefined;
    hintGeneration = 0;
    if (mode === "hints") setMode("normal");
  }

  function clickCurrent(element) {
    element.focus?.({ preventScroll: true });
    if (typeof element.click !== "function") {
      element.dispatchEvent(
        new MouseEvent("click", { bubbles: true, cancelable: true, composed: true }),
      );
      return;
    }
    // VimFx dispatches through Firefox's privileged pres shell so the event is
    // trusted. Keep the privileged call at this exact boundary when Brave
    // grows the equivalent API.
    // VIMFX PARITY TODO(rhoPrivate.vim):
    // chrome.rhoPrivate.vim.trustedClick({ frameId, x, y, modifiers });
    // For now this executes synchronously inside the trusted key event's user
    // activation window, but the resulting click event itself is untrusted.
    const target = element.getAttribute?.("target");
    if (target && target.toLowerCase() !== "_self") element.removeAttribute("target");
    element.click();
    if (target !== null) element.setAttribute("target", target);
  }

  function activateHint(item, byText = false) {
    clearHints();
    if (byText) ignoreKeyEventsUntil = performance.now() + HINT_TEXT_TIMEOUT_MS;
    if (isTypingElement(item.element) || item.element.matches("select")) {
      item.element.focus();
      item.element.select?.();
    } else if (item.scrollable) {
      if (!item.element.hasAttribute("tabindex")) item.element.tabIndex = -1;
      item.element.focus({ preventScroll: true });
    } else {
      clickCurrent(item.element);
    }
  }

  function hintMatches() {
    if (!hints) return [];
    if (hints.enteredHint) {
      return hints.items.filter((item) => item.hint.startsWith(hints.enteredHint));
    }
    if (hints.enteredText) {
      const words = hints.enteredText.trim().split(/\s+/);
      return hints.items.filter((item) => words.every((word) => item.text.includes(word)));
    }
    return hints.items;
  }

  function refreshHintMarkers() {
    hintRefreshPending = false;
    if (!hints) return;
    const matchingItems = hintMatches();
    if (hints.enteredText) {
      assignHints(matchingItems);
    } else if (!hints.enteredHint) {
      for (const item of hints.items) item.hint = item.baseHint;
    }
    const matches = new Set(matchingItems);
    for (const item of hints.items) {
      const rect = elementRect(item.element);
      const visible = matches.has(item) && rect;
      item.marker.style.display = visible ? "block" : "none";
      if (!visible) continue;
      item.rect = rect;
      item.marker.style.left = `${Math.max(0, Math.round(rect.left))}px`;
      item.marker.style.top = `${Math.max(0, Math.round(rect.top))}px`;
      item.marker.textContent = item.hint;
      if (hints.enteredHint) {
        const matched = document.createElement("b");
        matched.textContent = hints.enteredHint;
        item.marker.textContent = "";
        item.marker.append(matched, item.hint.slice(hints.enteredHint.length));
      }
    }
  }

  function scheduleHintRefresh() {
    if (!hints || hintRefreshPending) return;
    hintRefreshPending = true;
    requestAnimationFrame(refreshHintMarkers);
  }

  function enterHints(generation) {
    const items = actionableElements();
    if (items.length === 0) return false;
    clearHints();
    hintGeneration = generation;
    assignHints(items);

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
      item.marker = marker;
      shadow.append(marker);
    }
    document.documentElement.appendChild(host);
    hints = { host, items, enteredHint: "", enteredText: "" };
    refreshHintMarkers();
    return true;
  }

  function handleHintKey(event) {
    if (!hints) return;
    const key = event.key;
    if (key === "Escape") {
      clearHints();
      return;
    }
    if (key === "Backspace") {
      if (hints.enteredHint) hints.enteredHint = hints.enteredHint.slice(0, -1);
      else hints.enteredText = hints.enteredText.slice(0, -1);
      refreshHintMarkers();
      return;
    }
    if (key === "Enter") {
      const [match] = hintMatches();
      if (match) activateHint(match, Boolean(hints.enteredText));
      return;
    }
    if ((key === " " && (event.ctrlKey || event.shiftKey))
        || (key === "Backspace" && event.ctrlKey)
        || key === "ArrowUp") {
      // VimFx parity TODO: rotate overlapping markers, toggle the
      // complementary candidate pass, and increase multi-follow count.
      return;
    }
    if (key.length !== 1 || event.ctrlKey || event.altKey || event.metaKey) return;

    const char = key.toLowerCase();
    if (hints.enteredHint || HINT_CHARS.includes(char)) {
      const proposed = hints.enteredHint + char;
      const matches = hints.items.filter((item) => item.hint.startsWith(proposed));
      if (matches.length === 0) return;
      hints.enteredHint = proposed;
      refreshHintMarkers();
      const exact = matches.find((item) => item.hint === proposed);
      if (exact) activateHint(exact);
      return;
    }

    if (key === " " && (!hints.enteredText || hints.enteredText.endsWith(" "))) return;
    const proposed = hints.enteredText + key.toLowerCase();
    const words = proposed.trim().split(/\s+/);
    const matches = hints.items.filter((item) => words.every((word) => item.text.includes(word)));
    if (matches.length === 0) return;
    hints.enteredText = proposed;
    refreshHintMarkers();
    if (matches.length === 1) activateHint(matches[0], true);
  }

  function resetInput() {
    currentKeyTree = COMMANDS;
    count = "";
    lastInputTime = 0;
  }

  function command(run, { repeatable = false } = {}) {
    return { run, repeatable };
  }

  const COMMANDS = {
    h: command((amount, event) => runScroll("left", amount, event.repeat), { repeatable: true }),
    j: command((amount, event) => runScroll("down", amount, event.repeat), { repeatable: true }),
    k: command((amount, event) => runScroll("up", amount, event.repeat), { repeatable: true }),
    l: command((amount, event) => runScroll("right", amount, event.repeat), { repeatable: true }),
    d: command((amount, event) => runScroll("half-down", amount, event.repeat), { repeatable: true }),
    u: command((amount, event) => runScroll("half-up", amount, event.repeat), { repeatable: true }),
    Space: command((amount, event) => runScroll("page-down", amount, event.repeat), { repeatable: true }),
    "S-Space": command((amount, event) => runScroll("page-up", amount, event.repeat), { repeatable: true }),
    0: command((amount, event) => runScroll("far-left", amount, event.repeat)),
    "^": command((amount, event) => runScroll("far-left", amount, event.repeat)),
    "$": command((amount, event) => runScroll("far-right", amount, event.repeat)),
    G: command((amount, event) => runScroll("bottom", amount, event.repeat)),
    H: command((amount) => runBrowserCommand("back", amount), { repeatable: true }),
    L: command((amount) => runBrowserCommand("forward", amount), { repeatable: true }),
    r: command((amount) => runBrowserCommand("reload", amount), { repeatable: true }),
    Escape: command(() => escapeNormalMode()),
    f: command(() => startHints()),
    // VimFx parity TODO: `F` opens a hinted link in a background tab. Rho must
    // create a managed PageId and Desk-owned page rather than an orphan Brave
    // tab before this binding can be enabled.
    // F: command(() => startHintsInManagedTab()),
    i: command(() => {
      ignoreRemaining = undefined;
      setMode("ignore");
    }),
    I: command((amount) => {
      ignoreRemaining = amount;
      setMode("ignore");
    }),
    g: {
      g: command((amount, event) => runScroll("top", amount, event.repeat)),
      i: command((amount, _event, explicitCount) => focusTextInput(explicitCount ? amount : undefined)),
    },
  };
  currentKeyTree = COMMANDS;

  function startHints() {
    nextHintGeneration = (nextHintGeneration + 1) || 1;
    if (enterHints(nextHintGeneration)) setMode("hints");
  }

  function runBrowserCommand(name, amount) {
    chrome.runtime.sendMessage({
      type: "rho-browser-command",
      command: name,
      count: amount,
    }).catch(() => {});
  }

  function stopEvent(event) {
    event.preventDefault();
    event.stopImmediatePropagation();
  }

  function consume(event, repeatCommand) {
    keyDispositions.set(event.code, { consumed: true, repeatCommand });
    stopEvent(event);
  }

  function keyName(event) {
    if (event.ctrlKey || event.altKey || event.metaKey) return undefined;
    if (event.key === " ") return event.shiftKey ? "S-Space" : "Space";
    if (event.key.length === 1) {
      return event.shiftKey ? event.key.toUpperCase() : event.key.toLowerCase();
    }
    return event.key;
  }

  function focusConflict(event, focused) {
    if (isTypingElement(focused)) return true;
    if (isActivatableElement(focused)) return event.key === "Enter";
    if (isAdjustableElement(focused)) {
      return ["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Enter", " "]
        .includes(event.key);
    }
    return false;
  }

  function onKeyDown(event) {
    if (!event.isTrusted) return;
    const disposition = keyDispositions.get(event.code);
    if (event.repeat && disposition) {
      if (disposition.consumed) {
        disposition.repeatCommand?.run(1, event, false);
        stopEvent(event);
      }
      return;
    }
    keyDispositions.set(event.code, { consumed: false });

    if (performance.now() <= ignoreKeyEventsUntil) {
      consume(event);
      return;
    }

    if (mode === "ignore") {
      if (ignoreRemaining !== undefined) {
        ignoreRemaining -= 1;
        if (ignoreRemaining <= 0) {
          ignoreRemaining = undefined;
          setMode("normal");
        }
      } else if (!event.ctrlKey && !event.altKey && !event.metaKey && event.shiftKey
                 && event.key === "Escape") {
        ignoreRemaining = undefined;
        setMode("normal");
        resetInput();
        consume(event);
      }
      // VimFx parity TODO: Shift-F1 temporarily returns to Normal mode for one
      // command, then re-enters explicit Ignore mode.
      // if (key === "<s-f1>") unquoteOneNormalCommand();
      return;
    }

    if (mode === "hints") {
      // Keep browser/compositor shortcuts available. All unmodified keys belong
      // to Hints mode, including printable text that is not in the hint alphabet.
      const hintModifierCommand = (
        event.key === "Enter" && (event.ctrlKey || event.altKey)
      ) || (
        event.key === " " && event.ctrlKey
      ) || (
        event.key === "Backspace" && event.ctrlKey
      );
      if ((event.ctrlKey || event.altKey || event.metaKey) && !hintModifierCommand) return;
      handleHintKey(event);
      consume(event);
      return;
    }

    if (!event.ctrlKey && !event.altKey && !event.metaKey && event.key === "Tab"
        && moveInputFocus(event.shiftKey ? -1 : 1)) {
      consume(event);
      return;
    }

    const key = keyName(event);
    const focused = activeElement();
    if (key !== "Escape" && focusConflict(event, focused)) {
      resetInput();
      return;
    }

    if (!key) {
      resetInput();
      return;
    }

    const now = performance.now();
    if (lastInputTime && now - lastInputTime >= INPUT_TIMEOUT_MS) resetInput();
    lastInputTime = now;
    const topLevel = currentKeyTree === COMMANDS;
    const explicitCount = count !== "";
    const match = currentKeyTree[key];

    if (match && !(topLevel && key === "0" && explicitCount)) {
      if (match.run) {
        const amount = explicitCount ? Number(count) : 1;
        resetInput();
        const shouldConsume = match.run(amount, event, explicitCount) !== false;
        if (shouldConsume) consume(event, match.repeatable ? match : undefined);
      } else {
        currentKeyTree = match;
        consume(event);
      }
      return;
    }

    if (topLevel && /^\d$/.test(key) && !(key === "0" && !explicitCount)) {
      count = String(Math.min(9999, Number(`${count}${key}`)));
      consume(event);
      return;
    }

    resetInput();
    if (!topLevel) consume(event);
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
  addEventListener("focus", (event) => {
    if (!event.isTrusted || !isTypingElement(event.target)) return;
    lastFocusedTextInput = event.target;
    hasFocusedTextInput = true;

    // VimFx uses nsIFocusManager.getLastFocusMethod() here to distinguish
    // autofocus from a user's click or keypress, then optionally blurs focus
    // thieves while suppressing the resulting focus events.
    // VIMFX PARITY TODO(rhoPrivate.vim):
    // if (preventAutofocus && lastFocusMethod() === PROGRAMMATIC) event.target.blur();
  }, true);
  addEventListener("mousedown", (event) => {
    if (!event.isTrusted) return;
    if (hints) clearHints();
    if (!focusInputs?.includes(event.target)) focusInputs = undefined;
  }, true);
  addEventListener("pagehide", clearHints, true);
  addEventListener("resize", scheduleHintRefresh, true);
  addEventListener("scroll", scheduleHintRefresh, true);
})();
