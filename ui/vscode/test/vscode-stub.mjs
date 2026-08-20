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
  /** What the user clicks on an information message with buttons; undefined = dismissed. */
  answer: undefined,
  calls: [],
  errors: [],
  asked: [],
};

export function reset(folder) {
  state.folder = folder;
  state.claudeInstalled = true;
  state.claudeRefuses = false;
  state.openFolderFails = false;
  state.answer = undefined;
  state.calls = [];
  state.errors = [];
  state.asked = [];
}

export const workspace = {
  get workspaceFolders() {
    return state.folder === undefined ? undefined : [{ uri: { fsPath: state.folder } }];
  },
};

export const extensions = {
  getExtension(id) {
    if (!state.claudeInstalled) return undefined;
    return { id, isActive: true, activate: async () => {} };
  },
};

export const commands = {
  async executeCommand(name, ...args) {
    state.calls.push([name, ...args]);
    if (name.startsWith("claude-vscode.") && state.claudeRefuses) throw new Error("refused");
    if (name === "vscode.openFolder" && state.openFolderFails) throw new Error("no window");
  },
};

export const Uri = { file: (fsPath) => ({ fsPath }) };

export const window = {
  showErrorMessage: (message) => state.errors.push(message),
  /** Records the question and returns the scripted answer, as a dismissible prompt does. */
  showInformationMessage: async (message, ...items) => {
    state.asked.push({ message, items });
    return state.answer;
  },
};
