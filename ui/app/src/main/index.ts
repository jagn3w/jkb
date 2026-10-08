//! The main process: windows, the bridge's handlers, and the one `jkb serve` client (D53.1).
//
// The process split is a security boundary. The renderer runs with no Node
// (`contextIsolation`, `sandbox`, no `nodeIntegration`), may not navigate or open windows, and
// reaches main only through the three named bridge calls — each checked to come from the top
// frame of a window this process created, showing the app's own page. The daemon's token is read
// and held here and never crosses.

import { homedir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { REMOTE_VAR, TOKEN_FILE_VAR, daemonUrl, failed, portOf, tokenPath, type OpRequest } from "@jkb/core";
import { BrowserWindow, app, ipcMain, nativeTheme, type IpcMainInvokeEvent } from "electron";

import { BRIDGE_CHANNELS, type AppInfo } from "../shared/bridge";
import { DaemonClient } from "./daemon";

/** Set by `electron-vite dev` to the renderer's dev server; never honoured by a packaged app. */
const DEV_RENDERER_URL = (!app.isPackaged && process.env["ELECTRON_RENDERER_URL"]?.trim()) || undefined;
const RENDERER_FILE = join(__dirname, "../renderer/index.html");

const url = daemonUrl(process.env[REMOTE_VAR]);
const tokenFile = process.env[TOKEN_FILE_VAR]?.trim() || tokenPath(homedir(), portOf(url));
const daemon = new DaemonClient({ url, tokenFile });

/** The windows this process created: the only senders the bridge answers. */
const windows = new Set<number>();

/** Whether `frameUrl` is the app's own page, as opposed to anything it might be navigated to. */
function isAppPage(frameUrl: string): boolean {
  try {
    const page = new URL(frameUrl);
    if (DEV_RENDERER_URL !== undefined) return page.origin === new URL(DEV_RENDERER_URL).origin;
    page.search = "";
    page.hash = "";
    return page.protocol === "file:" && fileURLToPath(page) === RENDERER_FILE;
  } catch {
    return false;
  }
}

/** Refuse a bridge call from anything but the top frame of one of our windows, on our page. */
function assertTrusted(event: IpcMainInvokeEvent): void {
  const frame = event.senderFrame;
  if (!windows.has(event.sender.id) || frame === null || frame.parent !== null || !isAppPage(frame.url)) {
    throw new Error("bridge call refused: not from the app's own page");
  }
}

function isOpRequest(value: unknown): value is OpRequest {
  return (
    typeof value === "object" &&
    value !== null &&
    !Array.isArray(value) &&
    typeof (value as { op?: unknown }).op === "string"
  );
}

function registerBridge(): void {
  ipcMain.handle(BRIDGE_CHANNELS.hello, (event) => {
    assertTrusted(event);
    return daemon.hello();
  });
  ipcMain.handle(BRIDGE_CHANNELS.op, (event, request: unknown) => {
    assertTrusted(event);
    if (!isOpRequest(request)) return failed("bad_request", "an op is an object with a string `op`");
    return daemon.op(request);
  });
  ipcMain.handle(BRIDGE_CHANNELS.info, (event): AppInfo => {
    assertTrusted(event);
    return {
      daemonUrl: daemon.url,
      platform: process.platform,
      versions: {
        electron: process.versions.electron,
        chrome: process.versions.chrome,
        node: process.versions.node,
      },
    };
  });
}

function createWindow(): void {
  const win = new BrowserWindow({
    width: 1280,
    height: 820,
    minWidth: 720,
    minHeight: 480,
    show: false,
    title: "Code Factory",
    // The page's own --bg, so the first frame does not flash the other theme.
    backgroundColor: nativeTheme.shouldUseDarkColors ? "#191919" : "#ffffff",
    webPreferences: {
      preload: join(__dirname, "../preload/index.js"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webviewTag: false,
      spellcheck: false,
    },
  });
  const id = win.webContents.id;
  windows.add(id);
  win.on("closed", () => windows.delete(id));
  win.once("ready-to-show", () => win.show());

  // The app is one page. Links never open in it or in a new window.
  win.webContents.setWindowOpenHandler(() => ({ action: "deny" }));
  win.webContents.on("will-navigate", (event) => event.preventDefault());
  win.webContents.on("will-attach-webview", (event) => event.preventDefault());

  if (DEV_RENDERER_URL !== undefined) {
    void win.loadURL(DEV_RENDERER_URL);
  } else {
    void win.loadFile(RENDERER_FILE);
  }
}

// Nothing may be granted to a page by asking: no camera, notifications, clipboard reads, ….
app.on("web-contents-created", (_event, contents) => {
  contents.session.setPermissionRequestHandler((_wc, _permission, callback) => callback(false));
});

void app.whenReady().then(() => {
  registerBridge();
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
