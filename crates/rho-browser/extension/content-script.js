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
  let ignoreReason;
  let returnToIgnore = false;
  let currentKeyTree;
  let count = "";
  let lastInputTime = 0;
  let lastScrollRepeat = 0;
  let lastFocusedTextInput;
  let hasFocusedTextInput = false;
  let focusInputs;
  let hintRefreshPending = false;
  let ignoreKeyEventsUntil = 0;
  let captureKey;
  let caretSelecting = false;
  const scrollMarks = new Map();
  const scrollJumpList = [];
  let scrollJumpIndex = -1;
  const keyDispositions = new Map();

  // VimFx owns one synchronous key state and marker namespace per tab. MV3
  // gives each out-of-process frame an independent isolated world, while
  // service-worker messaging is asynchronous and cannot decide whether the
  // current key event must be cancelled. Keep the per-document implementation
  // until the Brave fork exposes a tab-scoped pre-target key/marker router.
  // VIMFX PARITY TODO(rhoPrivate.vim):
  // const tabVim = chrome.rhoPrivate.vim.attachTabKeyRouter();

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

  function isIgnoreModeElement(element) {
    return element?.hasAttribute?.("data-wasavi-state")
      || element?.closest?.("#wasavi_container");
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
    const style = element.ownerDocument.defaultView.getComputedStyle(element);
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

  function elementShape(root, element) {
    const style = element.ownerDocument.defaultView.getComputedStyle(element);
    if (style.visibility === "hidden" || style.display === "none") return undefined;
    const rects = [...element.getClientRects()]
      .map((rect) => ({
        left: Math.max(0, rect.left),
        top: Math.max(0, rect.top),
        right: Math.min(innerWidth, rect.right),
        bottom: Math.min(innerHeight, rect.bottom),
      }))
      .filter((rect) => rect.right - rect.left >= 4 && rect.bottom - rect.top >= 4);
    if (rects.length === 0) return undefined;
    const area = rects.reduce(
      (total, rect) => total + (rect.right - rect.left) * (rect.bottom - rect.top),
      0,
    );
    for (const rect of rects) {
      let x = rect.left + 1;
      const y = Math.round((rect.top + rect.bottom) / 2);
      for (let attempt = 0; attempt < 2 && x < rect.right; attempt += 1) {
        const covering = root.elementFromPoint?.(x, y)
          ?? element.ownerDocument.elementFromPoint(x, y);
        if (!covering || covering === element || element.contains(covering)) {
          return {
            rect: new DOMRect(rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top),
            area,
          };
        }
        const coveringRect = covering.getBoundingClientRect();
        x = Math.max(x + 1, coveringRect.right + 1);
      }
    }
    return undefined;
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
      if (axis === "x") return innerWidth;
      let headerBottom = 0;
      let footerTop = innerHeight;
      const maxHeight = innerHeight / 3;
      const minWidth = Math.min(innerWidth / 2, 800);
      for (const candidate of document.querySelectorAll("div, ul, nav, header, footer, section")) {
        const rect = candidate.getBoundingClientRect();
        if (rect.height > maxHeight || rect.width < minWidth
            || getComputedStyle(candidate).position !== "fixed") continue;
        if (rect.top <= headerBottom && rect.bottom > headerBottom) headerBottom = rect.bottom;
        if (rect.bottom >= footerTop && rect.top < footerTop) footerTop = rect.top;
      }
      return Math.max(0, footerTop - headerBottom);
    }
    return axis === "x" ? element.clientWidth : element.clientHeight;
  }

  function scrollPosition(element = scrollTarget()) {
    return { element, left: element?.scrollLeft ?? 0, top: element?.scrollTop ?? 0 };
  }

  function restoreScrollPosition(position) {
    if (!position?.element?.isConnected) return;
    position.element.scrollTo({
      left: position.left,
      top: position.top,
      behavior: "smooth",
    });
  }

  function addScrollJump() {
    const position = scrollPosition();
    const previous = scrollJumpList.at(-1);
    if (previous?.element === position.element
        && previous.left === position.left && previous.top === position.top) return;
    if (scrollJumpIndex >= 0) scrollJumpList.splice(scrollJumpIndex + 1);
    scrollJumpList.push(position);
    if (scrollJumpList.length > 100) scrollJumpList.shift();
    scrollJumpIndex = scrollJumpList.length - 1;
  }

  function moveScrollJump(direction, amount) {
    if (scrollJumpList.length === 0) return;
    if (direction < 0 && scrollJumpIndex === scrollJumpList.length - 1) addScrollJump();
    scrollJumpIndex = Math.max(
      0,
      Math.min(scrollJumpList.length - 1, scrollJumpIndex + direction * amount),
    );
    restoreScrollPosition(scrollJumpList[scrollJumpIndex]);
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
      addScrollJump();
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

  function copyText(text) {
    navigator.clipboard?.writeText(String(text)).catch(() => {});
  }

  function startKeyCapture(callback) {
    captureKey = callback;
    setMode("capture");
  }

  function markScrollPosition(key) {
    scrollMarks.set(key, scrollPosition());
  }

  function jumpToScrollMark(key) {
    const position = scrollMarks.get(key);
    if (!position) return;
    scrollMarks.set("'", scrollPosition());
    addScrollJump();
    restoreScrollPosition(position);
  }

  function goUpPath(amount) {
    const url = new URL(location.href);
    const parts = url.pathname.split("/").filter(Boolean);
    parts.splice(Math.max(0, parts.length - amount));
    url.pathname = `/${parts.join("/")}${parts.length ? "/" : ""}`;
    url.search = "";
    url.hash = "";
    location.assign(url.href);
  }

  function goToRoot() {
    location.assign(new URL("/", location.href).href);
  }

  const PREVIOUS_PATTERNS = ["prev", "previous", "‹", "«", "◀", "←", "<<", "<", "back", "newer"];
  const NEXT_PATTERNS = ["next", "›", "»", "▶", "→", ">>", ">", "more", "older"];

  function followPattern(patterns) {
    const candidates = [...document.querySelectorAll("a, button, input[type='button']")];
    for (const element of candidates) {
      if (!elementRect(element)) continue;
      const text = [
        element.innerText,
        element.value,
        element.rel,
        element.getAttribute("role"),
        element.getAttribute("aria-label"),
        element.getAttribute("data-tooltip"),
      ].filter(Boolean).join(" ").trim().toLowerCase();
      if (patterns.some((pattern) => text === pattern
          || (pattern.length > 2 && text.includes(pattern)))) {
        clickCurrent(element);
        return;
      }
    }
  }

  function setCaretAt(element, select = false) {
    const selection = getSelection();
    if (!selection) return;
    const range = document.createRange();
    range.selectNodeContents(element);
    if (!select) range.collapse(true);
    selection.removeAllRanges();
    selection.addRange(range);
    caretSelecting = select;
    setMode("caret");
  }

  function moveCaret(direction, granularity) {
    getSelection()?.modify(caretSelecting ? "extend" : "move", direction, granularity);
  }

  function leaveCaret({ copy = false } = {}) {
    if (copy) copyText(getSelection()?.toString() ?? "");
    caretSelecting = false;
    setMode("normal");
  }

  function toggleHelp() {
    const old = document.getElementById("__rho_vim_help");
    if (old) {
      old.remove();
      return;
    }
    const help = document.createElement("pre");
    help.id = "__rho_vim_help";
    help.textContent = `Rho Vim / VimFx\n\nScroll  h j k l  d u  Space  gg G  0 ^ $\nHistory H L   Reload r/R   Stop s\nHints   f  yf copy  ef focus  ec context  v/av/yv text\nInputs  gi   Ignore i   Quote I\nMarks   m{key}  '{key}  g[ g]\nPath    gu gU   Copy URL yy\nCaret   h j k l b w 0 ^ $ v o y Esc\nHelp    ?`;
    Object.assign(help.style, {
      all: "initial",
      whiteSpace: "pre",
      position: "fixed",
      top: "50%",
      left: "50%",
      transform: "translate(-50%, -50%)",
      zIndex: "2147483647",
      padding: "18px",
      color: "#eee",
      background: "#202124f5",
      border: "1px solid #777",
      borderRadius: "5px",
      font: "13px/1.55 monospace",
      boxShadow: "0 6px 30px #000a",
      pointerEvents: "none",
    });
    document.documentElement.append(help);
  }

  const BLACKLIST_KEY = "vimfx.blacklist";

  async function toggleBlacklist() {
    const origin = location.origin;
    const stored = await chrome.storage.local.get(BLACKLIST_KEY);
    const blacklist = new Set(stored[BLACKLIST_KEY] ?? []);
    if (blacklist.delete(origin)) {
      ignoreReason = undefined;
      setMode("normal");
    } else {
      blacklist.add(origin);
      ignoreReason = "blacklist";
      ignoreRemaining = undefined;
      setMode("ignore");
    }
    await chrome.storage.local.set({ [BLACKLIST_KEY]: [...blacklist] });
  }

  if (globalThis.chrome?.storage?.local) {
    chrome.storage.local.get(BLACKLIST_KEY).then((stored) => {
      if ((stored[BLACKLIST_KEY] ?? []).includes(location.origin)) {
        ignoreReason = "blacklist";
        ignoreRemaining = undefined;
        setMode("ignore");
      }
    }).catch(() => {});
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
      const shape = elementShape(root, element);
      if (!shape) return;
      const { rect } = shape;
      items.push({
        element,
        rect,
        text: elementText(element),
        scrollable: isScrollable(element, "x") || isScrollable(element, "y"),
        weight: Math.max(1, shape.area),
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
    const links = new Map();
    return items.filter((item) => {
      const href = item.element.href;
      if (!href || item.element.hasAttribute?.("onclick")) return true;
      const existing = links.get(href);
      if (!existing) {
        links.set(href, item);
        return true;
      }
      existing.weight += item.weight;
      return false;
    });
  }

  function complementaryElements(excluded) {
    const excludedElements = new Set(excluded.map((item) => item.element));
    const items = [];
    for (const root of walkRoots()) {
      for (const element of root.querySelectorAll("*")) {
        if (excludedElements.has(element) || element === document.body
            || element === document.documentElement) continue;
        const shape = elementShape(root, element);
        if (!shape) continue;
        items.push({
          element,
          rect: shape.rect,
          text: elementText(element),
          scrollable: false,
          weight: Math.max(1, shape.area),
          complementary: true,
        });
      }
    }
    return items;
  }

  function selectableTextElements() {
    const items = [];
    for (const root of walkRoots()) {
      for (const element of root.querySelectorAll(
        "p, pre, code, blockquote, li, dt, dd, td, th, h1, h2, h3, h4, h5, h6, div, span",
      )) {
        const hasDirectText = [...element.childNodes]
          .some((node) => node.nodeType === Node.TEXT_NODE && node.data.trim().length > 1);
        if (!hasDirectText) continue;
        const shape = elementShape(root, element);
        if (!shape) continue;
        items.push({
          element,
          rect: shape.rect,
          text: elementText(element),
          scrollable: false,
          weight: Math.max(1, shape.area),
        });
      }
    }
    return items;
  }

  function hintElementsForAction(action) {
    if (["caret", "select", "copy-text"].includes(action)) return selectableTextElements();
    const items = actionableElements();
    if (action === "copy") {
      return items.filter((item) => item.element.href || isTypingElement(item.element));
    }
    if (action === "focus") {
      return items.filter((item) => item.scrollable || item.element.tabIndex >= 0);
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
    const { action, count: hintCount } = hints;
    clearHints();
    if (byText) ignoreKeyEventsUntil = performance.now() + HINT_TEXT_TIMEOUT_MS;
    if (action === "copy") {
      copyText(item.element.href || item.element.value || item.element.innerText || "");
    } else if (action === "focus") {
      item.element.focus({ preventScroll: true });
      item.element.select?.();
    } else if (action === "context") {
      // VIMFX PARITY TODO(rhoPrivate.vim): dispatch a trusted context-menu
      // gesture through the renderer input pipeline.
      item.element.dispatchEvent(new MouseEvent("contextmenu", {
        bubbles: true,
        cancelable: true,
        composed: true,
        clientX: item.rect.left + 1,
        clientY: item.rect.top + item.rect.height / 2,
      }));
    } else if (action === "caret" || action === "select" || action === "copy-text") {
      setCaretAt(item.element, action !== "caret");
      if (action === "copy-text") leaveCaret({ copy: true });
    } else if (isTypingElement(item.element) || item.element.matches("select")) {
      item.element.focus();
      item.element.select?.();
    } else if (item.scrollable) {
      if (!item.element.hasAttribute("tabindex")) item.element.tabIndex = -1;
      item.element.focus({ preventScroll: true });
    } else {
      clickCurrent(item.element);
    }
    if (action === "multiple" && hintCount > 1) {
      setTimeout(() => startHints("multiple", hintCount - 1), 200);
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

  function addHintMarkers(items) {
    assignHints(items);
    items.forEach((item, index) => {
      const marker = document.createElement("span");
      marker.textContent = item.hint;
      marker.style.zIndex = String(index + 1);
      item.marker = marker;
      hints.shadow.append(marker);
    });
  }

  function toggleComplementaryHints() {
    if (!hints.complementaryItems) {
      hints.complementaryItems = complementaryElements(hints.primaryItems);
      addHintMarkers(hints.complementaryItems);
    }
    for (const item of hints.items) item.marker.style.display = "none";
    hints.complementary = !hints.complementary;
    hints.items = hints.complementary ? hints.complementaryItems : hints.primaryItems;
    hints.enteredHint = "";
    hints.enteredText = "";
    refreshHintMarkers();
  }

  function rotateOverlappingHints(forward) {
    const visible = hints.items.filter((item) => item.marker.style.display !== "none");
    const visited = new Set();
    for (const first of visible) {
      if (visited.has(first)) continue;
      const stack = visible.filter((item) => {
        const overlaps = item.rect.left < first.rect.right && item.rect.right > first.rect.left
          && item.rect.top < first.rect.bottom && item.rect.bottom > first.rect.top;
        if (overlaps) visited.add(item);
        return overlaps;
      });
      if (stack.length < 2) continue;
      const levels = stack.map((item) => item.marker.style.zIndex || "0");
      if (forward) levels.push(levels.shift());
      else levels.unshift(levels.pop());
      stack.forEach((item, index) => { item.marker.style.zIndex = levels[index]; });
    }
  }

  function enterHints(generation, action = "current", hintCount = 1) {
    const items = hintElementsForAction(action);
    if (items.length === 0) return false;
    clearHints();
    hintGeneration = generation;

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
    document.documentElement.appendChild(host);
    hints = {
      host,
      shadow,
      items,
      primaryItems: items,
      complementaryItems: undefined,
      complementary: false,
      enteredHint: "",
      enteredText: "",
      action,
      count: hintCount,
    };
    addHintMarkers(items);
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
    if (key === "Backspace" && event.ctrlKey) {
      toggleComplementaryHints();
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
    if (key === " " && (event.ctrlKey || event.shiftKey)) {
      rotateOverlappingHints(event.ctrlKey);
      return;
    }
    if (key === "ArrowUp") {
      hints.count += 1;
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

  function finishUnquote() {
    if (!returnToIgnore) return;
    returnToIgnore = false;
    ignoreReason = "explicit";
    ignoreRemaining = undefined;
    setMode("ignore");
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
    R: command((amount) => runBrowserCommand("reload-force", amount), { repeatable: true }),
    s: command(() => window.stop()),
    Escape: command(() => escapeNormalMode()),
    f: command((amount) => startHints("current", amount)),
    // VimFx parity TODO: `F` opens a hinted link in a background tab. Rho must
    // create a managed PageId and Desk-owned page rather than an orphan Brave
    // tab before this binding can be enabled.
    // F: command(() => startHintsInManagedTab()),
    i: command(() => {
      ignoreRemaining = undefined;
      ignoreReason = "explicit";
      setMode("ignore");
    }),
    I: command((amount) => {
      ignoreRemaining = amount;
      ignoreReason = "quote";
      setMode("ignore");
    }),
    m: command(() => startKeyCapture(markScrollPosition)),
    "'": command(() => startKeyCapture(jumpToScrollMark)),
    "[": command(() => followPattern(PREVIOUS_PATTERNS)),
    "]": command(() => followPattern(NEXT_PATTERNS)),
    // VimFx parity TODO(rhoPrivate.vim.find):
    // "/": command(() => nativeFind({})),
    // n: command(() => nativeFindAgain(false)),
    // N: command(() => nativeFindAgain(true)),
    v: command(() => startHints("caret")),
    "?": command(() => toggleHelp()),
    y: {
      y: command(() => copyText(location.href)),
      f: command(() => startHints("copy")),
      v: command(() => startHints("copy-text")),
      // t: duplicate tab requires a Desk-owned PageId.
    },
    e: {
      f: command(() => startHints("focus")),
      c: command(() => startHints("context")),
      // t/w/p: tab/window/private-window actions require Desk page creation.
    },
    a: {
      v: command(() => startHints("select")),
      // f: multi-follow requires managed background-page creation.
      // "/": command(() => nativeFind({ highlightAll: true })),
      r: command(() => runBrowserCommand("reload-all", 1)),
      R: command(() => runBrowserCommand("reload-all-force", 1)),
      s: command(() => runBrowserCommand("stop-all", 1)),
    },
    g: {
      g: command((amount, event) => runScroll("top", amount, event.repeat)),
      i: command((amount, _event, explicitCount) => focusTextInput(explicitCount ? amount : undefined)),
      u: command((amount) => goUpPath(amount)),
      U: command(() => goToRoot()),
      "[": command((amount) => moveScrollJump(-1, amount)),
      "]": command((amount) => moveScrollJump(1, amount)),
      B: command(() => toggleBlacklist()),
      // "/": command(() => nativeFind({ linksOnly: true })),
      // H: browser history popup is native Brave UI and intentionally omitted.
      // r: reader mode needs a fixed rhoPrivate browser command.
      // C: reload-config becomes active when key configuration is externalized.
      // T/t/l/L/J/K/w/0/^/$/p/X/x: tab commands require Desk handoff support.
    },
    // o/O/p/P/gh: omnibox, search, paste-and-go, and home need rhoPrivate.vim.
    // t/T/J/K/x/X/w/W: native tab/window commands must not bypass PageId ownership.
  };
  currentKeyTree = COMMANDS;

  function startHints(action = "current", hintCount = 1) {
    nextHintGeneration = (nextHintGeneration + 1) || 1;
    if (enterHints(nextHintGeneration, action, hintCount)) setMode("hints");
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

  function handleCaretKey(event) {
    const key = keyName(event);
    const moves = {
      h: ["backward", "character"],
      l: ["forward", "character"],
      j: ["forward", "line"],
      k: ["backward", "line"],
      b: ["backward", "word"],
      w: ["forward", "word"],
      0: ["backward", "lineboundary"],
      "^": ["backward", "lineboundary"],
      "$": ["forward", "lineboundary"],
    };
    if (moves[key]) {
      const amount = Math.max(1, count ? Number(count) : 1);
      for (let index = 0; index < amount; index += 1) moveCaret(...moves[key]);
      count = "";
      return true;
    }
    if (/^\d$/.test(key) && !(key === "0" && !count)) {
      count = String(Math.min(9999, Number(`${count}${key}`)));
      return true;
    }
    count = "";
    if (key === "v") {
      caretSelecting = !caretSelecting;
      return true;
    }
    if (key === "o") {
      const selection = getSelection();
      if (selection && !selection.isCollapsed) {
        const range = selection.getRangeAt(0);
        const end = range.cloneRange();
        end.collapse(caretSelecting);
        selection.removeAllRanges();
        selection.addRange(end);
      }
      caretSelecting = true;
      return true;
    }
    if (key === "y") {
      leaveCaret({ copy: true });
      return true;
    }
    if (key === "Escape") {
      leaveCaret();
      return true;
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

    if (mode === "capture") {
      const key = keyName(event);
      if (!key) return;
      if (key !== "Escape" && key.length === 1) captureKey?.(key);
      captureKey = undefined;
      setMode("normal");
      consume(event);
      return;
    }

    if (mode === "caret") {
      if (handleCaretKey(event)) consume(event);
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
        ignoreReason = undefined;
        setMode("normal");
        resetInput();
        consume(event);
      } else if (!event.ctrlKey && !event.altKey && !event.metaKey
                 && event.shiftKey && event.key === "F1") {
        returnToIgnore = true;
        setMode("normal");
        resetInput();
        consume(event);
      }
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
      finishUnquote();
      return;
    }

    const key = keyName(event);
    const focused = activeElement();
    if (key !== "Escape" && focusConflict(event, focused)) {
      resetInput();
      finishUnquote();
      return;
    }

    if (!key) {
      resetInput();
      finishUnquote();
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
        finishUnquote();
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
    finishUnquote();
    if (!topLevel) consume(event);
  }

  addEventListener("keydown", onKeyDown, true);
  globalThis.chrome?.runtime?.onMessage?.addListener((message) => {
    if (message?.type === "rho-vim-stop") window.stop();
  });
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
    if (!event.isTrusted) return;
    if (isIgnoreModeElement(event.target)) {
      ignoreReason = "focus";
      ignoreRemaining = undefined;
      setMode("ignore");
      return;
    }
    if (!isTypingElement(event.target)) return;
    lastFocusedTextInput = event.target;
    hasFocusedTextInput = true;

    // VimFx uses nsIFocusManager.getLastFocusMethod() here to distinguish
    // autofocus from a user's click or keypress, then optionally blurs focus
    // thieves while suppressing the resulting focus events.
    // VIMFX PARITY TODO(rhoPrivate.vim):
    // if (preventAutofocus && lastFocusMethod() === PROGRAMMATIC) event.target.blur();
  }, true);
  addEventListener("blur", (event) => {
    if (!event.isTrusted || ignoreReason !== "focus") return;
    setTimeout(() => {
      if (!isIgnoreModeElement(activeElement())) {
        ignoreReason = undefined;
        setMode("normal");
      }
    }, 50);
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
