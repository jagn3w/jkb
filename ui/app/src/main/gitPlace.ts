//! Where a session's working directory is, as git's own files say (D53.9, *Jump to context*): the
//! checkout's root, its repo key and its branch — what `task.by_branch` is asked with.
//
// Read from the files, never by running git: the app runs unsandboxed on the host, and a repository
// under the repos mount is writable from the container, so its config (hooks, `core.fsmonitor`, …) is
// not something a host process may execute. Reading `.git` and `HEAD` is all this needs, the way
// `.container/lib.sh`'s `dc_git_head` reads the kit's commit.
//
// The renderer names the directory, so main trusts nothing about it: only a path under the repos
// directory (either side's spelling of it) is looked at, the walk up never leaves it, a git dir a
// `.git` file points to must be under it too, links are not followed, and only a few hundred bytes of
// any file are read. What crosses back is the place, never a file's contents.

import { closeSync, constants, fstatSync, lstatSync, openSync, readSync } from "node:fs";
import { posix } from "node:path";

import { parseGitdirFile, parseHead, repoKeyOf, type GitPlace } from "@jkb/core";

import { hostPathOf, type TerminalResult, type TerminalRoots } from "../shared/terminal";

/** What the reader needs of the filesystem; the real one by default, a stand-in in tests. */
export interface PlaceFs {
  /** `"dir"`, `"file"`, `"other"` (a link, a socket, …) or `undefined` when nothing is there. Never follows a link. */
  kind(path: string): "dir" | "file" | "other" | undefined;
  /** At most `max` bytes of a regular file, not following a link; `undefined` when it cannot be read. */
  read(path: string, max: number): string | undefined;
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
      fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW);
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
};

/** The most read of `.git` or `HEAD`: a line naming a path or a ref. */
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
 * The checkout `cwd` is in: walk up from it (on the host's side of the repos mount) to the first
 * directory holding `.git`, follow a linked worktree's `gitdir:` line, and read its `HEAD`.
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
  let dir = host;
  for (;;) {
    const dotGit = posix.join(dir, ".git");
    const kind = fs.kind(dotGit);
    // A `.git` that is a link (or anything else) is this checkout's, so the walk stops here rather
    // than climbing past it into an enclosing one; it is not followed.
    if (kind === "other") return bad(`${dotGit} is not a file or a directory, and links are not followed`);
    if (kind === "dir" || kind === "file") {
      let gitDir = dotGit;
      if (kind === "file") {
        const pointed = parseGitdirFile(fs.read(dotGit, MAX_READ) ?? "");
        if (pointed === undefined) return bad(`${dotGit} is not a worktree's gitdir file`);
        const absolute = clean(pointed.startsWith("/") ? pointed : posix.join(dir, pointed));
        const onHost = absolute === undefined ? undefined : hostPathOf(absolute, roots);
        if (onHost === undefined || !under(onHost, hostRepos)) {
          return bad(`${dotGit} points outside the repos directory`);
        }
        gitDir = onHost;
      }
      const branch = parseHead(fs.read(posix.join(gitDir, "HEAD"), MAX_READ) ?? "");
      if (branch === undefined) return bad(`${gitDir}/HEAD names no branch or commit`);
      const repo = repoKeyOf(dir);
      if (repo === undefined) return bad(`${dir} has no name to key a repo by`);
      return { ok: true, value: { root: dir, repo, branch } };
    }
    if (dir === hostRepos) return bad(`${asked} is not inside a git checkout`);
    dir = posix.dirname(dir);
  }
}
