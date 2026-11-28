// The script API (lightair-docs/lightair/docs/script-api.md). Everything goes through one native
// function, __host(op, json) → json, which checks the package's permissions; this file only gives
// it a friendly shape. Runs before the package's own code, in its isolate.
(function () {
  "use strict";
  const host = globalThis.__host;
  delete globalThis.__host;
  const op = (name, arg) => {
    const out = JSON.parse(host(name, JSON.stringify(arg === undefined ? null : arg)));
    if (out && Object.prototype.hasOwnProperty.call(out, "err")) throw new Error(out.err);
    return out ? out.ok : undefined;
  };

  const handlers = { command: {}, step: {}, event: {}, ui: {}, overlay: {}, timer: {}, settings: [] };
  let timerId = 0;

  const lightair = {
    // Lightair's methods (the remote API's names), each checked against the permissions
    call: (method, params) => op("call", { method: String(method), params: params || {} }),
    on: (event, fn) => {
      op("subscribe", { event: String(event) });
      (handlers.event[event] = handlers.event[event] || []).push(fn);
    },
    command: (spec) => {
      handlers.command[spec.id] = spec.run;
      op("declare", { kind: "command", decl: { id: spec.id, title: spec.title || spec.id } });
    },
    hotkey: (spec) => {
      handlers.command[spec.id] = spec.run;
      op("declare", { kind: "hotkey", decl: { id: spec.id, title: spec.title || spec.id } });
    },
    step: (spec) => {
      handlers.step[spec.id] = spec.run;
      op("declare", { kind: "step", decl: { id: spec.id, title: spec.title || spec.id, fields: spec.fields || [] } });
    },
    settings: {
      declare: (list) => op("declare", { kind: "settings", decl: list || [] }),
      get: () => op("settings.get"),
      onChange: (fn) => handlers.settings.push(fn),
    },
    panel: (spec) => {
      if (spec.on) handlers.ui[spec.id] = spec.on;
      op("declare", { kind: "panel", decl: { id: spec.id, title: spec.title || spec.id, ui: spec.ui || [] } });
      return { update: (ui) => op("panel.update", { id: spec.id, ui: ui || [] }) };
    },
    overlay: (spec) => {
      if (spec.onMessage) handlers.overlay[spec.id] = spec.onMessage;
      op("declare", { kind: "overlay", decl: { id: spec.id, title: spec.title || spec.id, page: spec.page, width: spec.width || 1920, height: spec.height || 1080 } });
      return { send: (message) => op("overlay.send", { id: spec.id, message }) };
    },
    fetch: (url, init) => {
      const r = op("fetch", { url: String(url), method: (init && init.method) || "GET", headers: (init && init.headers) || {}, body: init && init.body != null ? String(init.body) : null });
      return { status: r.status, headers: r.headers, text: () => r.body, json: () => JSON.parse(r.body), ok: r.status >= 200 && r.status < 300 };
    },
    storage: {
      get: (key) => op("storage.get", { key: String(key) }),
      set: (key, value) => op("storage.set", { key: String(key), value: value === undefined ? null : value }),
    },
    files: {
      pick: () => op("files.pick"),
      read: (path) => op("files.read", { path: String(path) }),
    },
    notify: (text) => op("notify", { text: String(text) }),
  };
  Object.freeze(lightair.settings);
  Object.freeze(lightair.storage);
  Object.freeze(lightair.files);
  globalThis.lightair = Object.freeze(lightair);

  const log = (level) => (...args) => op("log", { level, text: args.map((a) => (typeof a === "string" ? a : JSON.stringify(a))).join(" ") });
  globalThis.console = Object.freeze({ log: log("info"), info: log("info"), warn: log("warn"), error: log("error"), debug: log("debug") });

  globalThis.setTimeout = (fn, ms) => {
    const id = ++timerId;
    handlers.timer[id] = { fn, repeat: false };
    op("timer", { id, ms: Math.max(0, Number(ms) || 0), repeat: false });
    return id;
  };
  globalThis.setInterval = (fn, ms) => {
    const id = ++timerId;
    handlers.timer[id] = { fn, repeat: true };
    op("timer", { id, ms: Math.max(10, Number(ms) || 0), repeat: true });
    return id;
  };
  globalThis.clearTimeout = globalThis.clearInterval = (id) => {
    delete handlers.timer[id];
    op("timer.clear", { id });
  };

  // Lightair → script: one message at a time. A handler's result (or its promise's) goes back as
  // the reply to `id`.
  const reply = (id, result) => {
    if (id == null) return;
    if (result && typeof result.then === "function") {
      result.then(
        (v) => op("reply", { id, ok: v === undefined ? null : v }),
        (e) => op("reply", { id, err: String((e && e.message) || e) }),
      );
    } else {
      op("reply", { id, ok: result === undefined ? null : result });
    }
  };
  const fail = (id, e) => {
    if (id != null) op("reply", { id, err: String((e && e.message) || e) });
    else throw e;
  };
  globalThis.__lightair_dispatch = (json) => {
    const m = JSON.parse(json);
    try {
      switch (m.type) {
        case "command": {
          const fn = handlers.command[m.command];
          if (!fn) throw new Error(`no command ${m.command}`);
          return reply(m.id, fn());
        }
        case "step": {
          const fn = handlers.step[m.step];
          if (!fn) throw new Error(`no step ${m.step}`);
          return reply(m.id, fn(m.params || {}));
        }
        case "event":
          for (const fn of handlers.event[m.event] || []) fn(m.data);
          return reply(m.id, null);
        case "ui": {
          const fn = handlers.ui[m.panel];
          if (fn) fn(m.element, m.value);
          return reply(m.id, null);
        }
        case "overlay": {
          const fn = handlers.overlay[m.overlay];
          if (fn) fn(m.message);
          return reply(m.id, null);
        }
        case "settings":
          for (const fn of handlers.settings) fn(m.values);
          return reply(m.id, null);
        case "timer": {
          const t = handlers.timer[m.timer];
          if (!t) return;
          if (!t.repeat) delete handlers.timer[m.timer];
          t.fn();
          return;
        }
      }
    } catch (e) {
      fail(m.id, e);
    }
  };
})();
