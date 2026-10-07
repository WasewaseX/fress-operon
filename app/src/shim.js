// The Tauri IPC shim — injected before any page script runs. The ORIGINAL
// Fress frontend bundle talks to @tauri-apps/api 2.9, which calls
// window.__TAURI_INTERNALS__.invoke(...) / transformCallback(...). This
// shim provides exactly that surface, backed by the fress-operon host.
// Event listeners are registered entirely JS-side (the callback objects
// only exist here); the host dispatches through emitEvent().
(() => {
  if (window.__TAURI_INTERNALS__) return;
  const pending = new Map();    // invoke id -> {resolve, reject}
  const callbacks = new Map();  // transformCallback id -> {callback, once}
  const listens = new Map();    // eventId -> {event, callback, once}
  let nextInvokeId = 0;
  let nextCallbackId = 0;
  let nextEventId = 0;

  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } },
    plugins: {},
    transformCallback(callback, once = false) {
      const id = ++nextCallbackId;
      callbacks.set(id, { callback, once: !!once });
      return id;
    },
    unregisterCallback(id) {
      callbacks.delete(id);
    },
    convertFileSrc(filePath, protocol = 'asset') {
      return filePath;
    },
    invoke(cmd, args = {}, options) {
      // The event plugin lives entirely in this shim (callbacks are JS
      // objects the host can never see).
      if (cmd === 'plugin:event|listen') {
        const eventId = ++nextEventId;
        const cb = callbacks.get(args.handler);
        listens.set(eventId, {
          event: args.event,
          callback: cb ? cb.callback : () => {},
          once: cb ? cb.once : false,
        });
        return Promise.resolve(eventId);
      }
      if (cmd === 'plugin:event|unlisten') {
        listens.delete(args.eventId);
        return Promise.resolve(null);
      }
      return new Promise((resolve, reject) => {
        const id = ++nextInvokeId;
        pending.set(id, { resolve, reject });
        window.ipc.postMessage(JSON.stringify({ t: 'invoke', id, cmd, args: args ?? {} }));
      });
    },
  };

  // The api's event module calls this in its unlisten path before the
  // plugin invoke (which the shim resolves locally).
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
    unregisterListener(event, eventId) {
      listens.delete(eventId);
    },
  };

  window.__FRESS_HOST__ = {
    resolveInvoke(id, ok, valueJson) {
      const p = pending.get(id);
      if (!p) return;
      pending.delete(id);
      if (ok) {
        try {
          p.resolve(valueJson == null ? null : JSON.parse(valueJson));
        } catch (e) {
          p.resolve(valueJson);
        }
      } else {
        // Tauri rejects command errors with the raw error string.
        p.reject(valueJson);
      }
    },
    emitEvent(name, payloadJson) {
      let payload = payloadJson;
      try {
        payload = payloadJson == null ? null : JSON.parse(payloadJson);
      } catch (e) {}
      for (const [id, l] of Array.from(listens)) {
        if (l.event === name) {
          try {
            l.callback({ event: name, id, payload });
          } catch (e) {}
          if (l.once) listens.delete(id);
        }
      }
    },
  };
})();
