//! The main process: windows, the bridge's handlers, and the one `jkb serve` client (D53.1).
//
// The process split is a security boundary. The renderer runs with no Node
// (`contextIsolation`, `sandbox`, no `nodeIntegration`), may not navigate or open windows, and
// reaches main only through the named bridge calls — each checked to come from the top frame of
// a window this process created, showing the app's own page. The daemon's token is read and held
// here and never crosses; so are the integrated terminal's PTYs (`terminals.ts`), whose output is
// the only thing about them that does.
//
// It runs from the installed copy (D53.3): a packaged app, built from a host-side clean clone of
// `origin/main` and updated from the menu (*jkb ▸ Update from main…*, `update.ts`). Run from anywhere
// else — a checkout's `out/`, or a package left in a checkout's `dist/` — it refuses unless
// `JKB_APP_FROM_CHECKOUT=1` says that is deliberate.

import { homedir, userInfo } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  REMOTE_VAR,
  TOKEN_FILE_VAR,
  checkoutRefusal,
  daemonUrl,
  failed,
  installedExecutable,
  portOf,
  tokenPath,
  updateSummary,
  type OpRequest,
} from "@jkb/core";
import {
  BrowserWindow,
  Menu,
  app,
  dialog,
  ipcMain,
  nativeTheme,
  webContents,
  type IpcMainEvent,
  type IpcMainInvokeEvent,
  type MenuItemConstructorOptions,
  type MessageBoxOptions,
  type MessageBoxReturnValue,
} from "electron";
import type * as NodePty from "node-pty";

import { BRIDGE_CHANNELS, type AppInfo } from "../shared/bridge";
import type { TerminalResult, TerminalRoots } from "../shared/terminal";
import { ContainerKit, machineKit } from "./container";
import { DaemonClient } from "./daemon";
import { DesignFeeds } from "./designFeeds";
import { rendererSource } from "./devRenderer";
import { gitPlace } from "./gitPlace";
import { NotifyFeed } from "./notifyFeed";
import { TerminalHost, accountHome, machineEnvironment, machineRoots, type SpawnPty } from "./terminals";
import { AppUpdater, builtCommit, isInstalledCopy, machineRunner } from "./update";

/**
 * Where the page comes from: the built file, or — from `pnpm run dev` only, and only on loopback —
 * the dev server `electron-vite dev` names (`devRenderer.ts`). Never a dev server in a packaged app.
 */
const rendererFrom = rendererSource(app.isPackaged, process.env);

/**
 * Why this process may not run — it is not the installed copy (a package whose executable, links
 * resolved, is the one build-app.sh installed), and nobody said that was deliberate; or it was
 * pointed at a renderer it will not load — or `undefined`.
 */
const refusal =
  checkoutRefusal(
    app.isPackaged && isInstalledCopy(app.getPath("exe"), process.platform, accountHome()),
    process.env,
    installedExecutable(process.platform, accountHome()),
  ) ?? (rendererFrom.kind === "refused" ? rendererFrom.reason : undefined);

/** The dev server's page, when that is where the page comes from. */
const DEV_RENDERER_URL = rendererFrom.kind === "dev" ? rendererFrom.url : undefined;
const RENDERER_FILE = join(__dirname, "../renderer/index.html");

const url = daemonUrl(process.env[REMOTE_VAR]);
const tokenFile = process.env[TOKEN_FILE_VAR]?.trim() || tokenPath(homedir(), portOf(url));
const daemon = new DaemonClient({ url, tokenFile, trustedRoot: homedir() });

/**
 * Where terminals run (`machineRoots`): the container `.container/run.sh` starts and the repos
 * mount, both spellings of the host's side resolved once, here.
 */
const terminalRoots: TerminalRoots = machineRoots(homedir(), process.env);


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

/** The installed container kit (D53.8), under the ACCOUNT's home (`accountHome`). */
const containerKit = new ContainerKit(machineKit(accountHome(), process.env));

/**
 * The installed copy's clean clone and its builder, under the same account home as the kit (D53.3).
 * It is told what is running — this executable, and the commit built into `out/` (this file is
 * `out/main/index.js`) — and updates only the installed copy.
 */
const updater = new AppUpdater(accountHome(), machineRunner(accountHome(), process.env), {
  exe: app.getPath("exe"),
  platform: process.platform,
  commit: builtCommit(join(__dirname, "..")),
});

/** Live design updates, one long-poll per open design shared by every window showing it (D53.4). */
const designFeeds = new DesignFeeds(
  (request, options) => daemon.op(request, options),
  (owner, event) => {
    const contents = webContents.fromId(owner);
    if (contents !== undefined && !contents.isDestroyed()) contents.send(BRIDGE_CHANNELS.designEvent, event);
  },
);

/** The needs-input feed: the app's own consumer group on `claude/notify`, one for every window (D53.9). */
const notifyFeed = new NotifyFeed(
  (request, options) => daemon.op(request, options),
  (owner, event) => {
    const contents = webContents.fromId(owner);
    if (contents !== undefined && !contents.isDestroyed()) contents.send(BRIDGE_CHANNELS.notifyEvent, event);
  },
  { log: (message) => process.stderr.write(`code-factory: ${message}\n`) },
);

/** Every feed a window can hold, ended together when it closes or reloads. */
function closeFeeds(owner?: number): void {
  designFeeds.closeAll(owner);
  notifyFeed.closeAll(owner);
}

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
  // window that opened it. Writes, resizes and acks are fire-and-forget (`send`, not `invoke`):
  // one round trip per keystroke buys nothing, and an untrusted one is dropped. Close is invoked:
  // its answer (`TerminalEnd`) is the only confirmation that the program ended.
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
  ipcMain.on(BRIDGE_CHANNELS.terminalAck, (event, id: unknown, chars: unknown) => {
    if (isTrusted(event)) terminals.ack(event.sender.id, id, chars);
  });
  ipcMain.handle(BRIDGE_CHANNELS.terminalClose, (event, id: unknown) => {
    assertTrusted(event);
    return terminals.close(event.sender.id, id);
  });


  // Live design updates. Only a design's own topic is subscribed to (`isDesignTopic`, checked in
  // the feeds), with the app's one consumer group.
  ipcMain.handle(BRIDGE_CHANNELS.designSubscribe, (event, topic: unknown) => {
    assertTrusted(event);
    return designFeeds.subscribe(event.sender.id, topic);
  });
  ipcMain.on(BRIDGE_CHANNELS.designUnsubscribe, (event, topic: unknown) => {
    if (isTrusted(event)) designFeeds.unsubscribe(event.sender.id, topic);
  });

  // The dev container, through the kit's run.sh (D53.8). The renderer names an action; main finds
  // the kit, checks it, and builds the terminal spec that runs the action's one flag.
  ipcMain.handle(BRIDGE_CHANNELS.containerStatus, (event) => {
    assertTrusted(event);
    return containerKit.status();
  });
  ipcMain.handle(BRIDGE_CHANNELS.containerSpec, async (event, action: unknown) => {
    assertTrusted(event);
    const answer = await containerKit.spec(action);
    // Main built this host spec from the kit it located and the action's one flag, not from
    // anything the renderer sent: the one kind of host program a window may open.
    if (answer.ok) terminals.issueHost(event.sender.id, answer.value);
    return answer;
  });

  // The Sessions tab (D53.9): the needs-input feed — `claude/notify` and nothing else, with the
  // app's one group — and where a session's directory is, read from git's files.
  ipcMain.handle(BRIDGE_CHANNELS.notifySubscribe, (event) => {
    assertTrusted(event);
    return notifyFeed.join(event.sender.id);
  });
  ipcMain.on(BRIDGE_CHANNELS.notifyUnsubscribe, (event) => {
    if (isTrusted(event)) notifyFeed.leave(event.sender.id);
  });
  ipcMain.handle(BRIDGE_CHANNELS.sessionsPlace, (event, cwd: unknown) => {
    assertTrusted(event);
    return gitPlace(cwd, terminalRoots);
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
    closeFeeds(id);
  });
  // A reload starts a renderer that knows none of the old page's terminals: end them rather than
  // leave processes nobody can see or type into.
  win.webContents.on("did-start-navigation", (details) => {
    if (details.isMainFrame && !details.isSameDocument) {
      terminals.closeAll(id);
      closeFeeds(id);
    }
  });
  win.webContents.on("render-process-gone", () => {
    terminals.closeAll(id);
    closeFeeds(id);
  });
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

/** A message box, over the focused window when there is one. */
function messageBox(options: MessageBoxOptions): Promise<MessageBoxReturnValue> {
  const win = BrowserWindow.getFocusedWindow();
  return win === null ? dialog.showMessageBox(options) : dialog.showMessageBox(win, options);
}

/** Every window's progress bar: indeterminate while `busy`, cleared otherwise. */
function showBusy(busy: boolean): void {
  for (const win of BrowserWindow.getAllWindows()) win.setProgressBar(busy ? 2 : -1);
}

/**
 * *jkb ▸ Update from main…*: fetch `main` into the clean clone, show the commits it would take,
 * and on a yes build exactly that commit, swap it in and relaunch (D53.3).
 */
async function updateFromMain(): Promise<void> {
  if (updater.busy) {
    await messageBox({ type: "info", message: "An update is already building." });
    return;
  }
  showBusy(true);
  const plan = await updater.plan();
  showBusy(false);
  if (!plan.ok) {
    await messageBox({ type: "error", message: "Could not check main for an update", detail: plan.error });
    return;
  }
  const summary = updateSummary(plan.value);
  if (summary === undefined) {
    await messageBox({ type: "info", message: "Code Factory is up to date with main", detail: `Installed: ${plan.value.target.slice(0, 12)}` });
    return;
  }
  const { response } = await messageBox({
    type: "question",
    buttons: ["Update and Relaunch", "Cancel"],
    defaultId: 0,
    cancelId: 1,
    message: summary.message,
    detail: summary.detail,
  });
  if (response !== 0) return;
  showBusy(true);
  const done = await updater.apply(plan.value.target);
  showBusy(false);
  // Quitting cancelled it (`before-quit`): no dialog over a quit.
  if (quitting) return;
  if (!done.ok) {
    await messageBox({ type: "error", message: "The update did not install", detail: done.error });
    return;
  }
  if (done.value.unrecorded) {
    await messageBox({
      type: "warning",
      message: "Installed, but not recorded",
      detail: `Code Factory ${done.value.target.slice(0, 12)} is in place, but its stamp could not be written (see ${updater.logFile}). It relaunches now; the next install records it.`,
    });
  }
  // The updater runs only as the installed copy, and the build swapped the new one in at that path,
  // so relaunching starts the new build. At once: until this process goes, the helpers it spawns
  // come from the new bundle (lib.sh's app_swap). `quit`, not `exit`, so `will-quit` ends the
  // terminals and feeds as on any quit.
  app.relaunch();
  app.quit();
}

/** The application menu: the platform's standard menus, plus *jkb* with *Update from main…*. */
function installMenu(): void {
  const mac = process.platform === "darwin";
  const jkb: MenuItemConstructorOptions = {
    label: "jkb",
    submenu: [
      { label: "Update from main…", click: () => void updateFromMain() },
      // On macOS Quit is in the app menu; elsewhere this is the first menu, and it goes here.
      ...(mac ? [] : [{ type: "separator" as const }, { role: "quit" as const }]),
    ],
  };
  const template: MenuItemConstructorOptions[] = [
    ...(mac ? [{ role: "appMenu" as const }] : []),
    jkb,
    { role: "editMenu" },
    { role: "viewMenu" },
    { role: "windowMenu" },
  ];
  Menu.setApplicationMenu(Menu.buildFromTemplate(template));
}

// Nothing may be granted to a page by asking: no camera, notifications, clipboard reads, ….
app.on("web-contents-created", (_event, contents) => {
  contents.session.setPermissionRequestHandler((_wc, _permission, callback) => callback(false));
});

if (refusal !== undefined) {
  process.stderr.write(`code-factory: ${refusal}\n`);
  app.exit(1);
} else {
  void app.whenReady().then(() => {
    installMenu();
    registerBridge();
    createWindow();
    app.on("activate", () => {
      if (BrowserWindow.getAllWindows().length === 0) createWindow();
    });
  });
}

/** Set once a quit has begun, so an update it cancels shows no dialog. */
let quitting = false;

// A quit during an update stops the build first — its whole process group — rather than leaving it
// to build, swap and stamp with nobody to relaunch, under a lock whose holder is gone.
app.on("before-quit", (event) => {
  quitting = true;
  if (!updater.busy) return;
  event.preventDefault();
  void updater.cancel().then(() => app.quit());
});

/** How long quitting waits for the daemon to take the app's group off `claude/notify` (D53.9). */
const LEAVE_WAIT_MS = 1500;
let leftNotify = false;

app.on("will-quit", (event) => {
  terminals.closeAll();
  closeFeeds();
  // Once: hold the quit until the app's group is off `claude/notify` (or the wait runs out), then
  // quit again. A group left there would keep every later notification unreapable until the topic's
  // cap refuses `notify.event`.
  if (leftNotify) return;
  leftNotify = true;
  event.preventDefault();
  void notifyFeed.leaveAll(LEAVE_WAIT_MS).finally(() => app.quit());
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
