//! Where a session's working directory is, as git's own files say (D53.9, *Jump to context*): the
//! checkout's root, its repo key and its branch — what `task.by_branch` is asked with.
//
// Read from the files, never by running git: the app runs unsandboxed on the host, and a repository
// under the repos mount is writable from the container, so its config (hooks, `core.fsmonitor`, …) is
// not something a host process may execute. Reading `.git`, `HEAD` and a worktree's `commondir` is
// all this needs, the way `.container/lib.sh`'s `dc_git_head` reads the kit's commit.
//
// The renderer names the directory, so main trusts nothing about it: only a path under the repos
// directory (either side's spelling of it) is looked at, and only when, resolved, it is still under
// the repos directory's real path — a link anywhere along it, not only at its end, cannot lead the
// reader out *as the path stands when it is resolved*. The resolution and the later reads are
// separate steps, so a directory the container swaps for a link in between is followed; what such a
// race can reach is bounded by what is read and returned: a file is never a link itself
// (`O_NOFOLLOW`), is opened without blocking (a FIFO planted as `HEAD` would otherwise hang the main
// process in `open`), only a few hundred bytes are read, and only a parsed ref and the place cross
// back — never a file's contents. The walk up never leaves the mount, and a git dir a `.git` file
// points to (and the common dir a worktree's `commondir` names) must pass the same two checks.

import { closeSync, constants, fstatSync, lstatSync, openSync, readSync, realpathSync } from "node:fs";
import { posix } from "node:path";

import { parseGitdirFile, parseHead, repoKeyOf, type GitPlace } from "@jkb/core";

import { hostPathOf, type TerminalResult, type TerminalRoots } from "../shared/terminal";

/** What the reader needs of the filesystem; the real one by default, a stand-in in tests. */
export interface PlaceFs {
  /** `"dir"`, `"file"`, `"other"` (a link, a socket, …) or `undefined` when nothing is there. Never follows a link. */
  kind(path: string): "dir" | "file" | "other" | undefined;
  /**
   * At most `max` bytes of a regular file, not following a link and never blocking in `open`;
   * `undefined` when it cannot be read or is not a regular file.
   */
  read(path: string, max: number): string | undefined;
  /** `path` with every link along it resolved, or `undefined` when it does not resolve. */
  real(path: string): string | undefined;
}

export const machineFs: PlaceFs = {
  kind(path) {
    try {
      const st = lstatSync(path);
      return st.isDirectory() ? "dir" : st.isFile() ? "file" : "other";
    } catch {
      return undefined;
    }
  },
  read(path, max) {
    let fd: number | undefined;
    try {
      // O_NONBLOCK: opening a FIFO for reading otherwise waits for a writer, which the container can
      // simply never provide. The type is checked on the open descriptor, so nothing swaps it after.
      fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
      if (!fstatSync(fd).isFile()) return undefined;
      const buf = Buffer.alloc(max);
      const n = readSync(fd, buf, 0, max, 0);
      return buf.subarray(0, n).toString("utf8");
    } catch {
      return undefined;
    } finally {
      if (fd !== undefined) closeSync(fd);
    }
  },
  real(path) {
    try {
      return realpathSync(path);
    } catch {
      return undefined;
    }
  },
};

/** The most read of `.git`, `HEAD` or `commondir`: a line naming a path or a ref. */
const MAX_READ = 4096;

function under(path: string, root: string): boolean {
  return path === root || path.startsWith(`${root}/`);
}

/** `path` normalized, or `undefined` when it is not absolute or still climbs after normalizing. */
function clean(path: string): string | undefined {
  if (!path.startsWith("/") || path.includes("\0")) return undefined;
  const n = posix.normalize(path).replace(/\/+$/, "");
  return n === "" ? "/" : n;
}

/**
 * The checkout `cwd` is in: walk up from it (on the host's side of the repos mount, links resolved)
 * to the first directory holding `.git`, follow a linked worktree's `gitdir:` line, and read its
 * `HEAD`. The repo key is the MAIN checkout's basename — for a linked worktree, the directory its git
 * dir's `commondir` lives in — because that is what `jkb task work` tags a task's `repo=` with
 * (`repo_ctx`: `gitrepo::key(gitrepo::main_root(cwd))`), not the worktree's own directory name.
 */
export function gitPlace(cwd: unknown, roots: TerminalRoots, fs: PlaceFs = machineFs): TerminalResult<GitPlace> {
  const bad = (error: string): TerminalResult<GitPlace> => ({ ok: false, error });
  if (typeof cwd !== "string") return bad("a working directory is a string");
  const asked = clean(cwd);
  const hostRepos = clean(roots.hostRepos);
  if (asked === undefined || hostRepos === undefined) return bad(`${String(cwd)} is not an absolute path`);
  const host = hostPathOf(asked, roots);
  if (host === undefined || !under(host, hostRepos)) {
    return bad(`${asked} is outside the repos directory, so the app does not look at it`);
  }
  const reposReal = fs.real(hostRepos);
  if (reposReal === undefined) return bad(`the repos directory ${hostRepos} cannot be resolved`);
  /**
   * `path` (in either side's spelling, or already resolved) with its links resolved, when it is under
   * the repos directory both as written and as resolved — else why not: it is `outside` (as written,
   * or once resolved), or it is `missing` (it does not resolve: nothing is there).
   */
  type Located = { readonly real: string } | { readonly not: "outside" | "missing" };
  const locate = (path: string | undefined): Located => {
    if (path === undefined) return { not: "outside" };
    // Already resolved (a path made from one this walk resolved), or either side's spelling.
    const onHost = under(path, reposReal) ? path : hostPathOf(path, roots);
    if (onHost === undefined || !(under(onHost, hostRepos) || under(onHost, reposReal))) return { not: "outside" };
    const real = fs.real(onHost);
    if (real === undefined) return { not: "missing" };
    return under(real, reposReal) ? { real } : { not: "outside" };
  };
  /** A path a git file names, made absolute against `base`, located. */
  const named = (text: string, base: string): Located => locate(clean(text.startsWith("/") ? text : posix.join(base, text)));
  /** What a git file's line is told when it does not lead inside. */
  const whyNot = (file: string, located: { readonly not: "outside" | "missing" }): string =>
    located.not === "missing" ? `${file} names a directory that does not exist` : `${file} points outside the repos directory`;

  if (fs.real(host) === undefined) return bad(`${asked} does not exist`);
  const start = locate(host);
  if (!("real" in start)) return bad(`${asked} leads outside the repos directory through a link`);
  let dir = start.real;
  for (;;) {
    const dotGit = posix.join(dir, ".git");
    const kind = fs.kind(dotGit);
    // A `.git` that is a link (or anything else) is this checkout's, so the walk stops here rather
    // than climbing past it into an enclosing one; it is not followed.
    if (kind === "other") return bad(`${dotGit} is not a file or a directory, and links are not followed`);
    if (kind === "dir" || kind === "file") {
      let gitDir = dotGit;
      let mainRoot = dir;
      if (kind === "file") {
        const pointed = parseGitdirFile(fs.read(dotGit, MAX_READ) ?? "");
        if (pointed === undefined) return bad(`${dotGit} is not a worktree's gitdir file`);
        const resolved = named(pointed, dir);
        if (!("real" in resolved)) return bad(whyNot(dotGit, resolved));
        gitDir = resolved.real;
        // A linked worktree's git dir names the repository's common dir (`../..` from
        // `<main>/.git/worktrees/<name>`); the main checkout is the directory holding it, as
        // `gitrepo::main_root` takes the parent of `--git-common-dir`. A git dir without one (a
        // submodule's) is its own checkout's.
        const commonFile = posix.join(gitDir, "commondir");
        if (fs.kind(commonFile) !== undefined) {
          const line = fs.read(commonFile, MAX_READ)?.split("\n")[0]?.trim() ?? "";
          if (line === "" || line.includes("\0")) return bad(`${commonFile} names no directory`);
          const common = named(line, gitDir);
          if (!("real" in common)) return bad(whyNot(commonFile, common));
          mainRoot = posix.dirname(common.real);
        }
      }
      const branch = parseHead(fs.read(posix.join(gitDir, "HEAD"), MAX_READ) ?? "");
      if (branch === undefined) return bad(`${gitDir}/HEAD names no branch or commit`);
      const repo = repoKeyOf(mainRoot);
      if (repo === undefined) return bad(`${mainRoot} has no name to key a repo by`);
      // The root crosses in the host's own spelling of the repos directory, not its real path: the
      // renderer carries it across the mount by that prefix (*Shell here*), and a symlinked repos
      // directory would otherwise leave it with a path it cannot rebase.
      const root = posix.join(hostRepos, dir.slice(reposReal.length));
      return { ok: true, value: { root, repo, branch } };
    }
    if (dir === reposReal) return bad(`${asked} is not inside a git checkout`);
    dir = posix.dirname(dir);
  }
}
