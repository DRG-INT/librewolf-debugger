// lwdbg live bridge — clean-room.
//
// Why this exists: Marionette and WebDriver BiDi are startup-only flags, and
// on LibreWolf the DevTools RDP listener cannot be enabled at all (its gate
// pref is forced by an AutoConfig file that runs after user.js). A browser
// that has been open for three days is therefore unreachable by every
// transport. An extension runs *inside* that process, so it needs no restart
// and loses no session.
//
// Design rules this file follows:
//
//  * It opens no network connection. The only channel is the native messaging
//    port, which the browser itself spawns. An extension that could reach the
//    network would be a far larger grant than the job needs.
//  * Every command is explicitly dispatched. There is no "eval arbitrary
//    message as code" path, and unknown commands are refused by name.
//  * Commands that change state are marked, and the host decides whether they
//    are allowed. Policy lives in one place, not scattered here.

const PORT_NAME = "net.lizicska.lwdbg";

// name -> { mutates, run }
const COMMANDS = {
  ping: {
    mutates: false,
    run: async () => ({ pong: true, at: Date.now() }),
  },

  // What is open. This is the command that answers "reach the session that
  // has been running for days" — the one thing no transport can do here.
  tabs: {
    mutates: false,
    run: async () => {
      const tabs = await browser.tabs.query({});
      return {
        tabs: tabs.map((t) => ({
          id: t.id,
          windowId: t.windowId,
          index: t.index,
          url: t.url,
          title: t.title,
          active: t.active,
          pinned: t.pinned,
          audible: t.audible || false,
          discarded: t.discarded || false,
          status: t.status,
        })),
      };
    },
  },

  windows: {
    mutates: false,
    run: async () => {
      const ws = await browser.windows.getAll({ populate: false });
      return {
        windows: ws.map((w) => ({
          id: w.id,
          focused: w.focused,
          incognito: w.incognito,
          state: w.state,
          type: w.type,
        })),
      };
    },
  },

  // Page timing out of a tab the user already has open, with the same
  // anchoring correction the transports use. Read-only.
  timing: {
    mutates: false,
    run: async (p) => {
      const tabId = await resolveTab(p);
      const [res] = await browser.tabs.executeScript(tabId, {
        code: TIMING_CODE,
        runAt: "document_idle",
      });
      return { tabId, timing: res };
    },
  },

  // Read a value out of a page. `code` must evaluate to something
  // structured-cloneable. This is a read in intent, and the host still
  // classifies it as mutating, because a page cannot be asked to promise it
  // will not act.
  eval: {
    mutates: true,
    run: async (p) => {
      if (typeof p.code !== "string" || !p.code.length) {
        throw new Error("eval needs a non-empty `code` string");
      }
      const tabId = await resolveTab(p);
      const [res] = await browser.tabs.executeScript(tabId, {
        code: p.code,
        runAt: "document_idle",
      });
      return { tabId, value: res };
    },
  },

  screenshot: {
    mutates: false,
    run: async (p) => {
      const tab = p.tabId
        ? await browser.tabs.get(p.tabId)
        : (await browser.tabs.query({ active: true, currentWindow: true }))[0];
      const dataUrl = await browser.tabs.captureVisibleTab(tab.windowId, {
        format: "png",
      });
      const comma = dataUrl.indexOf(",");
      return { tabId: tab.id, png_base64: dataUrl.slice(comma + 1) };
    },
  },

  navigate: {
    mutates: true,
    run: async (p) => {
      if (typeof p.url !== "string") throw new Error("navigate needs `url`");
      const tabId = await resolveTab(p);
      const t = await browser.tabs.update(tabId, { url: p.url });
      return { tabId: t.id, url: t.url };
    },
  },

  newTab: {
    mutates: true,
    run: async (p) => {
      const t = await browser.tabs.create({
        url: p.url || "about:blank",
        active: p.active !== false,
      });
      return { tabId: t.id, url: t.url };
    },
  },

  closeTab: {
    mutates: true,
    run: async (p) => {
      if (typeof p.tabId !== "number") throw new Error("closeTab needs `tabId`");
      await browser.tabs.remove(p.tabId);
      return { closed: p.tabId };
    },
  },

  reload: {
    mutates: true,
    run: async (p) => {
      const tabId = await resolveTab(p);
      await browser.tabs.reload(tabId, { bypassCache: !!p.bypassCache });
      return { tabId };
    },
  },
};

// Keep the timing definition identical to the transports', so a number from
// the extension and a number from Marionette mean the same thing.
const TIMING_CODE = `(() => {
  const n = performance.getEntriesByType('navigation')[0];
  if (!n) return null;
  const marks = [n.startTime, n.fetchStart, n.redirectStart]
    .filter(v => typeof v === 'number' && v !== 0);
  const zero = marks.length ? Math.min(0, ...marks) : 0;
  const us = v => (typeof v !== 'number' || v === 0) ? null : Math.round((v - zero) * 1000);
  return {
    us_ttfb: us(n.responseStart),
    us_response_end: us(n.responseEnd),
    us_dom_interactive: us(n.domInteractive),
    us_dcl: us(n.domContentLoadedEventEnd),
    us_load: us(n.loadEventEnd),
    transfer_size: n.transferSize ?? null,
    encoded_body_size: n.encodedBodySize ?? null,
    decoded_body_size: n.decodedBodySize ?? null,
    dom_nodes: document.getElementsByTagName('*').length,
    diag_anchor_ms: zero,
    diag_resource_entries: performance.getEntriesByType('resource').length,
    diag_url: location.href,
  };
})()`;

async function resolveTab(p) {
  if (typeof p.tabId === "number") return p.tabId;
  const [active] = await browser.tabs.query({
    active: true,
    currentWindow: true,
  });
  if (!active) throw new Error("no active tab");
  return active.id;
}

let port = null;

function connect() {
  try {
    port = browser.runtime.connectNative(PORT_NAME);
  } catch (e) {
    console.error("lwdbg: connectNative failed:", e);
    return;
  }

  port.onMessage.addListener(async (msg) => {
    const id = msg && msg.id;
    try {
      if (!msg || typeof msg.command !== "string") {
        throw new Error("message has no `command`");
      }
      const entry = COMMANDS[msg.command];
      if (!entry) {
        throw new Error(
          `unknown command ${JSON.stringify(msg.command)}; known: ` +
            Object.keys(COMMANDS).join(", ")
        );
      }
      const result = await entry.run(msg.params || {});
      port.postMessage({ id, ok: true, mutates: entry.mutates, result });
    } catch (e) {
      port.postMessage({ id, ok: false, error: String(e && e.message ? e.message : e) });
    }
  });

  port.onDisconnect.addListener(() => {
    const err = browser.runtime.lastError;
    console.warn("lwdbg: native port closed", err ? err.message : "");
    port = null;
    // The host is started by the browser and exits when the port closes, so
    // a disconnect is normal rather than exceptional. Reconnect with a delay
    // so a host that cannot start does not become a hot loop.
    setTimeout(connect, 2000);
  });

  // Announce the catalogue, so the host can report what this build supports
  // without hard-coding a list that could drift.
  port.postMessage({
    id: 0,
    ok: true,
    result: {
      hello: "lwdbg-live",
      commands: Object.fromEntries(
        Object.entries(COMMANDS).map(([k, v]) => [k, { mutates: v.mutates }])
      ),
    },
  });
}

connect();
