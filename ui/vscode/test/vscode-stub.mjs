//! Enough of the `vscode` API for `claude.ts`, with recorders.
//
// The module under test is glue over two APIs we do not own — VS Code's and the Claude Code
// extension's — so what is worth pinning is the part that is ours: which command is asked
// for, with which arguments, and the hand-off queue between two windows. A stub is the only
// way to ask that without a running VS Code, and it is honest about its limit: it proves the
// hand-off, not that VS Code opens a window.

/** Recorders and knobs, reset by each test. */
export const state = {
  /** This window's first workspace folder, or undefined when it has none. */
  folder: undefined,
  claudeInstalled: true,
  /** Make the Claude Code command refuse, as an older or broken extension would. */
  claudeRefuses: false,
  /** Make `vscode.openFolder` fail. */
  openFolderFails: false,
  /** The Claude Code extension starts inactive; its command throws until activate() resolves. */
  claudeActive: true,
  /** `jkb.taskLauncher`. */
  launcher: "auto",
  /** Scheme/authority of this window's folder, so a remote workspace can be modelled. */
  scheme: "file",
  authority: "",
  calls: [],
  errors: [],
  notices: [],
};

export function reset(folder) {
  state.folder = folder;
  state.claudeInstalled = true;
  state.claudeRefuses = false;
  state.openFolderFails = false;
  state.claudeActive = true;
  state.launcher = "auto";
  state.scheme = "file";
  state.authority = "";
  state.calls = [];
  state.errors = [];
  state.notices = [];
  window.terminals.length = 0;
}

/** A Uri-alike: `with` keeps scheme and authority, which is what the remote fix turns on. */
const uri = (fields) => ({
  ...fields,
  fsPath: fields.path,
  // Merges over the receiver, as vscode.Uri.with does. Re-reading the globals instead meant
  // the remote test's scheme/authority assertions described the knobs it had set rather than
  // what the code passed — a mutation that forced scheme back to "file" stayed green.
  with: (change) => uri({ ...fields, ...change }),
});
const folderOf = (fsPath) =>
  uri({ scheme: state.scheme, authority: state.authority, path: fsPath });

export const workspace = {
  get workspaceFolders() {
    return state.folder === undefined ? undefined : [{ uri: folderOf(state.folder) }];
  },
  // Section and key are honoured. Returning `state.launcher` for anything meant the code could
  // read `getConfiguration("claude").get("launcherTypo")` with all tests green — and in real
  // VS Code that is `undefined`, silently putting every operator back on the default.
  getConfiguration: (section) => ({
    get: (key) => (section === "jkb" && key === "taskLauncher" ? state.launcher : undefined),
  }),
};

export const extensions = {
  getExtension(id) {
    if (!state.claudeInstalled) return undefined;
    return {
      id,
      get isActive() {
        return state.claudeActive;
      },
      // Real activation registers the extension's commands; until then they do not resolve.
      activate: async () => {
        state.calls.push(["activate", id]);
        state.claudeActive = true;
      },
    };
  },
};

export const commands = {
  async executeCommand(name, ...args) {
    state.calls.push([name, ...args]);
    if (name.startsWith("claude-vscode.")) {
      // `true` throws an Error; any other truthy value is thrown AS IS, because
      // `executeCommand` propagates a rejection value unchanged and a command that rejects
      // with a bare string is the case `causeOf` exists for. Always wrapping in an Error made
      // that test unable to fail — it survived the mutation that removes `causeOf`.
      if (state.claudeRefuses) {
        throw state.claudeRefuses === true ? new Error("refused") : state.claudeRefuses;
      }
      // The ordering hazard `onStartupFinished` introduces: two extensions activate
      // concurrently, and a command of one is unregistered until its activate() has run.
      if (!state.claudeActive) throw new Error(`command '${name}' not found`);
    }
    if (name === "vscode.openFolder" && state.openFolderFails) throw new Error("no window");
  },
};

export const Uri = { file: (fsPath) => uri({ scheme: "file", authority: "", path: fsPath }) };

/** Real VS Code's Disposable: a class wrapping a teardown callback. */
export class Disposable {
  constructor(callOnDispose) {
    this.dispose = callOnDispose;
  }
}

export const window = {
  showErrorMessage: (message) => state.errors.push(message),
  showInformationMessage: (message) => state.notices.push(message),
  // `deliverQueuedPrompt` defaults its terminal host to `vscode.window`, so the stub has to
  // BE one — without these the default parameter is a seam no test can reach, and the first
  // thing it did on a real fallback was throw.
  // A REAL TerminalHost: created terminals join `terminals`, so the default-parameter path —
  // the only one where sendText actually runs — is exercised against a populated list. While
  // it stayed empty, a receiving window blind to its own terminal passed every test.
  terminals: [],
  createTerminal(options) {
    state.calls.push(["createTerminal", options.name, options.cwd]);
    const terminal = {
      name: options.name,
      creationOptions: { cwd: options.cwd },
      show: () => state.calls.push(["showTerminal", options.name]),
      sendText: (text) => state.calls.push(["sendText", text]),
    };
    window.terminals.push(terminal);
    return terminal;
  },
};
