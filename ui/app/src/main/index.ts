//! The main process: windows, the bridge's handlers, and the one `jkb serve` client (D53.1).
//
// The process split is a security boundary. The renderer runs with no Node
// (`contextIsolation`, `sandbox`, no `nodeIntegration`), may not navigate or open windows, and
// reaches main only through the named bridge calls — each checked to come from the top frame of
// a window this process created, showing the app's own page. The daemon's token is read and held
// here and never crosses; so are the integrated terminal's PTYs (`terminals.ts`), whose output is
// the only thing about them that does.

import { homedir, userInfo } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { REMOTE_VAR, TOKEN_FILE_VAR, daemonUrl, failed, portOf, tokenPath, type OpRequest } from "@jkb/core";
import { BrowserWindow, app, ipcMain, nativeTheme, webContents, type IpcMainEvent, type IpcMainInvokeEvent } from "electron";
import type * as NodePty from "node-pty";

import { BRIDGE_CHANNELS, type AppInfo } from "../shared/bridge";
import type { TerminalResult, TerminalRoots } from "../shared/terminal";
import { DaemonClient } from "./daemon";
import { TerminalHost, machineEnvironment, type SpawnPty } from "./terminals";

/** Set by `electron-vite dev` to the renderer's dev server; never honoured by a packaged app. */
const DEV_RENDERER_URL = (!app.isPackaged && process.env["ELECTRON_RENDERER_URL"]?.trim()) || undefined;
const RENDERER_FILE = join(__dirname, "../renderer/index.html");

const url = daemonUrl(process.env[REMOTE_VAR]);
const tokenFile = process.env[TOKEN_FILE_VAR]?.trim() || tokenPath(homedir(), portOf(url));
const daemon = new DaemonClient({ url, tokenFile });

/**
 * Where terminals run: the container `.container/run.sh` starts (`JKB_CONTAINER_NAME`, the variable
 * it reads, default `jkb-dev`) and the one directory both sides see, the repos mount (`HOST_REPOS`
 * and `CTR_REPOS` there).
 */
const terminalRoots: TerminalRoots = {
  container: process.env["JKB_CONTAINER_NAME"]?.trim() || "jkb-dev",
  containerRepos: "/home/vscode/repos",
  hostRepos: join(homedir(), "repos"),
  hostHome: homedir(),
};

function loginShell(): string | undefined {
  try {
    return userInfo().shell ?? undefined;
  } catch {
    return undefined;
  }
}

/**
 * `node-pty`, loaded on the first terminal rather than at startup: it is a native module, and a
 * build that cannot load it should lose its terminals, not its window.
 */
let ptyModule: typeof NodePty | undefined;
const spawnPty: SpawnPty = (file, args, options) => {
  ptyModule ??= require("node-pty") as typeof NodePty;
  return ptyModule.spawn(file, args, options);
};

const terminals = new TerminalHost(
  spawnPty,
  machineEnvironment(terminalRoots, process.env, loginShell()),
  (owner, event) => {
    const contents = webContents.fromId(owner);
    if (contents !== undefined && !contents.isDestroyed()) contents.send(BRIDGE_CHANNELS.terminalEvent, event);
  },
);

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

/** Whether a bridge call came from the top frame of one of our windows, on our page. */
function isTrusted(event: IpcMainInvokeEvent | IpcMainEvent): boolean {
  const frame = event.senderFrame;
  return windows.has(event.sender.id) && frame !== null && frame.parent === null && isAppPage(frame.url);
}

/** Refuse a bridge call from anything but the top frame of one of our windows, on our page. */
function assertTrusted(event: IpcMainInvokeEvent): void {
  if (!isTrusted(event)) throw new Error("bridge call refused: not from the app's own page");
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
      terminal: terminalRoots,
    };
  });

  // The terminal. Main validates every argument (`terminals.ts`); a terminal answers only the
  // window that opened it. Writes, resizes and closes are fire-and-forget (`send`, not `invoke`):
  // one round trip per keystroke buys nothing, and an untrusted one is dropped.
  ipcMain.handle(BRIDGE_CHANNELS.terminalOpen, (event, spec: unknown, cols: unknown, rows: unknown): TerminalResult<unknown> => {
    assertTrusted(event);
    return terminals.open(event.sender.id, spec, cols, rows);
  });
  ipcMain.on(BRIDGE_CHANNELS.terminalWrite, (event, id: unknown, data: unknown) => {
    if (isTrusted(event)) terminals.write(event.sender.id, id, data);
  });
  ipcMain.on(BRIDGE_CHANNELS.terminalResize, (event, id: unknown, cols: unknown, rows: unknown) => {
    if (isTrusted(event)) terminals.resize(event.sender.id, id, cols, rows);
  });
  ipcMain.on(BRIDGE_CHANNELS.terminalClose, (event, id: unknown) => {
    if (isTrusted(event)) terminals.close(event.sender.id, id);
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
  win.on("closed", () => {
    windows.delete(id);
    terminals.closeAll(id);
  });
  // A reload starts a renderer that knows none of the old page's terminals: end them rather than
  // leave processes nobody can see or type into.
  win.webContents.on("did-start-navigation", (details) => {
    if (details.isMainFrame && !details.isSameDocument) terminals.closeAll(id);
  });
  win.webContents.on("render-process-gone", () => terminals.closeAll(id));
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

app.on("will-quit", () => terminals.closeAll());

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
