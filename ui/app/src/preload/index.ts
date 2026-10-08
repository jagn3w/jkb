//! The preload: builds `window.jkb` from the bridge contract and nothing else.
//
// It runs sandboxed (`sandbox: true`), so it may import only `electron` — the bridge contract's
// channel names are bundled in. It exposes named functions, never `ipcRenderer` itself: a
// renderer that could send on any channel could reach any handler main ever registers.

import { contextBridge, ipcRenderer } from "electron";

import { BRIDGE_CHANNELS, type JkbBridge } from "../shared/bridge";

const bridge: JkbBridge = {
  hello: () => ipcRenderer.invoke(BRIDGE_CHANNELS.hello),
  op: (request) => ipcRenderer.invoke(BRIDGE_CHANNELS.op, request),
  info: () => ipcRenderer.invoke(BRIDGE_CHANNELS.info),
};

contextBridge.exposeInMainWorld("jkb", bridge);
