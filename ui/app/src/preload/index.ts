//! The preload: builds `window.jkb` from the bridge contract and nothing else.
//
// It runs sandboxed (`sandbox: true`), so it may import only `electron` — the bridge contract's
// channel names are bundled in. It exposes named functions, never `ipcRenderer` itself: a
// renderer that could send on any channel could reach any handler main ever registers.

import { contextBridge, ipcRenderer, type IpcRendererEvent } from "electron";

import { BRIDGE_CHANNELS, type DesignFeedEvent, type JkbBridge } from "../shared/bridge";
import type { TerminalEvent } from "../shared/terminal";

const bridge: JkbBridge = {
  hello: () => ipcRenderer.invoke(BRIDGE_CHANNELS.hello),
  op: (request) => ipcRenderer.invoke(BRIDGE_CHANNELS.op, request),
  info: () => ipcRenderer.invoke(BRIDGE_CHANNELS.info),
  terminal: {
    open: (spec, cols, rows) => ipcRenderer.invoke(BRIDGE_CHANNELS.terminalOpen, spec, cols, rows),
    write: (id, data) => ipcRenderer.send(BRIDGE_CHANNELS.terminalWrite, id, data),
    resize: (id, cols, rows) => ipcRenderer.send(BRIDGE_CHANNELS.terminalResize, id, cols, rows),
    close: (id) => ipcRenderer.send(BRIDGE_CHANNELS.terminalClose, id),
    onEvent: (listener) => {
      // Only the payload reaches the renderer, never the IPC event (whose `sender` is an
      // `ipcRenderer` that could send on any channel).
      const handler = (_event: IpcRendererEvent, payload: TerminalEvent): void => listener(payload);
      ipcRenderer.on(BRIDGE_CHANNELS.terminalEvent, handler);
      return () => {
        ipcRenderer.removeListener(BRIDGE_CHANNELS.terminalEvent, handler);
      };
    },
  },
  design: {
    subscribe: (topic) => ipcRenderer.invoke(BRIDGE_CHANNELS.designSubscribe, topic),
    unsubscribe: (topic) => ipcRenderer.send(BRIDGE_CHANNELS.designUnsubscribe, topic),
    onEvent: (listener) => {
      const handler = (_event: IpcRendererEvent, payload: DesignFeedEvent): void => listener(payload);
      ipcRenderer.on(BRIDGE_CHANNELS.designEvent, handler);
      return () => {
        ipcRenderer.removeListener(BRIDGE_CHANNELS.designEvent, handler);
      };
    },
  },
  container: {
    status: () => ipcRenderer.invoke(BRIDGE_CHANNELS.containerStatus),
    spec: (action) => ipcRenderer.invoke(BRIDGE_CHANNELS.containerSpec, action),
  },
};

contextBridge.exposeInMainWorld("jkb", bridge);
