const HOST = "dev.rho.browser";
const pageTabs = new Map();
const tabPages = new Map();
let port;
let reconnectTimer;
let operations = Promise.resolve();
let canonicalWindowId;
const parkingTabs = new Set();

function enqueue(operation) {
  const result = operations.then(operation, operation);
  operations = result.catch(() => {});
  return result;
}

function pageKey(id) {
  return `page:${id}`;
}

async function getPageId(tabId) {
  return chrome.rhoPrivate.tabs.getId(tabId);
}

async function setPageId(tabId, id) {
  await chrome.rhoPrivate.tabs.setId(tabId, id);
}

async function removePageId(tabId) {
  await chrome.rhoPrivate.tabs.removeId(tabId);
}

function reportTabState(state, tab, pageId = tabPages.get(tab?.id), reason = "") {
  if (!port || !pageId) return;
  try {
    port.postMessage({
      event: "tab-state",
      state,
      reason,
      page_id: pageId,
      tab_id: tab?.id,
      active: tab?.active,
      audible: tab?.audible,
      auto_discardable: tab?.autoDiscardable,
      discarded: tab?.discarded,
      frozen: tab?.frozen,
      status: tab?.status,
    });
  } catch (_) {}
}

async function rememberPage(id, tab, createdAt = Date.now(), launchUrl = tab.url || "") {
  // Re-emit the session command after restore so the browser-owned UUID is
  // part of both the restored tab and the newly recording session.
  await setPageId(tab.id, id);
  // Rho used to opt managed pages out of Brave's Memory Saver. Restore the
  // browser default for existing profiles and let Brave apply its native
  // eligibility checks before discarding a page.
  if (!tab.autoDiscardable) {
    tab = await chrome.tabs.update(tab.id, { autoDiscardable: true });
  }
  pageTabs.set(id, tab.id);
  tabPages.set(tab.id, id);
  reportTabState("registered", tab, id);
  const key = pageKey(id);
  const old = (await chrome.storage.local.get(key))[key];
  await chrome.storage.local.set({
    [key]: {
      id,
      launch_url: old?.launch_url || launchUrl,
      created_at_ms: old?.created_at_ms || createdAt,
      closing: old?.closing || false,
    },
  });
}

async function makePage(tab, id = crypto.randomUUID()) {
  await rememberPage(id, await chrome.tabs.get(tab.id));
  return id;
}

async function reconcile() {
  pageTabs.clear();
  tabPages.clear();
  let tabs = await chrome.tabs.query({ windowType: "normal" });
  const windows = await chrome.windows.getAll({ windowTypes: ["normal"] });
  if (!windows.some((window) => window.id === canonicalWindowId)) {
    canonicalWindowId = windows.find((window) => window.focused)?.id ?? windows[0]?.id;
  }
  if (canonicalWindowId !== undefined) {
    for (const tab of tabs) {
      if (tab.windowId !== canonicalWindowId) {
        await chrome.tabs.move(tab.id, { windowId: canonicalWindowId, index: -1 });
      }
    }
  }

  tabs = await chrome.tabs.query({ windowType: "normal" });
  for (const tab of tabs) {
    const id = await getPageId(tab.id);
    if (!id) continue;
    if (pageTabs.has(id)) {
      await makePage(tab);
    } else {
      await rememberPage(id, tab);
    }
  }

  // Extension storage is authoritative if Chrome could not restore a tab.
  const stored = await chrome.storage.local.get(null);
  for (const [key, record] of Object.entries(stored)) {
    if (!key.startsWith("page:")) continue;
    if (record.closing) {
      const tabId = pageTabs.get(record.id);
      if (tabId !== undefined) {
        const tab = await chrome.tabs.get(tabId).catch(() => undefined);
        if (tab) {
          await ensureParkingTab(tab);
          pageTabs.delete(record.id);
          tabPages.delete(tab.id);
          await chrome.tabs.remove(tab.id);
        }
      }
      await chrome.storage.local.remove(key);
      continue;
    }
    if (pageTabs.has(record.id)) continue;
    try {
      const parsed = new URL(record.launch_url);
      if (parsed.protocol !== "http:" && parsed.protocol !== "https:") continue;
      let tab = tabs.find(
        (candidate) => !tabPages.has(candidate.id) && candidate.url === record.launch_url,
      );
      tab ??= await chrome.tabs.create({
          windowId: canonicalWindowId,
          url: record.launch_url,
          active: false,
        });
      await makePage(tab, record.id);
    } catch (_) {}
  }
  tabs = await chrome.tabs.query({ windowType: "normal" });
}

async function findPage(id) {
  let tabId = pageTabs.get(id);
  if (tabId !== undefined) {
    try {
      return await chrome.tabs.get(tabId);
    } catch (_) {
      pageTabs.delete(id);
      tabPages.delete(tabId);
    }
  }
  await reconcile();
  tabId = pageTabs.get(id);
  if (tabId === undefined) throw new Error(`browser page not found: web-${id}`);
  return chrome.tabs.get(tabId);
}

async function createPage(url) {
  const parsed = new URL(url);
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error("browser URL must use http or https");
  }
  let tab = (await chrome.tabs.query({ active: true, lastFocusedWindow: true }))[0];
  if (tab && (tab.url === "chrome://newtab/" || tab.url === "about:blank")) {
    let id = tabPages.get(tab.id);
    if (!id) id = await makePage(tab);
    tab = await chrome.tabs.update(tab.id, { url, active: true });
    const record = { id, launch_url: url, created_at_ms: Date.now() };
    await chrome.storage.local.set({ [pageKey(id)]: record });
    return record;
  }
  if (canonicalWindowId === undefined) await reconcile();
  const windowId = canonicalWindowId;
  tab = await chrome.tabs.create({ windowId, url, active: true });
  const id = await makePage(tab);
  const record = { id, launch_url: url, created_at_ms: Date.now() };
  await chrome.storage.local.set({ [pageKey(id)]: record });
  await activatePage(id);
  return record;
}

async function activatePage(id) {
  let tab = await findPage(id);
  if (canonicalWindowId === undefined || tab.windowId !== canonicalWindowId) {
    await reconcile();
    tab = await findPage(id);
  }
  reportTabState("focus-requested", tab, id);
  tab = await chrome.tabs.update(tab.id, { active: true });
  reportTabState("focus-completed", tab, id);
  await chrome.windows.update(tab.windowId, { focused: true });
  return { id };
}

async function closePage(id) {
  const tab = await findPage(id);
  await ensureParkingTab(tab);
  const key = pageKey(id);
  const record = (await chrome.storage.local.get(key))[key];
  await chrome.storage.local.set({ [key]: { ...record, closing: true } });
  await removePageId(tab.id);
  pageTabs.delete(id);
  tabPages.delete(tab.id);
  await chrome.tabs.remove(tab.id);
  await chrome.storage.local.remove(key);
  return { id };
}

async function ensureParkingTab(tab) {
  const windowTabs = await chrome.tabs.query({ windowId: tab.windowId });
  if (windowTabs.length !== 1) return;
  const parking = await chrome.tabs.create({
    windowId: tab.windowId,
    url: chrome.runtime.getURL("parking.html"),
    active: false,
  });
  parkingTabs.add(parking.id);
}

async function listPages(limit = 100) {
  await reconcile();
  const tabs = await chrome.tabs.query({ windowType: "normal" });
  const records = [];
  for (const tab of tabs) {
    const id = tabPages.get(tab.id);
    if (!id) continue;
    const stored = (await chrome.storage.local.get(pageKey(id)))[pageKey(id)];
    records.push({
      id,
      launch_url: stored?.launch_url || tab.url || "",
      created_at_ms: stored?.created_at_ms || 0,
      last_accessed_ms: tab.lastAccessed || 0,
    });
  }
  records.sort((a, b) => b.last_accessed_ms - a.last_accessed_ms);
  return records.slice(0, Math.max(0, Math.min(limit, 1000)));
}

async function dispatch(message) {
  switch (message.method) {
    case "create": return createPage(message.params.url);
    case "focus": return activatePage(message.params.id);
    case "close": return closePage(message.params.id);
    case "list": return listPages(message.params?.limit);
    default: throw new Error(`unknown browser method: ${message.method}`);
  }
}

function connect() {
  clearTimeout(reconnectTimer);
  port = chrome.runtime.connectNative(HOST);
  port.onMessage.addListener((message) => {
    enqueue(() => dispatch(message)).then(
      (result) => port?.postMessage({ id: message.id, ok: true, result }),
      (error) => port?.postMessage({ id: message.id, ok: false, error: String(error?.message || error) }),
    );
  });
  port.onDisconnect.addListener(() => {
    port = undefined;
    reconnectTimer = setTimeout(connect, 1000);
  });
}

chrome.tabs.onRemoved.addListener((tabId, removeInfo) => {
  if (removeInfo.isWindowClosing || parkingTabs.delete(tabId)) return;
  const id = tabPages.get(tabId);
  if (!id) return;
  reportTabState("removed", { id: tabId }, id, "tab-removed");
  tabPages.delete(tabId);
  pageTabs.delete(id);
  chrome.storage.local.remove(pageKey(id));
});

chrome.tabs.onReplaced.addListener((addedTabId, removedTabId) => {
  const id = tabPages.get(removedTabId);
  if (!id) return;
  tabPages.delete(removedTabId);
  tabPages.set(addedTabId, id);
  pageTabs.set(id, addedTabId);
  setPageId(addedTabId, id).catch(() => {});
  chrome.tabs.get(addedTabId).then(
    (tab) => reportTabState("replaced", tab, id, "tab-replaced"),
    () => {},
  );
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  const id = tabPages.get(tabId);
  if (!id) return;
  const lifecycle = ["discarded", "frozen", "status"]
    .filter((field) => Object.hasOwn(changeInfo, field));
  if (lifecycle.length > 0) {
    reportTabState("updated", tab, id, lifecycle.join(","));
  }
});

chrome.runtime.onMessage.addListener((message, sender) => {
  if (
    sender.id !== chrome.runtime.id ||
    !sender.tab?.active ||
    (sender.documentLifecycle && sender.documentLifecycle !== "active") ||
    message?.type !== "rho-browser-command"
  ) return;
  switch (message.command) {
    case "back": return chrome.tabs.goBack(sender.tab.id);
    case "forward": return chrome.tabs.goForward(sender.tab.id);
    case "reload": return chrome.tabs.reload(sender.tab.id);
    default: return undefined;
  }
});

chrome.windows.onCreated.addListener(() => {
  enqueue(reconcile).catch(() => {});
});

enqueue(reconcile).catch(() => {});
connect();
