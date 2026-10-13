//! Git queries backing the task/branch lifecycle (design D34.2).
//!
//! Everything here shells out to `git` in a working directory. jkb does not link a git
//! library: the authority on what merged is the user's own git, with their config, remotes
//! and refs — reimplementing that against a second implementation of the object model is
//! how the answer starts disagreeing with `git log`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use jkb_fsm::Fact;

/// Branch names tried, in order, when a repo does not say which branch is its trunk.
const DEFAULT_TRUNKS: &[&str] = &["main", "master", "trunk", "develop"];

/// `git -C <dir> <args…>`, with the caller's repository selection stripped out.
///
/// EVERY git spawn in this module is built here. `GIT_DIR`, `GIT_WORK_TREE` and
/// `GIT_COMMON_DIR` outrank `-C`, so with `GIT_WORK_TREE` exported — the standard
/// bare-dotfiles shell recipe — `rev-parse --show-toplevel` answers somebody else's tree.
/// `repo::main_root` would then resolve that repository as "the repo", and `jkb task work`
/// creates a worktree under it and adds `/.jkb/` to its `.git/info/exclude`. jkb runs inside
/// other people's professional repositories and must not decorate them — the same rule that
/// keeps it from writing a git ref (D46). `scripts/lib.sh::_git` is this rule's shell half.
///
/// Everything that names a repository or a PART of one — six variables, not the three this
/// sentence claimed until round 28, when `jkb task work` was measured rewriting a foreign
/// repository's index through an inherited `GIT_INDEX_FILE`. `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS` inject
/// configuration and are deliberately left alone: this project's own dev container uses them
/// to carry `safe.directory` grants, and stripping those makes git refuse the checkout
/// outright.
///
/// **No hooks and no fsmonitor, ever** (design D52.11). A repository's own `.git/config` and
/// `.git/hooks` are writable from the dev container, and git runs whatever they name; jkb's plumbing
/// needs neither, and its only graft already ran hooks-off for its own reasons. What `-c` cannot
/// neutralize — a planted filter, textconv or pager — [`audit_repo_config`] refuses before every
/// call.
///
/// **And never into a submodule.** Recursing runs git inside the submodule under the submodule's own
/// config, which the audit does not judge (measured on git 2.51.1: a superproject `git status` ran a
/// clean filter planted in `.git/modules/sub/config`; with `diff.ignoreSubmodules=dirty` it did not,
/// and still reported a moved submodule commit). So jkb's git reads a submodule only as a commit its
/// superproject records — nothing about a submodule's own work tree reaches jkb's answers.
fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut all: Vec<&str> = NEUTRALIZERS.to_vec();
    let (options, sub_and_rest) = args.split_at(subcommand_at(args));
    all.extend_from_slice(options);
    if let Some((sub, rest)) = sub_and_rest.split_first() {
        all.push(sub);
        // On the command line as well as in `-c`: a tracked `.gitmodules` can set
        // `submodule.<name>.ignore=none`, which outranks `diff.ignoreSubmodules` — measured on git
        // 2.51.1, it put `status` back into the submodule, and the planted filter ran. The option is
        // the one thing a file in the repository cannot override.
        if matches!(*sub, "status" | "diff") {
            all.push("--ignore-submodules=dirty");
        }
        // And `checkout`/`switch` report local changes after moving, which enters every populated
        // submodule — `.gitmodules` `ignore=none` beats the `-c`, and a branch can bring a submodule
        // whose name only the TARGET tree's `.gitmodules` knows (measured on git 2.51.1: the planted
        // filter ran on `checkout --detach`, not on `checkout --quiet --detach`). `--quiet` skips that
        // report; errors still print.
        if matches!(*sub, "checkout" | "switch") {
            all.push("--quiet");
        }
        all.extend_from_slice(rest);
    }
    git_in(dir, &all)
}

/// The git subcommands jkb's own git may run: each measured, on git 2.51.1 against a submodule whose
/// config holds a planted filter and whose `.gitmodules` says `ignore = none`, not to enter it under
/// [`git_cmd`]'s settings. `add -A`, `stash`, `cherry-pick` and `diff-index` did enter it, so a
/// subcommand is added here only with a measurement of its own — refused at runtime, in
/// [`checked`], rather than left to every caller to remember.
const SAFE_SUBCOMMANDS: &[&str] = &[
    "rev-parse",
    "symbolic-ref",
    "config",
    "worktree",
    "for-each-ref",
    "branch",
    "status",
    "diff",
    "ls-files",
    "rev-list",
    "switch",
    "checkout",
    "merge",
    "rebase",
    "reset",
    // Ref plumbing only: it reads and writes `refs/`, never an index or a work tree. Measured on
    // git 2.51.1 by `jkb_s_git_does_not_run_a_submodule_s_config` — and by hand without jkb's
    // settings at all, where a bare superproject `git status` ran the planted filter and a bare
    // `git update-ref -d` did not.
    "update-ref",
    // Only as `clean -fd` (one `-f`, no `-x`), which `prepare_graft_tree` runs: it deletes untracked
    // files and does not descend into a nested repository or submodule. Measured on git 2.51.1 by
    // `jkb_s_git_does_not_run_a_submodule_s_config`: the planted filter did not run, and an
    // untracked file inside the submodule survived while the superproject's own was removed.
    "clean",
];

/// Where the subcommand is in `args`: past any leading `-c <key>=<value>` pairs, the one global
/// option a caller passes.
fn subcommand_at(args: &[&str]) -> usize {
    let mut i = 0;
    while args.get(i) == Some(&"-c") {
        i += 2;
    }
    i.min(args.len())
}

/// Refuse a subcommand outside [`SAFE_SUBCOMMANDS`], then audit `dir` — what every jkb git call asks
/// before it runs.
fn checked(dir: &Path, args: &[&str]) -> Result<()> {
    let sub = args.get(subcommand_at(args)).copied().unwrap_or_default();
    anyhow::ensure!(
        SAFE_SUBCOMMANDS.contains(&sub),
        "jkb's git does not run `git {sub}`: it is not among the subcommands measured not to enter a \
         submodule (design D52.11)"
    );
    audit_repo_config(dir)
}

/// The `-c` settings every jkb git call runs with ([`git_cmd`]).
const NEUTRALIZERS: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "diff.ignoreSubmodules=dirty",
    "-c",
    "submodule.recurse=false",
    "-c",
    "fetch.recurseSubmodules=false",
    "-c",
    "push.recurseSubmodules=no",
];

/// The **one** production `git` spawn in this module: `git -C <dir> <args>`, repository selection
/// scrubbed. [`git_cmd`] adds the neutralizers to it; [`audit_repo_config`] reads config through it.
fn git_in(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    scrub_repo_selection(&mut cmd);
    cmd
}

/// Whether `key` (as git lists it: section and name lower-cased, a subsection verbatim) may appear in
/// a repository's own config — the **repository-shape** keys: where it fetches, what its branches
/// track, how it is laid out, who commits. An allowlist, not a denylist, because git keeps growing
/// keys that run programs (`core.fsmonitor`, `core.sshCommand`, `core.pager`, `*.textconv`, `filter.*`,
/// `merge.*.driver`, `credential.*`, `gpg.program`, `alias.*`, `pager.*`, `submodule.*.update`,
/// `include.*`) and a denylist fails open on the next one.
pub(crate) fn config_key_allowed(key: &str) -> bool {
    let (section, rest) = key.split_once('.').unwrap_or((key, ""));
    let name = rest.rsplit('.').next().unwrap_or(rest);
    let has_sub = rest.contains('.');
    match section {
        "core" => {
            !has_sub
                && matches!(
                    name,
                    "repositoryformatversion"
                        | "filemode"
                        | "bare"
                        | "logallrefupdates"
                        | "ignorecase"
                        | "precomposeunicode"
                        | "symlinks"
                        | "autocrlf"
                        | "eol"
                        | "safecrlf"
                        | "abbrev"
                        | "quotepath"
                        | "sparsecheckout"
                        | "sparsecheckoutcone"
                        | "commitgraph"
                        | "splitindex"
                        | "untrackedcache"
                        | "multipackindex"
                )
        }
        "remote" => {
            has_sub
                && matches!(
                    name,
                    "url"
                        | "fetch"
                        | "pushurl"
                        | "push"
                        | "tagopt"
                        | "prune"
                        | "mirror"
                        | "promisor"
                        | "partialclonefilter"
                )
        }
        "branch" => {
            has_sub
                && matches!(
                    name,
                    "remote" | "merge" | "rebase" | "pushremote" | "description"
                        // VS Code's Git extension records a branch's merge base here.
                        | "vscode-merge-base"
                )
        }
        "submodule" => has_sub && matches!(name, "url" | "active" | "branch"),
        "extensions" => {
            !has_sub
                && matches!(
                    name,
                    "objectformat" | "refstorage" | "relativeworktrees" | "preciousobjects"
                )
        }
        "worktree" => !has_sub && name == "userelativepaths",
        // Scalar preferences with no program in them, key by key: a whole section admitted
        // `status.showUntrackedFiles=no`, which silences the dirty check a landing asks before it
        // writes a tree. `core.worktree` is gone for the same reason — it points checkout at any
        // directory, `$HOME` included (measured on git 2.51.1); jkb's worktrees are `.git` files.
        "user" => !has_sub && matches!(name, "name" | "email" | "signingkey" | "useconfigonly"),
        "init" => !has_sub && name == "defaultbranch",
        "pull" => !has_sub && matches!(name, "rebase" | "ff"),
        "push" => {
            !has_sub
                && matches!(
                    name,
                    "default" | "autosetupremote" | "followtags" | "recursesubmodules"
                )
        }
        "fetch" => {
            !has_sub
                && matches!(
                    name,
                    "prune" | "prunetags" | "writecommitgraph" | "recursesubmodules" | "parallel"
                )
        }
        "gc" => {
            !has_sub
                && matches!(
                    name,
                    "auto" | "autodetach" | "autopacklimit" | "reflogexpire" | "writecommitgraph"
                )
        }
        "rerere" => !has_sub && matches!(name, "enabled" | "autoupdate"),
        "log" => !has_sub && matches!(name, "date" | "decorate" | "follow" | "showsignature"),
        "commit" => !has_sub && matches!(name, "gpgsign" | "verbose" | "cleanup"),
        "tag" => !has_sub && matches!(name, "gpgsign" | "sort"),
        "index" => !has_sub && matches!(name, "version" | "threads" | "skiphash"),
        "pack" => !has_sub && matches!(name, "threads" | "writebitmaps"),
        "maintenance" => !has_sub && matches!(name, "auto" | "strategy"),
        // Display only: no key in either names a program or changes what a command reports.
        "advice" => !has_sub,
        "color" => true,
        "merge" | "diff" => {
            !has_sub && matches!(name, "conflictstyle" | "ff" | "renames" | "algorithm")
        }
        _ => false,
    }
}

/// Refuse to run git in `dir` when its repository could make git run a program there (design D52.11,
/// hole H5): its **own** config — local, worktree, a file those include, or a submodule's — sets a key
/// outside [`config_key_allowed`], or its git directory takes its config and hooks from somewhere that
/// is not its repository ([`check_layout`]). Those files are writable from the dev container, so on
/// the host they are a way for anything in the container to run code as the operator.
///
/// Asked with `git config --list --show-origin --show-scope` and `git rev-parse`, which read config and
/// execute nothing (measured on git 2.51.1 against a planted fsmonitor, hooks path, include, pager and
/// filter). The operator's own scopes (`system`, `global`) and `command` (the environment's, e.g. the
/// container's `safe.directory` grants) are theirs, not the repository's, and are not judged — nor is
/// an `unknown` one outside the repository ([`judged_scope`]).
///
/// **Asked fresh before every git call.** It was once per directory per process, and the reap service
/// is one long process: a config it passed on its first pass was never read again, so a filter planted
/// afterwards ran on the next `git status` it made (measured on git 2.51.1). Two spawns per call is the
/// price of the answer being about now.
///
/// # Errors
/// The refusal, naming the key or file and how to remove it; or git not running at all.
pub(crate) fn audit_repo_config(dir: &Path) -> Result<()> {
    check_repo_config(dir)
}

/// [`check_repo_config`]'s refusal when git cannot read a repository's config at all — a config it
/// cannot parse, or a `.git` file naming a git directory that is gone. Nothing in it was judged, so
/// it is not a finding of a planted key, and the reap scan words it apart ([`crate::git_audit`]).
#[derive(Debug)]
pub(crate) struct ConfigUnreadable {
    dir: PathBuf,
    git_says: String,
}

impl std::fmt::Display for ConfigUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "git could not read the config for {}: {}",
            self.dir.display(),
            self.git_says
        )
    }
}

impl std::error::Error for ConfigUnreadable {}

/// [`audit_repo_config`]'s check — also what the reap service's scan asks of every repository
/// (`git_audit`), so the two cannot come to disagree.
///
/// # Errors
/// As [`audit_repo_config`].
pub(crate) fn check_repo_config(dir: &Path) -> Result<()> {
    let out = git_in(
        dir,
        &["config", "--list", "--show-origin", "--show-scope", "-z"],
    )
    .output()
    .with_context(|| format!("reading {}'s git config", dir.display()))?;
    // Outside any repository `config --list` still lists the operator's own scopes and succeeds; a
    // failure is a config git cannot parse, which is no more trustworthy than a planted one.
    if !out.status.success() {
        return Err(ConfigUnreadable {
            dir: dir.to_path_buf(),
            git_says: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        }
        .into());
    }
    let layout = check_layout(dir)?;
    let git_dir = layout.as_ref().map(|(git_dir, _)| git_dir.clone());
    let repo = layout.and_then(|(_, common)| common.parent().map(Path::to_path_buf));
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split('\0');
    while let (Some(scope), Some(origin), Some(entry)) =
        (fields.next(), fields.next(), fields.next())
    {
        let file = origin.strip_prefix("file:").unwrap_or(origin);
        if !judged_scope(scope, file, repo.as_deref()) {
            continue;
        }
        let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
        // git writes a submodule's own `core.worktree`, pointing back at its checkout; jkb run inside
        // a submodule reads it as local config. Anywhere else it points checkout at any directory.
        if key == "core.worktree"
            && git_dir
                .as_deref()
                .is_some_and(|g| submodule_worktree(g, value))
        {
            continue;
        }
        refuse_key(dir, key, file, scope)?;
    }
    Ok(())
}

/// Whether a config entry `git config --list --show-scope` reports at `scope`, from `file`, is the
/// repository's to answer for. `local` and `worktree` are — a file the repository's config includes is
/// listed at the scope that included it. `system`, `global`, `command` and none are the operator's.
/// `unknown` is judged only when its file lies in the repository (`repo`, its top level; none when
/// git finds no readable repository, and then it is not judged): Apple's git
/// reads an extra, Xcode-owned layer — `/Library/Developer/CommandLineTools/usr/share/git-core/gitconfig`,
/// setting `credential.helper=osxkeychain` — and lists it as `unknown` (measured on git 2.50.1, Apple
/// Git-155), which refused every repository on a Mac until this.
fn judged_scope(scope: &str, file: &str, repo: Option<&Path>) -> bool {
    match scope {
        "system" | "global" | "command" | "" => false,
        "unknown" => {
            let file = fs::canonicalize(file).unwrap_or_else(|_| PathBuf::from(file));
            // No repository — outside one, or one git cannot read — has no config of its own.
            repo.is_some_and(|repo| file.starts_with(repo))
        }
        _ => true,
    }
}

fn refuse_key(dir: &Path, key: &str, file: &str, scope: &str) -> Result<()> {
    if config_key_allowed(key) {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to run git in {}: its repository config sets `{key}` ({file}, {scope} scope), \
         which is not on jkb's list of repository-shape keys and can make git run a program. That \
         file is writable from the dev container (design D52.11). If you did not put it there, \
         remove it on the host: git config --file {file} --unset-all {key}",
        dir.display()
    )
}

/// Refuse a git directory that takes its config and hooks from somewhere other than its repository,
/// returning it and the common directory it does use (`None` outside any repository).
///
/// A `commondir` file in a git directory redirects git's config, hooks and refs to whatever it names,
/// and `.git/` is writable from the dev container around its read-only `config` and `hooks` binds
/// (measured on git 2.51.1: a planted `.git/commondir` made `rev-parse --git-common-dir` answer the
/// planted directory, whose config and hooks git then used). jkb's own git is judged on the config it
/// actually reads either way; this is for **your** git in the same directory, which runs the hooks
/// there — so jkb stops, and says so, rather than going on as if nothing were wrong.
///
/// A main repository's git directory is its own common directory. A linked worktree's is
/// `<common>/worktrees/<name>`, and a jkb session's common directory is its repository's `.git` —
/// asked of the session worktree itself, not of a submodule checked out inside it, whose git
/// directory is its own.
fn check_layout(dir: &Path) -> Result<Option<(PathBuf, PathBuf)>> {
    let out = git_in(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ],
    )
    .output()
    .with_context(|| format!("locating {}'s git directory", dir.display()))?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let (Some(git_dir), Some(common)) = (lines.next(), lines.next()) else {
        return Ok(None);
    };
    let real = |p: &str| fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
    let (git_dir, common) = (real(git_dir), real(common));
    let redirected = |from: &Path, to: &Path| -> anyhow::Error {
        anyhow::anyhow!(
            "refusing to run git in {}: its git directory {} takes its config and hooks from {} — \
             not its own repository. A `commondir` or `.git` file is writable from the dev container \
             (design D52.11), and git you run there would use that directory's hooks. If you did not \
             put it there, look at {}/commondir and {}/.git on the host.",
            dir.display(),
            from.display(),
            to.display(),
            from.display(),
            dir.display()
        )
    };
    if git_dir != common && git_dir.parent() != Some(common.join("worktrees").as_path()) {
        return Err(redirected(&git_dir, &common));
    }
    if let Some((main, root)) = session_of(&real(&dir.to_string_lossy())) {
        // A repository nested inside the session (a submodule) answers its own top level; the
        // session's own rule is for the session. Anything that cannot say is held to it.
        let top = git_in(
            dir,
            &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        )
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| real(String::from_utf8_lossy(&o.stdout).trim()));
        let nested = top.is_some_and(|t| t != root && t.starts_with(&root));
        let own = real(&main.join(".git").to_string_lossy());
        // Nested, it must be one of the repository's own submodules: git keeps a linked worktree's
        // under `<main>/.git/worktrees/<name>/modules`, a checkout's under `<main>/.git/modules`.
        let submodule = common.starts_with(own.join("modules"))
            || (common.starts_with(own.join("worktrees"))
                && common.components().any(|c| c.as_os_str() == "modules"));
        if if nested { !submodule } else { common != own } {
            return Err(redirected(&git_dir, &common));
        }
    }
    Ok(Some((git_dir, common)))
}

/// Whether `git_dir` is a submodule's git directory — under a repository's `.git/modules/`, or
/// `.git/worktrees/<name>/modules/` — whose `core.worktree` of `value` lands inside that repository's
/// tree and outside its `.git`.
fn submodule_worktree(git_dir: &Path, value: &str) -> bool {
    let Some(dot_git) = git_dir
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == ".git"))
    else {
        return false;
    };
    let Ok(rel) = git_dir.strip_prefix(dot_git) else {
        return false;
    };
    let parts: Vec<_> = rel
        .components()
        .map(std::path::Component::as_os_str)
        .collect();
    let in_modules = parts.first().is_some_and(|p| *p == "modules")
        || (parts.first().is_some_and(|p| *p == "worktrees")
            && parts.get(2).is_some_and(|p| *p == "modules"));
    let Some(top) = dot_git.parent() else {
        return false;
    };
    in_modules && inside(&git_dir.join(value), top, dot_git)
}

/// The repository a jkb session worktree (`<main>/.jkb/work/<name>/…`) belongs to, and the session
/// worktree's own root.
fn session_of(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let parts: Vec<_> = dir.components().collect();
    let at = parts
        .windows(2)
        .position(|w| w[0].as_os_str() == ".jkb" && w[1].as_os_str() == "work")?;
    let name = parts.get(at + 2)?;
    let main: PathBuf = parts[..at].iter().collect();
    let root = main.join(".jkb").join("work").join(name);
    Some((main, root))
}

/// The deepest submodule nesting [`module_findings`] walks.
const MAX_MODULE_DEPTH: usize = 8;

/// What in `dir`'s **submodules** could make git run a program — for the reap scan to report, since
/// jkb's own git never enters a submodule ([`git_cmd`]) but yours does: a submodule git directory's own
/// config setting a key outside [`config_key_allowed`], or a `core.worktree` pointing outside the
/// superproject; a `commondir` in one, or a hook in its own `hooks/` that is neither git's sample nor
/// your template's; a symlinked submodule config, git directory, `modules/`, `hooks/` or `.git`; a `.gitmodules`
/// path outside its own checkout; a submodule checkout whose `.git` file points somewhere other than
/// a `modules/` this scan reads. Real git directories (those with a `HEAD`) under `<common>/modules`,
/// each `<common>/worktrees/*/modules`, and each un-absorbed submodule's own `.git/modules` are read,
/// and so is whatever git directory a submodule's `.git` file names — each once, and only its
/// top-level `config`: a loose ref that happens to be named `config` is not one. Submodules of
/// submodules are followed to [`MAX_MODULE_DEPTH`], and anything deeper is reported rather than
/// passed over.
pub(crate) fn module_findings(dir: &Path) -> Vec<String> {
    findings_with(dir, template_hooks(dir, None))
}

/// [`module_findings`], with your template's `hooks/` given.
fn findings_with(dir: &Path, template: Option<PathBuf>) -> Vec<String> {
    let Ok(Some((_, common))) = check_layout(dir) else {
        return Vec::new();
    };
    let Some(top) = common.parent().map(Path::to_path_buf) else {
        return Vec::new();
    };
    let mut scan = Scan {
        dir,
        top,
        common,
        out: Vec::new(),
        judged: std::collections::HashSet::new(),
        template,
    };
    let mut homes = vec![scan.common.join("modules")];
    if let Ok(entries) = fs::read_dir(scan.common.join("worktrees")) {
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_symlink()) {
                scan.out
                    .push(format!("{} is a symlink", e.path().display()));
            } else {
                homes.push(e.path().join("modules"));
            }
        }
    }
    for home in &homes {
        scan.walk(home);
    }
    // The checkout's own `.gitmodules` — a session's, not the main checkout's — is what git reads
    // there.
    let work_top = git_in(
        dir,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
    )
    .output()
    .ok()
    .filter(|o| o.status.success())
    .map_or_else(
        || scan.top.clone(),
        |o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()),
    );
    scan.links(&work_top, homes);
    scan.out
}

/// How a submodule hook stands against your template's hook of the same name.
enum TemplateMatch {
    /// The same bytes, from a template outside the repositories: yours.
    Yours,
    /// No template hook, or different bytes.
    Not,
    /// The same bytes, from a template hook that really lies in `repos`, which the container writes.
    Beside { real: PathBuf, repos: PathBuf },
}

/// One reap scan of a repository's submodules: what it found, and the git directories it has
/// already judged — each once, however many ways it is reached.
struct Scan<'a> {
    dir: &'a Path,
    top: PathBuf,
    common: PathBuf,
    out: Vec<String>,
    judged: std::collections::HashSet<PathBuf>,
    /// Your `init.templateDir`'s `hooks/` ([`template_hooks`]).
    template: Option<PathBuf>,
}

/// The `hooks/` of the template git copies into every repository and submodule it creates — the
/// operator's, from the environment or the global or system config, never the repository's own,
/// which the container writes.
///
/// Read `--includes`, as git does when it copies the template: a dotfiles `[include]` that sets it
/// otherwise read as no template at all, and every hook it installs as planted (review round 9).
/// `global` names the global config file in place of git's own choice.
fn template_hooks(dir: &Path, global: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GIT_TEMPLATE_DIR") {
        return Some(PathBuf::from(dir).join("hooks"));
    }
    ["--global", "--system"].iter().find_map(|scope| {
        let mut cmd = git_in(
            dir,
            &[
                "config",
                scope,
                "--includes",
                "--type=path",
                "--get",
                "init.templateDir",
            ],
        );
        if let Some(global) = global {
            cmd.env("GIT_CONFIG_GLOBAL", global);
        }
        cmd.output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("hooks"))
    })
}

impl Scan<'_> {
    /// Judge `gitdir` unless it already was; whether it was new.
    fn judge(&mut self, gitdir: &Path) -> bool {
        let key = fs::canonicalize(gitdir).unwrap_or_else(|_| gitdir.to_path_buf());
        if !self.judged.insert(key) {
            return false;
        }
        judge_module(self.dir, gitdir, &self.top, &self.common, &mut self.out);
        if !gitdir.join("commondir").exists() {
            self.judge_hooks(gitdir);
        }
        true
    }

    /// Whether `hook` is byte for byte the template hook `yours` — judged by where `yours` really is,
    /// through every symlink: one inside the tree that holds the repositories is the container's to
    /// write, so the same bytes planted there and in a submodule would pass as yours (review rounds
    /// 9–10: first the whole template was judged, which a hook symlinked into the repositories
    /// stepped around, and a standing finding against every repository hid a real one).
    fn template_match(&self, yours: &Path, hook: &Path) -> TemplateMatch {
        let Ok(real) = fs::canonicalize(yours) else {
            return TemplateMatch::Not;
        };
        if !matches!((fs::read(&real), fs::read(hook)), (Ok(a), Ok(b)) if a == b) {
            return TemplateMatch::Not;
        }
        match self.top.parent() {
            Some(repos) if real.starts_with(repos) => TemplateMatch::Beside {
                real,
                repos: repos.to_path_buf(),
            },
            _ => TemplateMatch::Yours,
        }
    }

    /// Report what could run from `gitdir`'s own `hooks/` for git you run there — measured on git
    /// 2.51.1, a planted `pre-commit` ran on `git -C sub commit` (review round 7). Git writes only
    /// `*.sample` files there, and copies your `init.templateDir`'s hooks in; anything else is
    /// reported, and so is a `hooks/` that cannot be listed — git needs only to search it to run a
    /// hook by name (round 8).
    fn judge_hooks(&mut self, gitdir: &Path) {
        let hooks = gitdir.join("hooks");
        match fs::symlink_metadata(&hooks) {
            Ok(m) if m.file_type().is_symlink() => {
                self.out.push(format!("{} is a symlink", hooks.display()));
                return;
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.out
                    .push(format!("{} cannot be read: {e}", hooks.display()));
                return;
            }
        }
        let entries = match fs::read_dir(&hooks) {
            Ok(entries) => entries,
            Err(e) => {
                self.out
                    .push(format!("{} cannot be listed: {e}", hooks.display()));
                return;
            }
        };
        let template = self.template.clone();
        for e in entries {
            let e = match e {
                Ok(e) => e,
                Err(err) => {
                    self.out
                        .push(format!("{} cannot be listed: {err}", hooks.display()));
                    continue;
                }
            };
            let name = e.file_name();
            if name.to_string_lossy().ends_with(".sample") {
                continue;
            }
            let judged = template.as_ref().map_or(TemplateMatch::Not, |t| {
                self.template_match(&t.join(&name), &e.path())
            });
            match judged {
                TemplateMatch::Yours => {}
                TemplateMatch::Not => self.out.push(format!("{} is a hook", e.path().display())),
                // Say why, or it reads as planted and the fix looks like deleting your own hook
                // (review round 11).
                TemplateMatch::Beside { real, repos } => self.out.push(format!(
                    "{} is a hook — it matches your template's, but {} lies in {}, which the \
                     container writes, so it cannot vouch for it; move your template out",
                    e.path().display(),
                    real.display(),
                    repos.display()
                )),
            }
        }
    }

    /// Judge every git directory under the `modules/` directory `root`, at any nesting to
    /// [`MAX_MODULE_DEPTH`]. A symlink there, `root` included, is reported and never followed.
    fn walk(&mut self, root: &Path) {
        if fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()) {
            self.out.push(format!("{} is a symlink", root.display()));
            return;
        }
        let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
        while let Some((at, depth)) = stack.pop() {
            let Ok(entries) = fs::read_dir(&at) else {
                continue;
            };
            for e in entries.flatten() {
                let path = e.path();
                let Ok(kind) = e.file_type() else {
                    continue;
                };
                if kind.is_symlink() {
                    self.out.push(format!("{} is a symlink", path.display()));
                    continue;
                }
                if !kind.is_dir() {
                    continue;
                }
                if depth >= MAX_MODULE_DEPTH {
                    self.out
                        .push(format!("{} is nested too deep to scan", path.display()));
                    continue;
                }
                if path.join("HEAD").is_file() {
                    self.judge(&path);
                    stack.push((path.join("modules"), depth + 1));
                } else {
                    // A submodule name may hold `/`: its git directory is deeper.
                    stack.push((path, depth + 1));
                }
            }
        }
    }

    /// Each submodule checkout `.gitmodules` names under the checkout `work_top`, and theirs in turn,
    /// each checkout once. A path outside its checkout is reported and not followed — a hostile
    /// `path = .` would otherwise read the same file again at every level. A `.git` that is a
    /// symlink, cannot be read, or is a file pointing outside every `modules/` in `homes` is
    /// reported; one that is a git directory in its own right (not absorbed) is judged, and so is
    /// every git directory under its own `modules/`; one a `.git` file names is judged too, if the
    /// walk did not reach it — git you run in the superproject enters them all.
    fn links(&mut self, work_top: &Path, homes: Vec<PathBuf>) {
        let start = fs::canonicalize(work_top).unwrap_or_else(|_| work_top.to_path_buf());
        let mut seen = std::collections::HashSet::from([start.clone()]);
        // A checkout to read `.gitmodules` in, the `modules/` directories its submodules' git
        // directories may be in, and how deep it is.
        let mut stack = vec![(start, homes, 0)];
        while let Some((checkout, homes, depth)) = stack.pop() {
            for rel in submodule_paths(self.dir, &checkout) {
                let Ok(sub) = fs::canonicalize(checkout.join(&rel)) else {
                    // Not checked out.
                    continue;
                };
                if !sub.starts_with(&checkout) || sub == checkout {
                    self.out.push(format!(
                        "{} names `{rel}`, outside its own checkout",
                        checkout.join(".gitmodules").display()
                    ));
                    continue;
                }
                if !seen.insert(sub.clone()) {
                    continue;
                }
                let mut homes = homes.clone();
                if !self.follow(&sub, &mut homes) {
                    continue;
                }
                if depth + 1 >= MAX_MODULE_DEPTH {
                    self.out
                        .push(format!("{} is nested too deep to scan", sub.display()));
                    continue;
                }
                stack.push((sub, homes, depth + 1));
            }
        }
    }

    /// Judge the submodule checked out at `sub` through its `.git`, adding its own `modules/` to
    /// `homes` when its git directory is here; whether to read its `.gitmodules` in turn.
    fn follow(&mut self, sub: &Path, homes: &mut Vec<PathBuf>) -> bool {
        let link = sub.join(".git");
        let meta = match fs::symlink_metadata(&link) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(e) => {
                self.out
                    .push(format!("{} cannot be read: {e}", link.display()));
                return false;
            }
        };
        if meta.file_type().is_symlink() {
            self.out.push(format!("{} is a symlink", link.display()));
            return false;
        }
        if meta.is_dir() {
            // Not absorbed: the submodule's git directory is here, in the checkout.
            self.judge(&link);
            let modules = link.join("modules");
            self.walk(&modules);
            homes.push(fs::canonicalize(&modules).unwrap_or(modules));
            return true;
        }
        let text = match fs::read_to_string(&link) {
            Ok(text) => text,
            Err(e) => {
                self.out
                    .push(format!("{} cannot be read: {e}", link.display()));
                return false;
            }
        };
        let Some(target) = text.trim().strip_prefix("gitdir:") else {
            self.out
                .push(format!("{} is not a gitdir link", link.display()));
            return false;
        };
        let target = sub.join(target.trim());
        let resolved = fs::canonicalize(&target).unwrap_or(target);
        if !homes.iter().any(|h| resolved.starts_with(h)) {
            self.out.push(format!(
                "{} points at {}, outside this repository's own modules",
                link.display(),
                resolved.display()
            ));
            return false;
        }
        // Inside a home, but maybe not where the walk looks (`modules/<a>/<b>` under a git directory
        // `modules/<a>`): judged here if the walk did not reach it.
        if self.judge(&resolved) {
            self.walk(&resolved.join("modules"));
        }
        true
    }
}

fn judge_module(dir: &Path, gitdir: &Path, top: &Path, common: &Path, out: &mut Vec<String>) {
    // A `commondir` sends git to another directory's config and hooks — measured on git 2.51.1, a
    // clean filter planted there ran on `git -C sub add` — so this directory's own `config` would
    // be the wrong one to judge. [`check_layout`] refuses the same redirect for a repository jkb
    // runs git in; a submodule's git directory never has one (review round 6).
    let redirect = gitdir.join("commondir");
    if fs::symlink_metadata(&redirect).is_ok() {
        let to = fs::read_to_string(&redirect).unwrap_or_default();
        out.push(format!(
            "{} redirects its config and hooks to `{}`",
            redirect.display(),
            to.trim()
        ));
        return;
    }
    let cfg = gitdir.join("config");
    match fs::symlink_metadata(&cfg) {
        Ok(m) if m.file_type().is_symlink() => {
            out.push(format!("{} is a symlink", cfg.display()));
            return;
        }
        Ok(_) => {}
        Err(_) => return,
    }
    let file = cfg.to_string_lossy().into_owned();
    let Ok(o) = git_in(dir, &["config", "--file", &file, "--list", "-z"]).output() else {
        return;
    };
    if !o.status.success() {
        out.push(format!("git cannot read the submodule config {file}"));
        return;
    }
    for entry in String::from_utf8_lossy(&o.stdout).split('\0') {
        let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
        if key.is_empty() {
            continue;
        }
        if key == "core.worktree" {
            if !inside(&gitdir.join(value), top, common) {
                out.push(format!(
                    "{file} points core.worktree outside the superproject"
                ));
            }
        } else if !config_key_allowed(key) {
            out.push(format!("{file} sets `{key}`"));
        }
    }
}

/// Whether `path` is inside the superproject's tree `top` and not inside its git directory — through
/// symlinks where the path exists (a symlink at a submodule's path is the container's to plant), and
/// lexically where it does not yet.
fn inside(path: &Path, top: &Path, common: &Path) -> bool {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| {
        let mut at = PathBuf::new();
        for c in path.components() {
            match c {
                std::path::Component::ParentDir => {
                    at.pop();
                }
                std::path::Component::CurDir => {}
                other => at.push(other),
            }
        }
        at
    });
    resolved.starts_with(top) && resolved != top && !resolved.starts_with(common)
}

/// The paths `checkout`'s `.gitmodules` names, relative to it.
fn submodule_paths(dir: &Path, checkout: &Path) -> Vec<String> {
    let gitmodules = checkout.join(".gitmodules");
    if !gitmodules.is_file() {
        return Vec::new();
    }
    let file = gitmodules.to_string_lossy().into_owned();
    let Ok(o) = git_in(
        dir,
        &[
            "config",
            "--file",
            &file,
            "-z",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
    )
    .output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&o.stdout)
        .split('\0')
        .filter_map(|entry| entry.split_once('\n').map(|(_, rel)| rel.to_owned()))
        .collect()
}

/// Remove the environment variables that select a repository, so the working directory decides
/// which one.
///
/// The rule lives here rather than at each call site, because it has three of them in three
/// modules: this module's git spawns, [`crate::pr`]'s `gh` (which resolves the repository
/// through git exactly as git does, so a leak makes it ask GitHub about somebody else's pull
/// requests — and `close-merged` then closes tasks on that answer), and [`crate::session`]'s
/// gate runner (whose verdict decides a landing). Anything else that shells out to a
/// repository-aware tool belongs here too.
///
/// Everything that names a repository or a PART of one — six variables, not the three this
/// sentence claimed until round 28, when `jkb task work` was measured rewriting a foreign
/// repository's index through an inherited `GIT_INDEX_FILE`. `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS` inject
/// configuration and are deliberately left alone: this project's own dev container carries
/// `safe.directory` grants in them, and stripping those makes git refuse the checkout.
pub(crate) fn scrub_repo_selection(cmd: &mut Command) -> &mut Command {
    for key in REPO_SELECTION_VARS {
        cmd.env_remove(key);
    }
    cmd
}

/// The variables [`scrub_repo_selection`] removes — and the list it ITERATES to remove them, so
/// the rule and the list cannot come to say different things.
///
/// It was three literal `env_remove` calls beside a `#[cfg(test)]` copy of the same three names,
/// which reads as a pinned list and is a mirror: a fourth variable added to the function would
/// have left every test green, since each only ever asked that the named three were gone. The
/// `cfg(test)` gate was what forced the duplication, and it bought nothing — three `&'static str`
/// in the binary is not a cost worth a second copy of a security rule.
pub(crate) const REPO_SELECTION_VARS: &[&str] = &[
    // Which repository.
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    // ...and which PARTS of one. Added in round 28 after the reason for leaving them out was
    // measured false. The comment that kept them out said production "must not discard a
    // component a caller legitimately handed it (git exports `GIT_INDEX_FILE` to hook
    // processes)". Measured on git 2.51.1, dumping `env | grep ^GIT_` from real hooks:
    // `post-merge` — the only hook jkb installs, and the one that runs `close-merged` — is handed
    // NO component selector at all, and `pre-commit` is handed `GIT_INDEX_FILE=.git/index`,
    // RELATIVE, which is meaningless to a `git -C <other dir>` call anyway.
    //
    // The harm is not hypothetical and it is reached through a user-facing command. With
    // `GIT_INDEX_FILE=<victim>/.git/index` exported, `worktree_add`'s `git -C <proj> worktree add`
    // — `jkb task work` — rewrote the victim's index (md5 changed) and left `git status` there
    // failing with `fatal: unable to read <sha>`. That is the same corruption round 27 fixed on
    // the fixture side, reached from the product.
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// The integration crates' fixture isolation, COMPILED INTO THIS CRATE'S TEST BUILD TOO.
///
/// `jkb-cli` is bin-only, so an integration test cannot import anything from here and this file
/// used to carry its own copy of the list — `FIXTURE_CONFIG` — with a test that parsed both out of
/// their source files and compared them. That test was deleted with the copy: it compared two
/// pieces of TEXT rather than two environments (round 24 measured a function drifting from a list
/// both copies agreed on), and its failure message said the lists "disagree", which reads as an
/// instruction to sync them — the precise edit round 25 measured as reopening the scrub hole.
///
/// One source text, three compilations, no parity to check. `tests/common/mod.rs` uses only
/// `std`, so it compiles here unchanged.
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
pub(crate) mod fixture_env;

/// The fixture's no-real-container pin names the SAME variable production reads.
///
/// Both sides were literals compared to nothing: rename [`crate::transcripts::CONTAINER_NAME_VAR`]
/// and `dev_container_name()`'s own test follows the constant, `assert_jkb_isolated` compares the
/// stale literal it set itself against the stale literal it expects, and `check-config.sh` compares
/// only the default VALUE — so every guard stays green while `cargo test` goes back to
/// `docker exec`-ing the sweep into the container the developer is working in. This file is where
/// the two spellings meet, because the fixture module is compiled into this build.
#[cfg(test)]
#[test]
fn the_fixture_pins_the_variable_production_actually_reads() {
    assert_eq!(
        fixture_env::NO_REAL_CONTAINER.0,
        crate::transcripts::CONTAINER_NAME_VAR,
        "the fixture must name the variable the reaper resolves its target from, \
         or `cargo test` archives the developer's own transcripts"
    );
}

/// The oracle for [`scrub_repo_selection`], written down rather than computed.
///
/// A LITERAL on purpose, and never [`REPO_SELECTION_VARS`]. Production iterates that list, so an
/// assertion that also read it would shrink with it: measured in round 25, deleting
/// `"GIT_WORK_TREE"` from the list left 260 tests passing while every `git` and `gh` spawn in the
/// crate inherited an exported one. That is the whole reason a test's expected value is a thing
/// somebody wrote down — two artifacts, one production and one test, never the same source.
///
/// Round 24 had the opposite defect and the fix for it created this one: the applying function
/// restated the names instead of iterating them, so it could quietly scrub MORE than the list
/// said. Both directions are now closed, by iterating on the production side and comparing for
/// EQUALITY here — a name added to the list forces an edit next to this paragraph, which is where
/// the reason lives.
#[cfg(test)]
const EXPECT_SELECTION_REMOVED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// Assert that `cmd` removes EXACTLY the repository selectors, plus `also` where a tool has its
/// own (`gh` names a repository outright through `GH_REPO`).
///
/// Equality, not a superset. A superset check cannot see a list that grew, and "it scrubs at
/// least these" is how a blanket sweep gets in — `config_injection_is_left_alone` names the two
/// variables this deliberately does NOT remove, and that test would have gone on passing beside a
/// removal of them.
#[cfg(test)]
pub(crate) fn assert_scrubbed(what: &str, cmd: &Command, also: &[&str]) {
    let mut removed: Vec<String> = cmd
        .get_envs()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    let mut want: Vec<String> = EXPECT_SELECTION_REMOVED
        .iter()
        .chain(also)
        .map(|s| (*s).to_owned())
        .collect();
    removed.sort();
    want.sort();
    assert_eq!(
        removed, want,
        "{what}: the removed set must be exactly the repository selectors. A missing one outranks \
         the working directory and points the tool at another repository; an extra one is a \
         blanket sweep, which this crate refuses deliberately (see \
         `config_injection_is_left_alone`). Do not reconcile this by editing the list it is \
         checked against — that is the edit measured to reopen the hole."
    );
}

/// Blank every comment and string literal, so a source-scanning test sees code and not text.
///
/// **There is a second Rust scanner in this crate** — `commands::tests::string_literals`, which
/// EXTRACTS literals rather than blanking them, for a check about what the binary prints. They
/// are not one function today, and that is a known cost rather than an oversight: unifying them
/// means rewriting a passing check in a file this change does not otherwise touch. Two scanners
/// that must both be right about Rust's syntax is exactly the drift shape this project warns
/// about, so it is written down here. (Noted while doing this: `string_literals` does NOT handle
/// raw strings, so `r#"…"#` fragments for it — harmless for its purpose, and the reason this
/// one does handle them is that a raw string mentioning `Command::new(` would otherwise be
/// reported as an unscrubbed spawn.)
///
/// Returns `src` with every comment and string literal blanked to spaces, newlines preserved so
/// line numbers survive. Handles line and (nested) block comments, ordinary strings with escapes
/// and line continuations, raw strings (`r"…"`, `r#"…"#`, `br#"…"#`), and char literals — a char
/// literal may hold a quote (`'"'`) while a lifetime never closes, so the latter falls through
/// as ordinary text.
///
/// It exists because a scan of raw LINES cannot see a spawn whose call is split by rustfmt:
///
/// ```ignore
/// let mut c = Command::new(
///     "git",
/// );
/// ```
///
/// Measured — that form evaded the spawn guard below, while a block comment and a closure did
/// not. Matching code rather than text also retires the guard's previous special cases (skip
/// lines starting with `//`, skip lines containing an escaped quote), which existed only to stop
/// the guard's own source from matching itself.
#[cfg(test)]
pub(crate) fn code_only(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<char> = vec![' '; chars.len()];
    let mut at = 0usize;
    while at < chars.len() {
        // A raw string first: its body may contain anything, quotes and `*/` included.
        if let Some(end) = lex::raw_string_end(&chars, at) {
            lex::blank_span(&chars, &mut out, at, end);
            at = end;
            continue;
        }
        let end = match chars[at] {
            '/' if chars.get(at + 1) == Some(&'/') => lex::line_comment_end(&chars, at),
            '/' if chars.get(at + 1) == Some(&'*') => lex::block_comment_end(&chars, at),
            '"' => lex::string_end(&chars, at),
            '\'' => {
                // A quote that opens a LIFETIME, not a literal, is ordinary code.
                let Some(end) = lex::char_literal_end(&chars, at) else {
                    out[at] = chars[at];
                    at += 1;
                    continue;
                };
                end
            }
            ch => {
                out[at] = ch;
                at += 1;
                continue;
            }
        };
        lex::blank_span(&chars, &mut out, at, end);
        at = end;
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod lex {
    //! `code_only`'s scanners, one per literal form. Split out of a single 107-line function
    //! that clippy refused on both length and six single-character bindings at once; each half
    //! is now named after the thing it skips, and the index arithmetic that made the short
    //! names tempting is confined to one scanner apiece.

    /// Blank `src[from..to]` in `out`, keeping newlines so every line number is preserved.
    pub(super) fn blank_span(src: &[char], out: &mut [char], from: usize, to: usize) {
        for k in from..to.min(src.len()) {
            if src[k] == '\n' {
                out[k] = '\n';
            }
        }
    }

    /// If a raw string (`r"…"`, `r#"…"#`, `br#"…"#`) opens at `start`, the index just past its
    /// close — or past the end of input when it never closes, which is what the single-pass
    /// scanner did before and keeps an unterminated literal from being re-scanned as code.
    ///
    /// `starts_token` is why an `r` inside an identifier (`var"` never happens, but `for r#x`
    /// and `let br = …` do) is not read as a literal opener.
    pub(super) fn raw_string_end(src: &[char], start: usize) -> Option<usize> {
        let mut open = start;
        if src[open] == 'b' {
            open += 1;
        }
        if src.get(open) != Some(&'r') {
            return None;
        }
        let mut hashes = 0usize;
        let mut probe = open + 1;
        while src.get(probe) == Some(&'#') {
            hashes += 1;
            probe += 1;
        }
        let starts_token =
            start == 0 || !(src[start - 1].is_alphanumeric() || src[start - 1] == '_');
        if src.get(probe) != Some(&'"') || !starts_token {
            return None;
        }
        let mut pos = probe + 1;
        while pos < src.len() {
            if src[pos] == '"' {
                let mut seen = 0usize;
                let mut after = pos + 1;
                while seen < hashes && src.get(after) == Some(&'#') {
                    seen += 1;
                    after += 1;
                }
                if seen == hashes {
                    return Some(after);
                }
            }
            pos += 1;
        }
        Some(src.len())
    }

    /// The index just past the block comment opening at `start`, honouring nesting.
    pub(super) fn block_comment_end(src: &[char], start: usize) -> usize {
        let mut depth = 1usize;
        let mut pos = start + 2;
        while pos < src.len() && depth > 0 {
            if src[pos] == '/' && src.get(pos + 1) == Some(&'*') {
                depth += 1;
                pos += 2;
            } else if src[pos] == '*' && src.get(pos + 1) == Some(&'/') {
                depth -= 1;
                pos += 2;
            } else {
                pos += 1;
            }
        }
        pos
    }

    /// The index just past the line comment opening at `start` (its newline is not consumed).
    pub(super) fn line_comment_end(src: &[char], start: usize) -> usize {
        let mut pos = start;
        while pos < src.len() && src[pos] != '\n' {
            pos += 1;
        }
        pos
    }

    /// The index just past the ordinary string literal opening at `start`.
    pub(super) fn string_end(src: &[char], start: usize) -> usize {
        let mut pos = start + 1;
        while pos < src.len() {
            if src[pos] == '\\' {
                pos += 2;
                continue;
            }
            if src[pos] == '"' {
                pos += 1;
                break;
            }
            pos += 1;
        }
        pos
    }

    /// The index just past a char literal opening at `start`, or `None` when the quote opens a
    /// LIFETIME instead — `'a` never closes, so it has to fall through as ordinary text while
    /// `'"'` must not.
    pub(super) fn char_literal_end(src: &[char], start: usize) -> Option<usize> {
        let close = start + 2 + usize::from(src.get(start + 1) == Some(&'\\'));
        (src.get(close) == Some(&'\'')).then_some(close + 1)
    }
}

/// Run `git` in `dir`, returning trimmed stdout. `Ok(None)` when git exits non-zero — the
/// common "this ref does not exist" case, which is a fact rather than a failure.
fn git(dir: &Path, args: &[&str]) -> Result<Option<String>> {
    checked(dir, args)?;
    let out = git_cmd(dir, args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

/// Whether the repository containing `dir` has a remote called `name` (one with a URL).
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn has_remote(dir: &Path, name: &str) -> Result<bool> {
    valid_ref(name)?;
    Ok(
        git(dir, &["config", "--get", &format!("remote.{name}.url")])?
            .is_some_and(|url| !url.is_empty()),
    )
}

/// The repository root containing `dir`, or `None` if it is not inside a work tree.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn root(dir: &Path) -> Result<Option<PathBuf>> {
    Ok(git(dir, &["rev-parse", "--show-toplevel"])?
        .filter(|s| !s.is_empty())
        .map(PathBuf::from))
}

/// The **main** working copy's root, even when `dir` is inside a linked worktree.
///
/// [`root`] answers "which checkout am I in", which is the wrong question for anything that
/// belongs to the repository as a whole: session worktrees and archives all live under
/// the main copy's `.jkb/`, or a session that ran `jkb` from inside another session would
/// nest its own `.jkb/work` inside a checkout.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn main_root(dir: &Path) -> Result<Option<PathBuf>> {
    let Some(here) = root(dir)? else {
        return Ok(None);
    };
    // `--path-format=absolute` needs git 2.31; without it the answer would be relative to
    // git's cwd, which is not `here` when `dir` is a subdirectory. Falling back to this
    // checkout is right for the overwhelmingly common case of not being in a worktree.
    let Some(common) = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?
    .filter(|s| !s.is_empty()) else {
        return Ok(Some(here));
    };
    Ok(Some(
        PathBuf::from(common)
            .parent()
            .map_or(here, Path::to_path_buf),
    ))
}

/// The repo's short name — the basename of its root. This is the `repo=` tag value and
/// mirrors the `repos/<repo>` / `tasks/<repo>` namespace key (design D26/D32).
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn key(dir: &Path) -> Result<Option<String>> {
    Ok(root(dir)?.and_then(|r| {
        r.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
    }))
}

/// The currently checked-out branch, or `None` when detached.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn current_branch(dir: &Path) -> Result<Option<String>> {
    Ok(git(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])?.filter(|s| !s.is_empty()))
}

/// The repo's trunk branch: whatever `origin/HEAD` points at, else the first of
/// [`DEFAULT_TRUNKS`] that exists.
///
/// Asking the remote first matters for the "works across a variety of repos" case — a repo
/// whose default is `master` or `develop` must not be silently measured against a `main`
/// that does not exist, because "no such ref" and "nothing merged" would look identical.
///
/// **Whatever comes back resolves here.** `origin/HEAD` is a symbolic ref and can dangle — the
/// remote's default branch renamed, or `origin/main` pruned — and it was the one arm that took its
/// answer on trust while the fallback arm verified. A trunk that does not resolve used to make
/// `ahead_count` quietly answer zero; it now refuses, which turned `jkb staging ls` from a listing
/// into a hard error in exactly that repo. Callers are entitled to assume this ref works.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn trunk(dir: &Path) -> Result<Option<String>> {
    // `origin/HEAD` -> `origin/main`; take the part after the remote name.
    if let Some(sym) = git(
        dir,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )? {
        if let Some((_, branch)) = sym.split_once('/') {
            let reference = format!("origin/{branch}");
            if !branch.is_empty() && exists(dir, &reference)? {
                return Ok(Some(reference));
            }
        }
    }
    for candidate in DEFAULT_TRUNKS {
        for reference in [format!("origin/{candidate}"), (*candidate).to_owned()] {
            if exists(dir, &reference)? {
                return Ok(Some(reference));
            }
        }
    }
    Ok(None)
}

/// Check a value that will be handed to `git` as a **ref operand**, not as a flag.
///
/// `git` parses argv positionally and has no way to know that `-D` was meant as a branch name, so
/// a user-supplied ref beginning with `-` becomes an option: `jkb task work <uid> --onto=-D`
/// reached `git branch -D <trunk>` and **deleted the repository's trunk branch**. (`clap` blocks
/// the separated form `--onto -D` but passes `--onto=-D` through, which is the ordinary way to
/// give an option a hyphenated value.)
///
/// Nothing legitimate is lost: `git check-ref-format` rejects a ref name starting with `-`, so
/// such a branch cannot exist to be referred to. Empty is refused for the same reason.
///
/// Checked here, in the module every git invocation goes through, rather than at the handful of
/// CLI flags that happen to accept a branch today — `--onto`, `--branch`, `--trunk`, `task base`,
/// `task tag add branch=…`, and whatever is added next. A rule spread over entry points is the
/// defect this file has now been taught twice.
///
/// # Errors
/// Returns an error if `name` cannot be passed to git as an operand.
pub fn valid_ref(name: &str) -> Result<()> {
    // The rule and its sentence are `jkb_core::location::ref_problem`'s, shared with the store.
    match jkb_core::location::ref_problem(name) {
        Some(why) => Err(anyhow::Error::msg(why)),
        None => Ok(()),
    }
}

/// The commit `reference` resolves to, if any.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn rev(dir: &Path, reference: &str) -> Result<Option<String>> {
    valid_ref(reference)?;
    Ok(git(dir, &["rev-parse", reference])?.filter(|s| !s.is_empty()))
}

/// Run `git` in `dir` for its exit status, returning `(ok, combined output)`. Used by the
/// mutating half of this module (worktrees, branches, rebase), where the failure text is
/// what the user needs to see and a non-zero exit is not "this ref does not exist".
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
fn git_run(dir: &Path, args: &[&str]) -> Result<(bool, String)> {
    checked(dir, args)?;
    let out = git_cmd(dir, args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text.trim().to_owned()))
}

/// Run `git` in `dir`, turning a non-zero exit into an error carrying git's own message.
fn git_must(dir: &Path, args: &[&str]) -> Result<String> {
    let (ok, text) = git_run(dir, args)?;
    anyhow::ensure!(ok, "git {}: {text}", args.join(" "));
    Ok(text)
}

/// One entry of `git worktree list` — a checkout of this repository.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// The worktree's absolute path.
    pub path: PathBuf,
    /// The branch checked out there, or `None` when detached.
    pub branch: Option<String>,
}

/// Every worktree of the repository containing `dir`, main copy included.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn worktrees(dir: &Path) -> Result<Option<Vec<Worktree>>> {
    let Some(text) = git(dir, &["worktree", "list", "--porcelain"])? else {
        // `None`, NOT an empty list. A repo always has at least its own main worktree, so "none"
        // is never a truthful reading of a non-zero exit — and the empty vec was read as *this
        // path is not registered*, which is the value that selects a destructive remedy in the
        // disposal sweep. Callers that only want a list collapse this deliberately.
        return Ok(None);
    };
    let mut out = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            // A blank line separates records, but the last one may have none — flush on the
            // next header instead of relying on the trailing separator.
            if let Some(prev) = path.take() {
                out.push(Worktree {
                    path: prev,
                    branch: branch.take(),
                });
            }
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.trim_start_matches("refs/heads/").to_owned());
        }
    }
    if let Some(p) = path {
        out.push(Worktree { path: p, branch });
    }
    Ok(Some(out))
}

/// Drop git's registration of the worktree at `path`, whose directory is gone.
///
/// **This path only — never `git worktree prune`.** Prune drops every registration whose directory
/// this process cannot see, and across the host/container bind that is every session opened on the
/// other side: their gitdirs name paths that do not exist here, so one side's prune unregistered the
/// other side's live checkouts (stage-3 review, round 3). `git worktree remove` on a missing directory
/// only drops its registration (measured, git 2.51.1).
///
/// Refused while the directory is there: `worktree remove` would delete a clean one.
///
/// # Errors
/// Returns an error if the directory is present or cannot be examined, or git refuses — including
/// for a path it does not register.
pub fn forget_worktree(dir: &Path, path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => anyhow::bail!(
            "{} is still there, so its registration is kept",
            path.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("examining {}", path.display())),
    }
    let path_s = path.to_string_lossy().into_owned();
    git_must(dir, &["worktree", "remove", "--", &path_s])?;
    Ok(())
}

/// Add a worktree at `path` checked out to `branch`. A local `branch` is used as it is; when
/// there is none, it is created from an existing remote-tracking `origin/<branch>` if there is
/// one — **in preference to `start`** — and from `start` only when neither exists. This is
/// [`ensure_branch`]'s rule (adopt the remote, because a session branch may already carry pushed
/// work), not [`create_branch`]'s: a caller that needs the branch to begin exactly at `start`
/// must not use this.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the worktree.
pub fn worktree_add(dir: &Path, path: &Path, branch: &str, start: &str) -> Result<()> {
    valid_ref(branch)?;
    valid_ref(start)?;
    let path_s = path.to_string_lossy().into_owned();
    // `ensure_branch`, so a branch that exists only on the remote is checked out rather than
    // re-cut from `start`: the `-b` fallback this replaced had the same blind spot as every other
    // bare `has_branch`, and here it would silently start a session over on top of commits that
    // had already been pushed.
    let created = ensure_branch(dir, branch, start)?;
    if let Err(e) = git_must(dir, &["worktree", "add", &path_s, branch]) {
        // Undo the branch this call created: a failed `git worktree add` must not leave behind a
        // branch the user never asked for, cluttering `git branch` and the staging listing (which
        // derives its rows from branches that exist). Only the branch this call cut is removed —
        // it is seconds old, has no checkout and carries nothing, so there is nothing to lose —
        // and one that was already there is left strictly alone.
        if created {
            let _ = git_run(dir, &["branch", "-D", branch])?;
        }
        return Err(e);
    }
    Ok(())
}

/// Remove the worktree at `path`, which also drops its own administrative entry. `force` discards
/// uncommitted changes; without it git refuses a dirty worktree, which is the check the
/// caller wants.
///
/// **No `git worktree prune` after it.** Prune drops every registration whose directory this side
/// cannot see — across the host/container bind, every session opened on the other side (stage-3
/// review, round 4). `worktree remove` needs no prune to unregister its own path (measured, git
/// 2.51.1).
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to remove the worktree.
pub fn worktree_remove(dir: &Path, path: &Path, force: bool) -> Result<()> {
    let path_s = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_s);
    git_must(dir, &args)?;
    Ok(())
}

/// Whether a local branch named `branch` exists — **three-valued**.
///
/// Asked with `for-each-ref`, not `show-ref --verify --quiet`, and the difference is the whole
/// point. `show-ref` exits non-zero both for *no such ref* and for *this repository cannot be
/// read*, and [`git`] collapses every non-zero exit to `Ok(None)` — so a corrupt `packed-refs`
/// was reported as a **deleted branch**, and `jkb task land` printed "removed its branch",
/// which is the one direction that costs something: it stops the operator looking.
///
/// `for-each-ref` exits **zero with empty output** when nothing matches (measured, along with
/// the 128 a broken repository gives), so a non-zero exit means only that git could not answer.
///
/// The match is exact. A `for-each-ref` pattern also matches deeper refs — `refs/heads/foo`
/// matches `refs/heads/foo/bar`, likewise measured — so comparing on emptiness alone would
/// report a branch that does not exist.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn has_branch(dir: &Path, branch: &str) -> Result<Fact> {
    valid_ref(branch)?;
    let want = format!("refs/heads/{branch}");
    Ok(Fact::maybe(
        git(dir, &["for-each-ref", "--format=%(refname)", &want])?
            .map(|s| s.lines().any(|l| l == want)),
    ))
}

/// Every branch that exists here, **counting a remote-tracking copy**, mapped to a ref that
/// actually **resolves** to it — in one `git` call.
///
/// The batched form of the question [`branch_ref`] asks per branch, with the same
/// [`Prefer::Local`] preference — for names that **are** branches. It is not simply `branch_ref`
/// in bulk: its keys are the set of branch names, so a tag or a raw object id is absent here and
/// resolvable there, and that difference is [`branch_name`]'s whole reason for existing.
/// [`has_branch`] is one subprocess per question and each spawn measured ~11ms here; `staging ls`
/// redraws on every database write, so it resolves this once before its loop.
///
/// It returns the **ref**, not mere membership, and that is the load-bearing part. A branch living
/// only under `refs/remotes/origin/` is live — the ordinary state after a pruned local ref — but
/// its bare short name resolves to nothing, so every git question asked with that name fails.
/// `rev-list --count` failing read as **zero commits**, so the listing admitted such a batch and
/// then told its tasks they had nothing to land, while `task work` and `task land` went on acting
/// on it. Handing callers the resolved ref means they cannot ask a question the name cannot answer.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn branch_refs(dir: &Path) -> Result<BTreeMap<String, String>> {
    // `%(refname)` decides local-vs-remote and `%(refname:short)` is what git can be handed back.
    // Classifying on the short form instead would read a local branch literally named
    // `origin/x` as a remote copy of `x`.
    let Some(text) = git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)\t%(refname:short)",
            "refs/heads",
            "refs/remotes/origin",
        ],
    )?
    else {
        return Ok(BTreeMap::new());
    };
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let Some((full, short)) = line.split_once('\t') else {
            continue;
        };
        if let Some(name) = full.strip_prefix("refs/heads/") {
            // The local ref wins, whichever order they arrive in — `Prefer::Local`.
            out.insert(name.to_owned(), short.to_owned());
        } else if let Some(name) = full.strip_prefix("refs/remotes/origin/") {
            if name != "HEAD" {
                out.entry(name.to_owned())
                    .or_insert_with(|| short.to_owned());
            }
        }
    }
    Ok(out)
}

/// What a caller-supplied name turns out to be, when the question is "is this a **branch**".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchName {
    /// It names a branch, under the bare short name [`branch_refs`] keys it by. An
    /// `origin/`-qualified spelling of a branch that exists here comes back canonicalized.
    Is(String),
    /// Nothing here goes by that name — a branch the caller may legitimately be about to create.
    Unknown,
    /// It resolves to a commit, but not through a branch: a tag, a raw object id, `HEAD`.
    NotABranch,
}

/// Which **branch** `name` refers to, as [`branch_refs`] keys them — the one answer to "is this a
/// branch, and what is it called".
///
/// Distinct from [`branch_ref`], which maps a branch name to a revision. This maps an arbitrary
/// user-supplied string to a *key*, and that is the direction every consumer of a stored branch
/// name needs: `jkb staging ls` indexes batches by bare short name, so a land target stored as
/// `origin/integration` — which `rev-parse` resolves perfectly well — matched no row and silently
/// dropped its task out of the one listing behind the branch picker and In Flight. A tag was
/// accepted the same way and vanished the task with nothing created and nothing reported.
///
/// The exact key wins over the `origin/`-stripped form, so a local branch genuinely named
/// `origin/x` is itself rather than a remote copy of `x`.
///
/// # Errors
/// Returns an error if `name` cannot be handed to git, or if `git` cannot be executed.
pub fn branch_name(dir: &Path, name: &str) -> Result<BranchName> {
    valid_ref(name)?;
    let refs = branch_refs(dir)?;
    if refs.contains_key(name) {
        return Ok(BranchName::Is(name.to_owned()));
    }
    if let Some(short) = name.strip_prefix("origin/") {
        if refs.contains_key(short) {
            return Ok(BranchName::Is(short.to_owned()));
        }
    }
    // It is not a branch. Whether it is *something* decides which of the two answers this is: a
    // name nothing here uses is a branch waiting to be cut, a name that resolves is a real object
    // the caller has mistaken for a branch, and only the second is worth refusing loudly.
    Ok(if exists(dir, name)? {
        BranchName::NotABranch
    } else {
        BranchName::Unknown
    })
}

/// Create branch `branch` at `start` if it does not already exist. Returns whether it was
/// created.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the branch — naming the race
/// when the branch was proven absent and then appeared while this was creating it.
pub fn create_branch(dir: &Path, branch: &str, start: &str) -> Result<bool> {
    create_branch_between(dir, branch, start, || {})
}

/// [`create_branch`], running `between` after the existence check and before `git branch` — the
/// window a concurrent creator lands in, held open by the test that pins the race's wording.
fn create_branch_between(
    dir: &Path,
    branch: &str,
    start: &str,
    between: impl FnOnce(),
) -> Result<bool> {
    valid_ref(branch)?;
    valid_ref(start)?;
    // Proven present, or attempt it: `git branch` refuses an existing branch on its own, so an
    // unestablished answer costs a clear git error rather than a silent wrong turn.
    let before = has_branch(dir, branch)?;
    if before.is_yes() {
        return Ok(false);
    }
    between();
    let args = ["branch", branch, start];
    let (ok, text) = git_run(dir, &args)?;
    if ok {
        return Ok(true);
    }
    // A RACE, said as one. Two `jkb task work` on one task both see no branch and both run `git
    // branch`; the loser used to print git's `cannot lock ref … reference already exists`. Decided
    // by asking git again, never by parsing its English, and only when the first answer was a
    // proven `No`: after an `Unknown` the branch may have been there all along, which is no race.
    // Still a refusal rather than `Ok(false)`: the caller is mid-way through a command another
    // process is also running, and what it would build on the other's branch is the other's call.
    anyhow::ensure!(
        !(before.is_no() && has_branch(dir, branch)?.is_yes()),
        "branch `{branch}` was created by another process while this one was creating it — most \
         likely a concurrent `jkb task work` on the same task. Nothing was changed here; let the \
         other command finish, then run this one again."
    );
    anyhow::bail!("git {}: {text}", args.join(" "))
}

/// Delete branch `branch` **only if it still points at `expected`** — git's own compare-and-delete
/// (`update-ref -d <ref> <old>`), so a commit that lands on the branch after the caller read it is
/// never discarded with it. `git branch -D` takes no old value: a caller that read the tip,
/// compared it, then deleted, lost whatever arrived in between — which is why there is no
/// `delete_branch` beside this one any more, and why a disposal deletes only through
/// `archive::delete_branch_planned`.
///
/// `Ok(true)` when git deleted it, `Ok(false)` when git refused — the ref moved, is gone, or is
/// locked. Which of those is the caller's to establish by asking again, never by parsing git's
/// English.
///
/// **Not `git branch -D`'s checks.** `update-ref` is plumbing: it deletes a branch another
/// worktree has checked out (measured on git 2.51.1: that worktree then reads `No commits yet`),
/// which `branch -D` refuses. A caller that may meet one must ask [`worktrees`] first.
///
/// # Errors
/// Returns an error if `branch` or `expected` cannot be passed to git (`expected` must be a full
/// object id: anything else would be resolved as a revision), or `git` cannot be executed.
pub fn delete_branch_at(dir: &Path, branch: &str, expected: &str) -> Result<bool> {
    valid_ref(branch)?;
    anyhow::ensure!(
        matches!(expected.len(), 40 | 64) && expected.bytes().all(|b| b.is_ascii_hexdigit()),
        "`{expected}` is not a full object id, so it cannot say which commit `{branch}` must be \
         at to be deleted"
    );
    let full = format!("refs/heads/{branch}");
    // `--no-deref`: delete the ref itself, as `branch -D` does, never a symbolic ref's target.
    let (ok, _) = git_run(dir, &["update-ref", "--no-deref", "-d", &full, expected])?;
    Ok(ok)
}

/// Whether the working tree at `dir` has uncommitted changes (staged, unstaged, or
/// untracked). Untracked files count: a session's new module is untracked until it is
/// added, and landing without it would land a branch that does not build.
///
/// **Three-valued, and that is the whole point.** This returned `Result<bool>` and mapped a
/// `git status` that exited non-zero — an unlinked `.git`, a corrupt index, a worktree whose
/// administrative directory has been pruned — to `false`, i.e. *clean*. Every one of the eight
/// callers is a guard standing in front of something destructive (`reset --hard`, an archiving
/// rename, `git switch` across branches, the graft itself), so "git could not answer" was read
/// as "safe to proceed" at all of them.
///
/// That is the defect [`Fact`] exists to prevent, named in its own module docs — *is that
/// checkout clean* is the example given — and `CLAUDE.md` already states the rule this now
/// satisfies: **landing needs `work_dirty.is_no()`; an unreadable checkout refuses.** The
/// machine's [`jkb_core::lifecycle::TaskFacts::work_dirty`] was three-valued all along and its
/// guard asks `is_no()` correctly; the CLI simply never had an `Unknown` to give it, because the
/// collapse happened here, at the boundary, before the type could carry it.
///
/// So callers must state their polarity: a guard says `is_no()` (refuse when unreadable), a
/// report says `is_yes()` or renders all three.
///
/// `dir` must be a **worktree root** — every caller here passes a session checkout, `.jkb/base`
/// or a repo root. Handed a subdirectory this answers [`Fact::Unknown`] rather than reporting on
/// the enclosing repository, which is [`worktree_identity`]'s whole subject: a question asked
/// about one tree must not be answered by another.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all. A `git` that ran and failed is
/// [`Fact::Unknown`], not an error and emphatically not `No`.
pub fn is_dirty(dir: &Path, anchor: &Path) -> Result<Fact> {
    match worktree_identity(dir, anchor)? {
        // Nothing there IS an answer: an absent directory holds no uncommitted work. Spelling it
        // `Unknown` refused, for ever, a landing that used to complete — git keeps listing a
        // worktree whose directory was removed (`prunable`), so `session::discover` still returns
        // it, `git -C <gone>` exits 128, and `land_blocker` then blocked on a checkout that the
        // disposal step three functions later handles as `Disposal::AlreadyGone`. The remedy that
        // refusal printed could not even be run.
        WorktreeIdentity::Absent => Ok(Fact::No),
        // Git answered, but about somewhere else — or would not answer at all. Either way nothing
        // it said is about this tree, so the two collapse HERE, deliberately: dirtiness has one
        // unestablished answer and no remedy of its own to keep them apart for.
        WorktreeIdentity::Foreign | WorktreeIdentity::Unestablished => Ok(Fact::Unknown),
        WorktreeIdentity::Own => Ok(Fact::maybe(
            git(dir, &["status", "--porcelain"])?.map(|s| !s.is_empty()),
        )),
    }
}

/// Whether a per-worktree git question asked in `dir` is answered by `dir` itself.
///
/// **Git's discovery walks up**, and that quietly invalidates every question this module asks of
/// a session worktree. With `<repo>/.jkb/work/x/.git` gone, `git -C <repo>/.jkb/work/x status`
/// does not fail — it finds `<repo>/.git` and exits 0, reporting the MAIN checkout's status
/// (empty, since `.jkb/` is in `.git/info/exclude`). So the very state this three-valued rewrite
/// was written for — a worktree unlinked part-way — produced a confident `No` about a different
/// tree, and `dispose` recorded the main repo's HEAD as that session's identity, which the sweep
/// then reads as a different session reusing the name and holds for ever.
///
/// Asked here, once, so no call site has to remember that `-C` is not a fence.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn worktree_identity(dir: &Path, anchor: &Path) -> Result<WorktreeIdentity> {
    // Absence is established without asking git, and must be: git answers for the enclosing repo
    // when handed a path that is not there, which would make a removed worktree read as `Own`.
    //
    // Asked through `presence::present_under` rather than `Path::exists()`, which answers `false`
    // for ANY stat error — an untraversable parent (EACCES), ELOOP, ENAMETOOLONG — so an
    // unreadable path became `Absent`, and `Absent` is `Fact::No`, which is "proven clean": the
    // spelling this module exists to forbid, feeding `land`'s post-gate check,
    // `abandon_session`'s ensure, `land_dir_for`'s pre-`switch` check and the sweep's identity
    // guard.
    //
    // THE ANCHOR IS THE REPO ROOT, and it is the caller's to supply because only the caller knows
    // it. Anchoring on the path's own parent — which is what the owner-id probe does, having
    // nothing better — is wrong here and was a must-fix: `.jkb/` is in `.git/info/exclude`, so
    // `git clean -xdf` removes `.jkb/work` and every checkout under it while git goes on listing
    // them as prunable. Parent gone plus worktree gone reads as `Unknown`, `is_dirty` says
    // `Unknown`, and `land_blocker` then refuses for ever, printing a `git -C <worktree> status`
    // remedy about a directory that is not there. The repo root is the anchor no ordinary
    // operation removes.
    // `.fact()` — a deliberate collapse: this function reports an identity, never a remedy, so
    // the two ways of failing to establish presence are one answer to it.
    match crate::presence::present_under(dir, anchor).fact() {
        Fact::No => return Ok(WorktreeIdentity::Absent),
        // Not established that it is there, and not established that it is gone. Nothing this
        // function could ask git afterwards would be about `dir` either.
        Fact::Unknown => return Ok(WorktreeIdentity::Unestablished),
        Fact::Yes => {}
    }
    // `Unestablished`, NOT `Foreign`. `git()` maps every non-zero exit to `Ok(None)`, so this arm
    // is "git would not answer here" — `fatal: detected dubious ownership` on a uid-mismatched
    // bind, a corrupt object store, a `safe.directory` policy — and calling that a proven wreck is
    // what let the sweep rename a checkout it had established nothing about.
    let Some(top) = git(dir, &["rev-parse", "--show-toplevel"])?.filter(|s| !s.is_empty()) else {
        return Ok(WorktreeIdentity::Unestablished);
    };
    // Both sides resolved: `/tmp` and `/private/tmp` are the same directory on macOS, and
    // comparing the strings would call every worktree under one of them foreign.
    let (Ok(a), Ok(b)) = (fs::canonicalize(dir), fs::canonicalize(&top)) else {
        return Ok(WorktreeIdentity::Unestablished);
    };
    Ok(if a == b {
        WorktreeIdentity::Own
    } else {
        WorktreeIdentity::Foreign
    })
}

/// Whether a directory answers git's questions about **itself** — see [`worktree_identity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeIdentity {
    /// The directory is not there. An established observation, not an unobtainable answer.
    Absent,
    /// It is there and `rev-parse --show-toplevel` is the directory itself.
    Own,
    /// It is there, git ANSWERED, and the answer names an enclosing repository — the wreck: a
    /// part-way `git worktree remove` that unlinked the `.git` file and stopped.
    ///
    /// **Established**, and that is the whole reason it is separate from [`Self::Unestablished`].
    /// It used to cover "or it could not say" as well, and the disposal sweep keys its most
    /// destructive outcome on it: a repo git declines to speak for at all — `fatal: detected
    /// dubious ownership`, which a uid-mismatched container bind produces routinely — read as a
    /// proven wreck, and every deferred checkout under it was renamed away with the dirty check
    /// waived. Same defect as `worktrees`, `deletions_only` and `present_under`, in the last
    /// probe of the family that still had it.
    Foreign,
    /// It is there and nothing about what it is was established: `rev-parse --show-toplevel` did
    /// not answer, `canonicalize` failed, or its presence itself is unproven.
    Unestablished,
}

/// The commit `dir`'s **own** HEAD is on, or `None` if `dir` cannot answer for itself.
///
/// [`rev`] with `"HEAD"` is the wrong tool for a session worktree: git's discovery walks up, so a
/// worktree whose `.git` file has been removed cheerfully returns the enclosing repository's
/// HEAD. A session's identity is exactly what must not be borrowed from its parent.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn worktree_head(dir: &Path, anchor: &Path) -> Result<Option<String>> {
    if worktree_identity(dir, anchor)? == WorktreeIdentity::Own {
        rev(dir, "HEAD")
    } else {
        Ok(None)
    }
}

/// What the dirt in `dir`'s working tree is made of — see [`Deletions`] for the three answers.
///
/// [`Deletions::Only`] means: `n` tracked files are missing, nothing is staged, nothing is
/// modified and nothing is untracked — a tree somebody (or something) part-way removed,
/// recoverable in full with one `git restore .`.
///
/// This exists because of a real incident: a `git worktree remove` refused part-way through left
/// 152 deletions, and the next landing refused with "it has uncommitted changes — commit them in
/// the session first", which would have committed 62,421 deleted lines. The two states need
/// opposite advice, so they must not read the same. `jkb task land` no longer creates that state
/// — disposal is an atomic rename now — but nothing stops a stale binary, an interrupted `rm`, or
/// a hand-run `git worktree remove` from doing so.
///
/// **`anchor` is the repo root, and the identity fence is here** — the same shape as [`is_dirty`]
/// and [`worktree_head`], for the same reason their docs give: `git -C <dir>` is not a fence, so
/// for a tree whose `.git` file has been unlinked git's discovery walks up and these four
/// questions are answered *by the enclosing checkout*. That is not a hypothetical: the test below
/// asserted `Unknown` for a directory inside the fixture repo and got `Only(1)` from the repo
/// around it. It was the only probe in this family that made "do not ask this of a tree that
/// cannot answer for itself" a rule every call site had to remember, and both call sites were
/// safe only by each independently gating on its own `is_dirty(..).is_yes()`.
///
/// # Errors
/// Returns an error if `git` cannot be executed at all.
pub fn deletions_only(dir: &Path, anchor: &Path) -> Result<Deletions> {
    // Nothing this function could ask afterwards would be about `dir`.
    if worktree_identity(dir, anchor)? != WorktreeIdentity::Own {
        return Ok(Deletions::Unknown);
    }
    // Asked as four questions with no whitespace in the answers, rather than by parsing
    // `status --porcelain` — whose leading status column is exactly what a trimmed capture eats,
    // so the first entry of every listing would read as the wrong code.
    let lines = |args: &[&str]| -> Result<Option<usize>> {
        // A `git` that exits non-zero here means the question was not answered, and the caller
        // turns that into `Deletions::Unknown` — never into a negative answer.
        Ok(git(dir, args)?.map(|s| s.lines().filter(|l| !l.trim().is_empty()).count()))
    };
    let (Some(staged), Some(untracked), Some(unstaged), Some(deleted)) = (
        lines(&["diff", "--cached", "--name-only"])?,
        lines(&["ls-files", "--others", "--exclude-standard"])?,
        lines(&["diff", "--name-only"])?,
        lines(&["diff", "--name-only", "--diff-filter=D"])?,
    ) else {
        // `Unknown`, and it used to be the same `None` as "this is real work" — defended by a
        // comment claiming `None` meant *no advice*, when at the one call site that read it
        // `None` selected the advice "commit them": exactly what must never be said about a
        // part-way removal. An unanswered question is not a negative answer.
        return Ok(Deletions::Unknown);
    };
    // A staged change of any kind is deliberate — staging a deletion included — and an untracked
    // file is work `git restore .` would not bring back and must not be advised over.
    if staged > 0 || untracked > 0 || unstaged != deleted || deleted == 0 {
        return Ok(Deletions::NotOnly);
    }
    Ok(Deletions::Only(deleted))
}

/// What the dirt in a worktree is made of — asked only of a tree already known to be dirty.
///
/// Three values because the two ways of not being a pure deletion set want opposite advice, and
/// the previous `Option<usize>` gave them one: a probe git could not answer read as "real work",
/// so a part-way removal was met with *commit them* — the 62,421-deleted-lines advice this
/// question exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deletions {
    /// Every change is a deletion of a tracked file, and there are this many. A part-way removal,
    /// recoverable in full with `git restore .`.
    Only(usize),
    /// Established to be something else: staged work, untracked files, edits alongside — or
    /// nothing at all. A clean tree lands here rather than in `Only(0)`, which would read as a
    /// part-way removal of no files and license `git restore .` advice about nothing; the
    /// function is contracted to be asked only of a dirty tree, and this is what it says if it
    /// is not.
    NotOnly,
    /// `git` did not answer, so nothing is established about what the dirt is.
    Unknown,
}

impl Deletions {
    /// What must be added to a bare *"it has uncommitted changes"* for that sentence to be true —
    /// or `None` where it already is.
    ///
    /// **One wording, because there are two readers and they drifted.** `jkb task land` and the
    /// disposal sweep explain the same observation to the same person, and the sweep was taught
    /// this round to caveat an unanswered probe while the land path went on wording it exactly
    /// like ordinary work — at the site of the 152-deletion incident, telling an operator to
    /// commit what may be a part-way removal. Three-valued `Deletions` exists so those two cannot
    /// read the same; a renderer per caller is how they came to.
    ///
    /// Deliberately carries no remedy and no path: its readers offer different ones (`land` and
    /// `abandon` suggest `git restore .` inline, through [`Deletions::refusal_suffix`]; the sweep
    /// has a [`crate::archive::Remedy`] of its own), and folding a remedy in here would have one
    /// of them print it twice.
    #[must_use]
    pub fn caveat(self) -> Option<String> {
        match self {
            Self::Only(n) => Some(format!(
                "those {n} change(s) are deletions of tracked files and nothing else — a \
                 part-way removal, not work"
            )),
            Self::NotOnly => None,
            Self::Unknown => Some(
                "git could not say what they are made of, so whether this is a part-way removal \
                 is unestablished"
                    .to_owned(),
            ),
        }
    }

    /// [`Deletions::caveat`] as the sentence a command line appends to its own *"it has
    /// uncommitted changes"* refusal for the session checkout at `worktree` — with a leading
    /// space, or empty where there is nothing to add — and, for a part-way removal, the
    /// `git restore .` that puts it back.
    ///
    /// **One rendering for the two refusals that print it inline**, `jkb task land` and `jkb task
    /// abandon`. Abandon is the verb the sweep's own [`crate::archive::Remedy::CommitOrForce`]
    /// sends the operator to, and it went on saying "commit them" with no caveat over exactly the
    /// part-way removal the caveat exists for, because the wording lived inside `land_preflight`.
    /// The sweep keeps its structured [`crate::archive::Remedy`] instead of this sentence.
    #[must_use]
    pub fn refusal_suffix(self, worktree: &Path) -> String {
        let Some(c) = self.caveat() else {
            return String::new();
        };
        // By characters, not by bytes. `c[..1]` panics on any caveat whose first character is
        // multi-byte, and nothing in `caveat` warns a future editor that a caller slices it.
        let mut ch = c.chars();
        let capped = ch.next().map_or_else(String::new, |f| {
            f.to_uppercase().collect::<String>() + ch.as_str()
        });
        // The remedy belongs only to the arm where putting the files back is the answer, which is
        // why `caveat` does not carry one.
        match self {
            Self::Only(_) => format!(
                " {capped}. `git -C {} restore .` puts them all back.",
                worktree.display()
            ),
            Self::NotOnly | Self::Unknown => format!(" {capped}."),
        }
    }
}

/// How many commits `branch` has that `onto` does not. Both must be refs this repository can
/// resolve — see [`branch_refs`].
///
/// An unresolvable operand is an **error**, not zero. It used to be zero, and zero is a load-
/// bearing answer here: `land_blocker` reads it as "nothing to land" and the listing prints it as
/// the row's commit count. So a remote-only batch, whose bare name resolves to nothing, was
/// reported as having no commits and refused a landing the command then performed. A count that
/// could not be taken must not be indistinguishable from a count of none.
///
/// # Errors
/// Returns an error if `git` cannot be executed, if either revision does not resolve here, or if
/// the count cannot be read.
pub fn ahead_count(dir: &Path, onto: &str, branch: &str) -> Result<usize> {
    valid_ref(onto)?;
    valid_ref(branch)?;
    let range = format!("{onto}..{branch}");
    let count = git(dir, &["rev-list", "--count", &range])?.with_context(|| {
        format!(
            "`git rev-list --count {range}` failed in {} — usually because one of those revisions \
             does not resolve here. A branch that exists only on the remote has to be named by \
             its `origin/` ref.",
            dir.display()
        )
    })?;
    count
        .parse()
        .with_context(|| format!("`git rev-list --count {range}` printed `{count}`"))
}

/// Check `branch` out in the working tree at `dir`.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses the switch (a dirty tree, or the
/// branch being checked out in another worktree).
pub fn switch_to(dir: &Path, branch: &str) -> Result<()> {
    valid_ref(branch)?;
    git_must(dir, &["switch", branch])?;
    Ok(())
}

/// The outcome of [`graft`]: what the rebase produced. **Neither arm has moved `onto`**, and
/// neither has changed what any other checkout is on: the graft happens in jkb's own graft
/// worktree, which is never on a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Graft {
    /// The branch rebased cleanly onto `onto`'s tip. The graft worktree is left **detached at
    /// `grafted`** — byte-identical to what `onto` becomes if [`advance`] is called — so the gate
    /// runs there, on the integrated result, while `onto` still points at its pre-graft tip.
    Rebased { grafted: String },
    /// The rebase hit a conflict and was aborted; nothing outside the graft worktree changed. The
    /// branch's author must rebase it.
    Conflict,
}

/// The outcome of [`advance`]: whether `onto` was put on a gated graft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Advance {
    /// `onto` now points at the graft.
    Advanced,
    /// The graft passed its gate but `onto` was NOT moved onto it: `onto` moved while the gate
    /// ran, its checkout is no longer on it, or git refused the fast-forward. **Not the branch's
    /// fault**, and `onto` is wherever it was — a concurrent move is refused, never clobbered.
    /// Carries the reason, git's own text where git gave one.
    ///
    /// No rollback on this arm, for the reason `scripts/merge-queue.sh` deleted both of its own:
    /// nothing has moved, so there is nothing to restore and a reset can only destroy.
    Refused { why: String },
}

/// Add a worktree at `path`, **detached** at `start` — on no branch, so it claims no ref.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the worktree.
pub fn worktree_add_detached(dir: &Path, path: &Path, start: &str) -> Result<()> {
    valid_ref(start)?;
    let path_s = path.to_string_lossy().into_owned();
    git_must(dir, &["worktree", "add", "--detach", &path_s, start])?;
    Ok(())
}

/// `path`, compared the way `git worktree list` spells a directory: physically. A path that does
/// not exist (a registration whose directory is gone) is compared through its parent.
fn physical(path: &Path) -> PathBuf {
    if let Ok(p) = fs::canonicalize(path) {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => {
            fs::canonicalize(parent).map_or_else(|_| path.to_path_buf(), |p| p.join(name))
        }
        _ => path.to_path_buf(),
    }
}

/// Make `path` — jkb's graft worktree, `<repo>/.jkb/graft` — a registered checkout **detached at
/// `onto`'s tip**, whatever an earlier landing left in it.
///
/// THE RESET IS UNCONDITIONAL. Nothing but a landing ever works in this tree, and nothing reads
/// it, so what a landing interrupted mid-rebase or mid-gate left there (a rebase in progress, a
/// detached ungated graft, tracked files the gate modified, untracked files it wrote) is harmless
/// and is simply discarded: `rebase --abort` (its failure, when no rebase is in progress, is the
/// ordinary case), `reset --hard`, `clean -fd`, then `checkout --detach`. Ignored files are kept,
/// so a gate's build directory stays warm from one landing to the next.
///
/// A path that exists but git does not register is **refused**, never reused or removed: it is
/// not known to be jkb's, the same rule [`forget_worktree`] keeps and `.jkb/base` follows. A
/// registration whose directory is gone is dropped (that path only — never a prune) and the
/// worktree added again.
///
/// # Errors
/// Returns an error if `git` cannot be executed or cannot list the worktrees, if `path` exists but
/// is not a registered worktree, or if git refuses any step.
pub fn prepare_graft_tree(root: &Path, path: &Path, onto: &str) -> Result<()> {
    valid_ref(onto)?;
    let onto_ref = format!("refs/heads/{onto}");
    let listed = worktrees(root)?.with_context(|| {
        format!(
            "git could not list the worktrees of {}, so {} cannot be established as jkb's graft \
             checkout. `git -C {} worktree list` will say what it makes of the repository.",
            root.display(),
            path.display(),
            root.display()
        )
    })?;
    let present = match fs::symlink_metadata(path) {
        // A SYMLINK IS REFUSED BEFORE ANY GIT COMMAND RUNS THROUGH IT. Canonicalized, a link to the
        // main checkout or to a session equals that registered worktree, and the reset and clean
        // below would then run in the user's own tree — deleting untracked files git has no copy
        // of. Only a real directory at this exact path is known to be jkb's.
        Ok(m) if m.file_type().is_symlink() => anyhow::bail!(
            "{} is a symlink, so it is not used as jkb's graft checkout — look at it, then move it \
             out of the way and land again",
            path.display()
        ),
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("examining {}", path.display())),
    };
    let here = physical(path);
    let registered = listed.iter().any(|w| physical(&w.path) == here);
    match (registered, present) {
        (false, true) => anyhow::bail!(
            "{} exists but git does not know it as a worktree, so it is not reused as jkb's graft \
             checkout — look at it, then move it out of the way and land again (not `git worktree \
             prune`, which drops every checkout this side cannot see)",
            path.display()
        ),
        (true, true) => {
            let _ = git_run(path, &["rebase", "--abort"])?;
            git_must(path, &["reset", "--hard"])?;
            // Untracked files a gate left would survive the reset, and a later branch adding one of
            // those paths would then fail `checkout --detach` on every landing — and the gate would
            // not see exactly what `onto` becomes. `-fd` only: not `-x`, so ignored build output
            // stays warm; not `-ff`, so a nested repository is not descended into.
            git_must(path, &["clean", "-fd"])?;
            git_must(path, &["checkout", "--detach", &onto_ref])?;
        }
        (registered, false) => {
            if registered {
                forget_worktree(root, path)?;
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            worktree_add_detached(root, path, &onto_ref)?;
        }
    }
    Ok(())
}

/// Rebase `branch` onto the tip of `onto` in `graft_dir` — jkb's graft worktree, made ready by
/// [`prepare_graft_tree`] — leaving that tree **detached at the result**, WITHOUT moving `onto`
/// and without changing what any other checkout is on. Returns the tip of `onto` the graft was
/// rebased onto alongside the outcome: [`advance`] compares `onto` against it, so a concurrent
/// move during the gate is refused rather than overwritten.
///
/// THE GATE RUNS BETWEEN THIS AND [`advance`], the order `scripts/merge-queue.sh` settled on. This
/// used to fast-forward `onto` here, before `jkb task land` ran its gate, and rewind it with
/// `reset --hard` on red: for the whole gate `onto` pointed at commits that had passed nothing, an
/// implementer cutting from it carried them away, and a gate that was interrupted (or a rollback
/// that failed) left `onto` there with no record that it had moved. Now `onto` only ever advances
/// to a commit that has already passed, and there is no rollback to fail.
///
/// The rebase runs on a **detached HEAD** at `branch` rather than via `git rebase <onto>
/// <branch>`, because that form checks `branch` out first and git refuses while the session
/// worktree holds it (design D36.4). Detaching does not claim the ref — which also means it
/// does not *move* it: `branch` still points at its pre-rebase commits when this returns.
///
/// It is rebased onto the commit `onto` named when this read it, not onto the name, so the
/// returned tip is exactly the base of the graft: a move of `onto` between the read and the
/// rebase is then the concurrent move [`advance`] refuses, never a base the gate did not see.
///
/// # Errors
/// Returns an error if `git` cannot be executed, if `onto` has no commit, or if `branch` cannot
/// be checked out (detached) in `graft_dir` — carrying git's own message, since that is a fault
/// of the working tree and never a [`Graft::Conflict`].
pub fn graft(graft_dir: &Path, branch: &str, onto: &str) -> Result<(Graft, String)> {
    valid_ref(branch)?;
    valid_ref(onto)?;
    // BOTH NAMES FULLY QUALIFIED for every question asked with them below. `branch` and `onto` are
    // branches, and a bare name is a DWIM lookup that ranks `refs/tags/<name>` above
    // `refs/heads/<name>` (measured on git 2.51.1). A tag named like the work branch was what the
    // count below measured — a tag at `onto` refused two real commits as "nothing to land", a tag
    // ahead of it waved an empty branch through — and a tag named like `onto` was what the branch
    // was rebased onto, commits `onto` never had. (`checkout --detach <name>` happens to prefer the
    // branch, also measured, but is qualified too rather than relied on.) `switch` above takes
    // only branches, so it is the one place the bare name is exact.
    let branch_ref = format!("refs/heads/{branch}");
    let onto_ref = format!("refs/heads/{onto}");
    let pre = rev_commit(graft_dir, &onto_ref)?
        .with_context(|| format!("{onto} has no commit to graft onto"))?;

    // NOTHING TO GRAFT IS NOT A LANDING. A branch sitting at `onto`'s tip rebases to a no-op and
    // `merge --ff-only` then answers "Already up to date." with exit 0 — so this returned
    // `Graft::Landed` and the caller marked every task recording that branch done, dependents
    // unblocked, with not one commit added to `onto`. Reachable whenever a worker reports success
    // without committing: the branch exists, so every check that asks whether it exists passes.
    //
    // Measured before the rebase, which is what separates this from the legitimate case. A branch
    // that HAD commits and whose rebase drops them all as empty — because an earlier landing
    // carried the same content — is a real landing of that content and still returns `Landed`.
    //
    // THIS COUNTS COMMITS, NOT CONTENT, and the two are not the same question. Measured on git
    // 2.51.1: `git rebase` drops a commit that BECOMES empty but keeps one that STARTED empty, so
    // a branch carrying a single `git commit --allow-empty` passes this check and advances `onto`
    // by a commit that changes nothing. `scripts/merge-queue.sh` asks the content
    // question at ENTRY, against the merge-base, and EJECTS a branch that diverged and
    // contributes nothing rather than reporting it as a landing — an earlier shape of that check
    // did report it as one, and this comment described that shape for a commit after it was
    // replaced. The queue also STALLS a branch already an ancestor of the base, because the graph
    // cannot say whether an earlier entry landed its work or it was never committed to. The
    // twins disagree here, deliberately and for now — the shell queue closes whole groups
    // unattended, which is where the harm was, and the same fix here needs a `Graft` variant and
    // a `do_land` arm deciding whether a task with no content should be marked done at all. Filed
    // rather than guessed at.
    if ahead_count(graft_dir, &pre, &branch_ref)? == 0 {
        anyhow::bail!(
            "{branch} has no commits ahead of {onto} — there is nothing to land. If its work is \
             already in {onto} under someone else's commit, close it with `jkb task landed \
             {branch} --onto {onto}`; if the work was never committed, it is still in the session."
        );
    }

    // A CHECKOUT THAT FAILS IS NOT A CONFLICT. Nothing has been rebased yet, so whatever stopped
    // the detach — an untracked file the checkout would overwrite, a held `index.lock`, a
    // permissions error — is a property of the graft worktree, and `Graft::Conflict` would send the
    // user to resolve a conflict in a branch that has none. It used to, with git's own text
    // discarded. The error carries that text instead, the same split `scripts/merge-queue.sh`
    // makes between a setup failure and a content conflict. Nothing is put back: the graft
    // worktree is on no branch, and the next landing resets it.
    let (detached, why) = git_run(graft_dir, &["checkout", "--detach", &branch_ref])?;
    anyhow::ensure!(
        detached,
        "could not check out {branch} in {} to rebase it — {onto} did not move, and this is not a \
         conflict in {branch}. git said: {why}",
        graft_dir.display()
    );
    if !git_run(graft_dir, &["rebase", &pre])?.0 {
        let _ = git_run(graft_dir, &["rebase", "--abort"])?;
        return Ok((Graft::Conflict, pre));
    }
    let grafted = rev(graft_dir, "HEAD")?.context("rebase produced no commit")?;
    Ok((Graft::Rebased { grafted }, pre))
}

/// Fast-forward `onto` to `grafted` — which the caller has gated — in `target_dir`, the checkout
/// that holds `onto` (the operator's own, else `.jkb/base`), **only if that checkout is still on
/// `onto` and `onto` still points at `pre`**, the tip [`graft`] rebased onto.
///
/// THE COMPARE-AND-SWAP IS THE POINT. The gate ran elsewhere, for minutes, and anything may have
/// moved `onto` or switched its checkout meanwhile. A fast-forward from wherever it is now would
/// either fail with git's opaque refusal or — when the new tip happens to be an ancestor of the
/// graft — succeed over a move this side never saw. So both are compared and a mismatch refused
/// with what was found. Past the comparison, `git merge` itself updates the ref against the HEAD
/// it read, so a move in the remaining instant is either refused by git or is to a commit the
/// gated graft already contains.
///
/// **No `git switch`.** The target checkout was never detached, so there is nothing to switch
/// back; a checkout found on another branch (or none) belongs to whoever put it there, and is
/// refused rather than moved.
///
/// # Errors
/// Returns an error if `git` cannot be executed or a revision cannot be read. Every refusal is an
/// [`Advance::Refused`], never an error, because the caller owes the user a sentence saying the
/// graft passed and `onto` did not move.
pub fn advance(target_dir: &Path, onto: &str, pre: &str, grafted: &str) -> Result<Advance> {
    valid_ref(onto)?;
    valid_ref(pre)?;
    valid_ref(grafted)?;
    // The full ref, not `--short`: a short name is shortened differently when a tag shares it, and
    // a git that does not answer reads as "not on `onto`", which refuses.
    let on = git(target_dir, &["symbolic-ref", "--quiet", "HEAD"])?.filter(|s| !s.is_empty());
    if on.as_deref() != Some(format!("refs/heads/{onto}").as_str()) {
        return Ok(Advance::Refused {
            why: format!(
                "{} is no longer on {onto} (HEAD is {}) — it is left as it is, not switched \
                 back",
                target_dir.display(),
                on.as_deref().unwrap_or("detached, or unreadable")
            ),
        });
    }
    let now = rev(target_dir, "HEAD")?.context("the target branch has no commit")?;
    if now != pre {
        return Ok(Advance::Refused {
            why: format!(
                "{onto} moved while the gate ran, from {} to {} — the graft was gated against the \
                 old tip, so it is not put over the new one",
                short(pre),
                short(&now)
            ),
        });
    }
    // HOOKS OFF FOR THE FAST-FORWARD, the same rule `scripts/merge-queue.sh` states at its own
    // graft. A fast-forward fires `post-merge`, which in this repository runs setup.sh —
    // cargo-installing the jkb binary, rebuilding the VS Code extension, reinstalling the watcher
    // service. Even with the gate already passed, a landing is not the place to reinstall an
    // operator's tooling. (Every jkb git call already runs hooks-off; this one says so itself.)
    //
    // GIT'S OWN TEXT TRAVELS WITH THE REFUSAL: the checkout is on `onto` at `pre`, so what reaches
    // here — a held `index.lock`, a tracked file the fast-forward would overwrite — is whatever
    // git says it is.
    let (advanced, why) = git_run(
        target_dir,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "merge",
            "--ff-only",
            grafted,
        ],
    )?;
    if !advanced {
        return Ok(Advance::Refused {
            why: format!("git refused the fast-forward: {why}"),
        });
    }
    Ok(Advance::Advanced)
}

/// The first eight characters of a commit id, for a message.
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// Hard-reset the working tree at `dir` to `reference` — used to point a landed session at the
/// commit that landed.
///
/// # Errors
/// Returns an error if `git` cannot be executed or the reset fails.
pub fn reset_hard(dir: &Path, reference: &str) -> Result<()> {
    valid_ref(reference)?;
    git_must(dir, &["reset", "--hard", reference])?;
    Ok(())
}

/// The commit `reference` names, or `None` if this repo does not have one.
///
/// **Not [`rev`].** Plain `rev-parse` is a *parser*: handed a 40-character hex string it exits 0
/// and echoes it back whether or not the object exists, because that is already a well-formed
/// object name. So `rev` answers "is this spellable", and using it to mean "is this a commit I
/// have" once made a fabricated sha read as a real commit. `--verify --quiet` with `^{commit}` is
/// the question that actually looks the object up, and it is the one every caller wanting
/// existence must ask.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn rev_commit(dir: &Path, reference: &str) -> Result<Option<String>> {
    valid_ref(reference)?;
    Ok(git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ],
    )?
    .filter(|s| !s.is_empty()))
}

/// Whether `reference` resolves to a commit in `dir`.
fn exists(dir: &Path, reference: &str) -> Result<bool> {
    Ok(rev_commit(dir, reference)?.is_some())
}

/// Which ref to ask about when a branch exists both locally and on the remote. They can
/// disagree, and which answer is right depends on the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer {
    /// The remote-tracking copy, falling back to the local branch. Right for "did this work
    /// ship?": after a merged pull request the local branch is often stale or gone, while
    /// `origin/<branch>` reflects what was actually merged.
    Remote,
    /// The local branch, falling back to the remote copy. Right for "is this branch spent?":
    /// a staging branch whose pushed copy merged, but which has since had another task landed
    /// onto it locally, still has commits to give.
    Local,
}

/// The ref that represents `branch` here — the local branch or its remote-tracking copy — or
/// `None` if neither exists. `prefer` decides which is asked for first (see [`Prefer`]).
///
/// The **one** implementation of "given a branch name, what ref may I hand to git for it". It
/// takes a branch name and hands back a *revision* — `close-merged` resolves one to decide whether
/// to tell the user a branch is gone, and `ahead_count` to have something it can count with. A second spelling of that question got written as a bare `has_branch`, which
/// only looks at `refs/heads/` — so a branch living solely on the remote, the ordinary state after
/// a local branch is deleted post-merge or on a fresh clone, was reported "gone, remove the stale
/// tag" while it still carried unmerged work.
///
/// It is deliberately **not** the answer to "is this string a branch". Its argument is assumed to
/// be one already, and its probe is `rev-parse`, which resolves a tag, a raw object id and
/// `origin/<b>` alike — so used as an admission check it accepts values that are not branches at
/// all. That question is [`branch_name`], and the difference between the two is what let an
/// `origin/`-qualified land target be stored under a key `staging ls` could never look up.
///
/// # Errors
/// Returns an error if `git` cannot be executed.
pub fn branch_ref(dir: &Path, branch: &str, prefer: Prefer) -> Result<Option<String>> {
    valid_ref(branch)?;
    let candidates = match prefer {
        Prefer::Remote => [format!("origin/{branch}"), branch.to_owned()],
        Prefer::Local => [branch.to_owned(), format!("origin/{branch}")],
    };
    for candidate in &candidates {
        if exists(dir, candidate)? {
            return Ok(Some(candidate.clone()));
        }
    }
    Ok(None)
}

/// Make sure `branch` exists locally, **preferring an existing remote-tracking copy** over
/// `start`.
///
/// The distinction from [`create_branch`] is the whole point, and it is per-caller rather than
/// universal:
///
/// - *Adopt the remote* when the branch is one the caller is **referring to** — an explicit
///   `--onto <batch>`, or a session branch whose commits may already have been pushed. Cutting a
///   namesake from `start` there produces a branch carrying none of the work the name means, which
///   git accepts silently because no local ref exists, and the eventual push is rejected as
///   non-fast-forward.
/// - Use [`create_branch`] when the caller is **making a new branch** and `start` is the point it
///   must begin at. Adopting a same-named remote branch there is the opposite failure: a "fresh"
///   batch named after a task silently becomes some earlier, possibly already-merged batch.
///
/// This lived inside `create_branch` for one commit, which made that function ignore its own
/// `start` argument — a primitive whose name and signature promise something it does not do is a
/// trap for whoever calls it next.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the branch.
/// Returns whether the branch was **created here** rather than already existing.
///
/// Used only by [`worktree_add`], to undo a branch it created when the worktree add then fails.
/// It was once threaded out to the cut-point writer as evidence that a record under this name
/// belonged to a different branch — a flag every caller had to supply, and one a crash between
/// the git write and the database write loses. There is no cut point to protect any more.
pub fn ensure_branch(dir: &Path, branch: &str, start: &str) -> Result<bool> {
    // Composed from [`adopt_remote`] rather than repeating its logic: two functions that both
    // knew how to prefer a remote copy is the overlap that once made `create_branch` silently
    // ignore its own `start` argument.
    if adopt_remote(dir, branch)? {
        return Ok(false);
    }
    create_branch(dir, branch, start)
}

/// Create the local `branch` from its remote-tracking copy when that is the only place it
/// exists. Returns whether the branch is usable locally afterwards.
///
/// Separate from [`ensure_branch`] because it needs **no start point**: it either adopts what is
/// already published or reports that there is nothing to adopt. That matters at the callers that
/// resolve a land target, where computing a start point means resolving trunk — which fails in a
/// repo whose trunk cannot be discovered, and which is not needed at all when the branch exists.
///
/// # Errors
/// Returns an error if `git` cannot be executed or refuses to create the branch.
pub fn adopt_remote(dir: &Path, branch: &str) -> Result<bool> {
    valid_ref(branch)?;
    if has_branch(dir, branch)?.is_yes() {
        return Ok(true);
    }
    let Some(remote) = branch_ref(dir, branch, Prefer::Remote)? else {
        return Ok(false);
    };
    git_must(dir, &["branch", branch, &remote])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{current_branch, deletions_only, git_cmd, is_dirty, key, trunk};
    use jkb_fsm::Fact;
    use std::path::Path;
    use std::process::Command;

    /// Configuration injection is deliberately NOT stripped: the dev container carries
    /// `safe.directory` grants in `GIT_CONFIG_PARAMETERS`, and removing those makes git refuse
    /// the checkout. Pinned so the list is not "tidied" into a blanket sweep.
    #[test]
    fn config_injection_is_left_alone() {
        let cmd = git_cmd(Path::new("/somewhere"), &["status"]);
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for keep in ["GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"] {
            assert!(
                !removed.iter().any(|k| k == keep),
                "{keep} must not be stripped — it carries safe.directory grants"
            );
        }
    }

    /// How a process gets BUILT here — every spelling, because the guard is about the
    /// spawn and not about one way of writing it. Keyed on `Command::new(` alone, the
    /// three `Command::cargo_bin("jkb")` fixtures were invisible: deleting the
    /// `isolate_git_env` line from `Fixture::jkb` left this guard and the whole suite
    /// green, while the doc above claimed NO SPAWN IN THE CRATE goes unscrubbed. Measured.
    const SPAWN_FORMS: &[&str] = &["Command::new(", "cargo_bin("];

    /// `(file, fn)` — each of these is separately asserted to scrub, by the test named.
    const SCRUBBERS: &[(&str, &str)] = &[
        ("src/gitrepo.rs", "git_in"), // every_git_call_drops_the_callers_repository_selection
        ("src/gitrepo.rs", "fixture_git"), // the_test_fixtures_do_not_reach_another_repository
        ("src/pr.rs", "gh_cmd"),      // the_gh_spawn_does_not_inherit_a_repository_selection
        ("src/session.rs", "gate_cmd"), // the_gate_spawn_does_not_inherit_a_repository_selection
        ("src/archive.rs", "fixture_git"), // the_archive_fixture_does_not_reach_another_repository
        ("src/git_audit.rs", "fixture_git"), // the_audit_fixture_does_not_reach_another_repository
        // The spawn is in `git_cmd`, which delegates to `isolate_git_env`; the KEY is where
        // the spawn is, since that is what the scan can see.
        ("tests/sessions.rs", "git_cmd"), // the_fixture_isolation_covers_selection_and_config
        ("tests/sessions.rs", "jkb"),     // the_session_fixture_jkb_does_not_inherit_a_repository
        ("tests/cli.rs", "jkb_bare"),     // the_cli_fixture_does_not_inherit_a_repository
    ];
    /// `(file, fn)` — spawns that do NOT resolve a repository, listed so that adding one
    /// is a decision rather than an omission.
    const NOT_REPO_AWARE: &[(&str, &str, &str)] = &[
        (
            "src/owner.rs",
            "a_reaped_child_is_established_dead",
            "spawns a shell purely to own a pid; it is never asked about a repository",
        ),
        (
            "src/archive.rs",
            "the_old_store_s_files_are_never_opened",
            "spawns `mkfifo` on a temp path; it is never asked about a repository",
        ),
        (
            "src/session.rs",
            "gate_place",
            "asks `docker inspect` whether the dev container is running; it is never asked about a \
             repository (the gate itself runs through `gate_cmd`)",
        ),
        (
            "src/transcripts.rs",
            "sweep_dev_container",
            "spawns `docker`, which is addressed by CONTAINER NAME and never resolves a \
             repository: the thing it acts on is a named volume inside that container, and \
             the caller's cwd changes nothing about which container is poked",
        ),
        (
            "tests/cli.rs",
            "serve_under_nohup_keeps_sighup_ignored",
            "spawns `nohup` to start `jkb serve` with SIGHUP ignored; nohup resolves no \
             repository, and the jkb it execs gets the program, arguments and every environment \
             setting and removal of a `jkb()`-built command, so `jkb_bare`'s scrub reaches it",
        ),
        (
            "src/rbac_cli.rs",
            "bash_argv",
            "spawns `bash` to word-split literal command strings, with `jkb` shadowed by a shell \
             function; it calls `env_clear`, so it inherits no `GIT_*` variable to be scrubbed \
             of, and its `PATH` is empty -- or, for the hidden-jkb oracle, a fake `jkb` plus \
             `/usr/bin:/bin` -- so no row reaches a repository-resolving tool",
        ),
        (
            "src/rbac_cli.rs",
            "test_bash",
            "spawns each candidate `bash` only to test `BASH_VERSINFO`; it calls `env_clear` and \
             runs no command, so it is never asked about a repository",
        ),
    ];

    /// The function a declaration line declares, or `UNPARSED` when the line declares one
    /// in a shape this scan does not model. `None` when it is not a declaration at all.
    ///
    /// Qualifiers are enumerated rather than guessed at, and anything else before `fn`
    /// makes the answer `UNPARSED` — the honest third value.
    const UNPARSED: &str = "<unparsed declaration>";
    fn declared_fn(trimmed: &str) -> Option<&str> {
        // `fn` must be a whole token: `fn foo`, never `fn(u8) -> u8` (a fn-pointer type).
        let at = trimmed
            .match_indices("fn ")
            .find(|(i, _)| *i == 0 || trimmed.as_bytes()[i - 1] == b' ')?;
        let before = &trimmed[..at.0];
        let known = before.split_whitespace().all(|tok| {
            matches!(
                tok,
                "pub" | "const" | "async" | "unsafe" | "extern" | "default"
            ) || tok.starts_with("pub(")
                || (tok.starts_with('"') && tok.ends_with('"'))
        });
        if !known {
            return Some(UNPARSED);
        }
        let rest = &trimmed[at.0 + 3..];
        let name = rest.split(['(', '<']).next().unwrap_or(rest).trim();
        if name.is_empty() {
            Some(UNPARSED)
        } else {
            Some(name)
        }
    }

    /// EVERY EXEMPTION NAMES A TEST, AND THAT TEST EXISTS — lifted out of the guard above so
    /// the guard stays readable and this stays one question. `src/archive.rs` was exempted on
    /// a trailing comment naming `the_archive_fixture_does_not_reach_another_repository`,
    /// which existed nowhere in the crate — so that fixture was exempt by location with
    /// nothing observing it, and deleting its scrub left the whole suite green. That is the
    /// "a claimed pin that does not exist is worse than no claim" defect, granted BY the
    /// allowlist whose doc asserts each entry is separately checked. So the comment is the
    /// machine-checked part now and cannot rot into a false claim.
    fn every_exemption_names_a_test_that_observes_it(
        root: &Path,
        files: &[std::path::PathBuf],
        scrubbers: &[(&str, &str)],
    ) {
        let all_src: String = files
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .collect();
        let code_src = super::code_only(&all_src);
        // Assembled, never written whole: see the anchor below.
        let marker = ["const ", "SCRUBBERS"].concat();
        let mut unpinned: Vec<String> = Vec::new();
        let mut parsed: Vec<(String, String)> = Vec::new();
        let mut in_list = false;
        let self_src = std::fs::read_to_string(root.join("src/gitrepo.rs")).expect("read self");
        for line in self_src.lines() {
            // THE DECLARATION, and the needle is ASSEMBLED AT RUN TIME so that this line
            // cannot be it.
            //
            // WHEN THE CONST LIVED INSIDE THIS TEST, BELOW the scan, a literal
            // `line.contains("const SCRUBBERS")` matched its OWN source first — it reached the
            // right entries only because nothing in between happened to trim to `];` or start
            // with `(`, and one `vec![…];` in this function ended the scan on the wrong list.
            // Adding `&& line.contains("= &[")` did NOT fix it: that predicate is also true of
            // the line spelling it. Measured, both ways.
            //
            // The const has since moved to module scope ABOVE the scan, so a literal needle
            // would find the const first today and the bug would not reproduce. That is a fact
            // about the current layout, not a property of the scan: moving either block back
            // restores it. The assembled marker is what makes the anchor about the LIST rather
            // than about where this scanner happens to sit, and the identity assertion below is
            // what would notice if it stopped being.
            if line.contains(&marker) && line.contains("= &[") {
                in_list = true;
                continue;
            }
            if !in_list {
                continue;
            }
            if line.trim() == "];" {
                break;
            }
            // An ENTRY line, whatever it carries — counted before the comment is looked for, so
            // that an entry written without one is a failure rather than a silent skip. Written
            // the other way round (find the comment, then check the entry), a comment-less entry
            // `continue`d and its exemption was granted with no named test at all: the archive.rs
            // defect this check exists to close, one shape over.
            if !line.trim_start().starts_with('(') {
                continue;
            }
            // The (file, fn) pair this line declares, so the premise below can be about
            // IDENTITY rather than arity — a count matches for any six lines at all.
            let pair = line
                .trim()
                .trim_start_matches('(')
                .split_once(')')
                .map_or("", |(inner, _)| inner);
            let mut halves = pair
                .split(',')
                .map(|h| h.trim().trim_matches('"').to_owned());
            parsed.push((
                halves.next().unwrap_or_default(),
                halves.next().unwrap_or_default(),
            ));
            let Some((_, named)) = line.split_once("// ") else {
                unpinned.push(format!("{} names no test at all", line.trim()));
                continue;
            };
            let named = named.trim();
            // CODE, not text — the same rule the spawn scan above had to learn, and it matters
            // more here: these tests carry long comment blocks that name the very constructor
            // they are vouching for, so a prose scan would accept a test that only TALKS about
            // it. `code_only` blanks in place, so indices into it are indices into `all_src`.
            let Some(at) = code_src.find(&format!("fn {named}(")) else {
                unpinned.push(format!(
                    "{} names `{named}`, which is not a function here",
                    line.trim()
                ));
                continue;
            };
            // ...and the named test must OBSERVE the constructor it is named beside. Existing
            // somewhere in the crate is not evidence about this entry — a test could be named
            // here and assert something else entirely, which is the same "reads as pinned and
            // is not" shape one level up.
            //
            // The body ends at the first closing brace AT THE FUNCTION'S OWN INDENTATION, which
            // is DERIVED rather than assumed. `\n    }` was hard-coded, on the assumption that
            // every test here sits inside `mod tests`; `tests/sessions.rs` holds its tests at
            // top level, so the pattern matched the first brace NESTED inside the function
            // instead. Measured on `the_fixture_isolation_covers_selection_and_config`: the
            // body stopped at its inner `for … { … }`, six characters short of the real end.
            // Harmless there only because the constructor is called on the line above it — a
            // constructor used after that block would have been reported missing when it is
            // not, and the `map_or(body, …)` fallback fails the other way, handing `contains`
            // the rest of the crate so that it is true of almost anything. So both are gone:
            // the indentation is read off the declaration, and a body with no end is a failure.
            let line_start = code_src[..at].rfind('\n').map_or(0, |i| i + 1);
            let indent: String = code_src[line_start..at]
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();
            let Some(end) = code_src[at..].find(&format!("\n{indent}}}")) else {
                unpinned.push(format!(
                    "{} names `{named}`, whose body has no end at its own indentation",
                    line.trim()
                ));
                continue;
            };
            let body = &code_src[at..at + end];
            // An entry whose constructor cannot be read is a failure too. `map_or("", …)` plus
            // an `is_empty` skip granted the exemption with nothing checked — the same silent
            // skip the comment-less entry above had to be turned into a failure.
            let Some(ctor) = line
                .split_once(", \"")
                .and_then(|(_, r)| r.split_once('"'))
                .map(|(n, _)| n)
                .filter(|n| !n.is_empty())
            else {
                unpinned.push(format!(
                    "{} names no constructor we could read",
                    line.trim()
                ));
                continue;
            };
            if !body.contains(ctor) {
                unpinned.push(format!(
                    "{} names `{named}`, which never mentions `{ctor}` in code",
                    line.trim()
                ));
            }
        }
        // The premise, and it is about IDENTITY. `scrubbers.len() >= 6` was `6 >= 6` on a const
        // array of six, so it could not fail whatever the parse did; counting what was parsed
        // instead fixed the arity but still said nothing about WHICH six lines were read — any
        // six would have satisfied it, including six from a different list that happened to sit
        // where the scan landed. The pairs the scan reads must BE the pairs the compiler saw.
        let declared: Vec<(String, String)> = scrubbers
            .iter()
            .map(|(f, c)| ((*f).to_owned(), (*c).to_owned()))
            .collect();
        assert_eq!(
            parsed, declared,
            "the SCRUBBERS parse did not read the list the compiler saw; the block moved, was \
             renamed or was reformatted, so the entries checked were not its entries"
        );
        assert!(
            unpinned.is_empty(),
            "a SCRUBBERS entry grants an exemption while naming no test, a test that does not \
             exist, or one that never mentions the constructor it exempts: {unpinned:?}"
        );
    }

    /// NO SPAWN IN THE CRATE — production or test — resolves a repository from
    /// the environment without going through a scrubbing constructor.
    ///
    /// It began as a check that every git spawn in THIS MODULE is built by `git_cmd` — added
    /// because the behavioural test observes `git_cmd` only, so reverting `git()` to a bare
    /// `Command::new("git")` left the suite green while the doc claimed otherwise. That check
    /// is now a special case of this one and has been retired; its distinctive premise (this
    /// file holds exactly one production git spawn, so an empty result is not a broken walk)
    /// is asserted below.
    ///
    /// The rules it had to learn, each from a defect it had missed:
    ///
    /// 1. **Test code counts.** The first version cut every file at its `mod tests`, so it could
    ///    not see that this module's OWN four fixtures scrubbed nothing. Measured: with
    ///    `GIT_DIR`/`GIT_WORK_TREE` exported and a dirty checkout at the other end, running
    ///    `gitrepo::tests` commits into that repository, creates branches `deep/er` and
    ///    `mergecommit` in it, and moves its HEAD. A guard that exempts the half where the
    ///    damage was is not a guard.
    /// 2. **Exempt by LOCATION, not by name.** Keyed on the bare name `git_cmd`, a new module
    ///    copying the idiom — name included, which is the likeliest way a fifth spawn gets
    ///    written, since two files already spell it that way — was exempt on arrival.
    /// 3. **Every spawn is classified.** A program that does not resolve a repository is listed
    ///    too, with its reason, so adding one forces the decision either way rather than
    ///    defaulting to silence.
    #[test]
    fn no_spawn_in_the_crate_resolves_a_repository_unscrubbed() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        // The CRATE ROOT, so the claim "every .rs under the crate" is true — a `build.rs` git
        // spawn was outside a `src/` + `tests/` walk while the doc said coverage was complete.
        // `target/` is skipped: it holds generated sources that are not ours to classify.
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "rs") {
                    files.push(path);
                }
            }
        }
        // Finding no files means the walk is broken, and an empty result would read exactly
        // like a clean sweep — the `check_shell_syntax` lesson.
        assert!(
            files.len() >= 5,
            "walked {} source files under {}; the walk is broken, not the code",
            files.len(),
            root.display()
        );

        let mut stray: Vec<String> = Vec::new();
        let mut exempted: std::collections::HashSet<(&str, &str)> =
            std::collections::HashSet::new();
        let mut found = 0usize;
        for path in &files {
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string();
            // CODE, not text. A raw-line scan cannot see a call rustfmt has split across lines
            // (measured: that form evaded this guard, while a block comment and a closure did
            // not), and it needed two special cases — skip `//` lines, skip lines carrying an
            // escaped quote — that existed only to stop the guard's own source matching itself.
            // Blanking comments and literals removes the blind spot and both special cases.
            let scanned = super::code_only(&src);
            let mut enclosing = "<no enclosing fn>";
            for (i, line) in scanned.lines().enumerate() {
                let trimmed = line.trim_start();
                // AN UNRECOGNIZED DECLARATION IS NOT THE PREVIOUS FUNCTION. A three-prefix list
                // (`pub fn`/`pub(crate) fn`/`fn`) did not match `pub(super) fn`, so `enclosing`
                // kept the name above it — and an unscrubbed `gh` spawn written immediately
                // after `gh_cmd` inherited `gh_cmd`'s exemption and passed. Measured. That is
                // exempt-on-arrival reproduced inside the mechanism that exists to stop it, and
                // it is this project's central rule broken in its own enforcement: an unknown
                // must never be spelled as a definite answer.
                //
                // So a line is classified in three ways, not two: not a declaration (carry on),
                // a declaration whose name we parsed, or a declaration in a shape we do not
                // model — which becomes a sentinel no allowlist entry can equal, so the spawn
                // below it is REPORTED rather than exempted. Adding a spelling is then a
                // failing test, never a silent hole.
                if let Some(name) = declared_fn(trimmed) {
                    enclosing = name;
                }
                if !SPAWN_FORMS.iter().any(|f| line.contains(f)) {
                    continue;
                }
                found += 1;
                let here = (rel.as_str(), enclosing);
                if SCRUBBERS.iter().any(|&(f, n)| f == here.0 && n == here.1) {
                    continue;
                }
                if let Some(&(f, n, why)) = NOT_REPO_AWARE
                    .iter()
                    .find(|&&(f, n, _)| f == here.0 && n == here.1)
                {
                    let _ = why;
                    exempted.insert((f, n));
                    continue;
                }
                stray.push(format!("{rel}:{} in `{enclosing}`", i + 1));
            }
        }
        assert!(
            found >= 6,
            "found {found} spawns; the scan is broken, not the code"
        );
        // An exemption that matched no spawn is not harmless: it pre-approves whatever spawn is
        // next written under that (file, fn) name. One outlived its test's deletion this way, with
        // the scan green (stage-5 review).
        let stale: Vec<_> = NOT_REPO_AWARE
            .iter()
            .filter(|&&(f, n, _)| !exempted.contains(&(f, n)))
            .collect();
        assert!(
            stale.is_empty(),
            "NOT_REPO_AWARE names spawns that no longer exist; remove them: {stale:?}"
        );

        every_exemption_names_a_test_that_observes_it(root, &files, SCRUBBERS);

        let self_src = std::fs::read_to_string(root.join("src/gitrepo.rs")).expect("read self");

        // The retired per-module check's distinctive premise: this file holds exactly ONE
        // production git spawn, in `git_in`. It is what makes an empty `stray` above mean
        // "nothing bypasses" rather than "the slice was wrong" for the file that matters most.
        let self_prod = match self_src.find("\n#[cfg(test)]\nmod tests {") {
            Some(cut) => &self_src[..cut],
            None => panic!("this module has a `mod tests`, which marks the end of production code"),
        };
        assert_eq!(
            SPAWN_FORMS
                .iter()
                .map(|f| super::code_only(self_prod).matches(f).count())
                .sum::<usize>(),
            1,
            "expected exactly one production git spawn in gitrepo.rs (in `git_in`); the scan is \
             looking at the wrong slice or the spawn has been respelled"
        );
        assert!(
            stray.is_empty(),
            "a tool is spawned outside any scrubbing constructor, so it inherits the caller's \
             repository selection and may act on an unrelated repository. Route it through one \
             of {SCRUBBERS:?}, or list it in NOT_REPO_AWARE with the reason: {stray:?}"
        );
    }

    /// Everything production SELECTS is also dropped by the fixtures.
    ///
    /// The two were coupled by construction until round 26: both `fixture_git` builders called
    /// `scrub_repo_selection` on top of the config isolation, so a name added to
    /// `REPO_SELECTION_VARS` reached the fixtures automatically. That call was removed because it
    /// had become redundant AND was pinning nothing — but removing it also removed the coupling,
    /// and nothing replaced it: measured in round 27, adding `"GIT_OBJECT_DIRECTORY"` to
    /// `REPO_SELECTION_VARS` and `EXPECT_SELECTION_REMOVED` left 118 tests passing while the two
    /// library fixtures no longer dropped it, and with that variable exported they wrote loose
    /// objects into a foreign object store.
    ///
    /// This is containment, not parity. `MUST_DROP` is deliberately WIDER by three names, for two
    /// different reasons: the `GIT_CONFIG_*` channels, because production must not discard the
    /// `safe.directory` grants this container carries there; and `GIT_TEMPLATE_DIR`, because
    /// production never runs `git init`. So the assertion is one-directional and production may
    /// grow without the fixture list being wrong. It reads the constants the compiler saw, not
    /// their source text, which is what the deleted parity test did wrong.
    #[test]
    fn the_fixtures_drop_everything_production_selects() {
        let missing: Vec<&str> = super::REPO_SELECTION_VARS
            .iter()
            .filter(|v| !super::fixture_env::MUST_DROP.contains(v))
            .copied()
            .collect();
        assert!(
            missing.is_empty(),
            "production selects {missing:?} but the test fixtures do not drop them, so every \
             fixture `git` would inherit them from the developer's shell. Add each to \
             `MUST_DROP` and to `EXPECT_DROPPED` in tests/common/mod.rs — that is the edit, and \
             it is not the same as editing `REPO_SELECTION_VARS` back down."
        );
    }

    /// Every git spawn in this module drops the caller's repository selection.
    ///
    /// Asserted on the built `Command` rather than by exporting the variables, because
    /// `std::env::set_var` is process-global and would race every other test in this binary —
    /// the assertion would then be flaky in exactly the direction that reads as a pass.
    /// That git honours these over `-C` is git's own documented precedence, measured for the
    /// shell half in `scripts/tests/git-hooks.test.sh::case6p`; what can drift here, and what
    /// this pins, is whether jkb still asks it to.
    #[test]
    fn every_git_call_drops_the_callers_repository_selection() {
        super::assert_scrubbed(
            "git",
            &git_cmd(Path::new("/somewhere"), &["rev-parse", "--show-toplevel"]),
            &[],
        );
        // The config audit reads through the same spawn.
        super::assert_scrubbed(
            "git",
            &super::git_in(Path::new("/somewhere"), &["config", "--list"]),
            &[],
        );
    }

    /// The ONE builder for every git spawn in this test module.
    ///
    /// The fixtures used to neutralize the developer's git CONFIG and not the three variables
    /// that SELECT a repository — and those outrank `-C`. Measured on this branch: with
    /// `GIT_DIR`/`GIT_WORK_TREE` exported (the bare-dotfiles shell recipe), running these tests
    /// commits into the developer's unrelated repository, creates branches `deep/er` and
    /// `mergecommit` in it, and moves its HEAD off `main`. That is `./scripts/check.sh` — the
    /// gate `jkb task land` and the merge queue trust — writing to somebody else's repo.
    ///
    /// The identical incident had already been fixed in `tests/sessions.rs` and `archive.rs`
    /// and left live HERE, in the file that hosts the guard against it, under a doc comment
    /// claiming these fixtures "scrub by hand". One builder, so there is nothing to remember.
    fn fixture_git(at: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        // NO BACKGROUND MAINTENANCE, set at the one place every fixture git command goes through
        // so that no test has to remember it. git may DETACH a gc/repack that outlives the command
        // that triggered it; it then packs and deletes loose objects while a test is still walking
        // `.git/objects`, which is the shape of the intermittent `NotFound` in `copy_tree` below --
        // seen on macOS in the full parallel suite, never under a filter, never in the container.
        // Prevention rather than tolerance: with no detached process there is no window. The skip
        // in `copy_tree` stays as a belt only while this cause is inferred rather than observed.
        cmd.arg("-C").arg(at);
        cmd.arg("-c")
            .arg("gc.auto=0")
            .arg("-c")
            .arg("gc.autoDetach=false")
            .arg("-c")
            .arg("maintenance.auto=false");
        cmd.args(args);
        // ONE call, and the selection is inside it. This used to be two — a
        // `scrub_repo_selection` for the three selectors and then the config isolation — and the
        // doc above pinned the first by saying its deletion was caught. It stopped being caught
        // the moment `isolate_git_env` became the shared applier, because `MUST_DROP` is a
        // SUPERSET of `REPO_SELECTION_VARS`: measured in round 26, deleting the scrub line left
        // every test passing. A claimed pin that does not exist is worse than no claim, which
        // this cluster has already paid for once in `archive.rs`.
        //
        // It also coupled the fixtures' equality oracle to the PRODUCTION list: adding a fourth
        // selector to `REPO_SELECTION_VARS` — the edit `EXPECT_SELECTION_REMOVED`'s own doc asks
        // for — failed these fixture tests with a message forbidding the edit that reconciles
        // them. A fixture must not be able to fail because production got stricter.
        super::fixture_env::isolate_git_env(&mut cmd);
        cmd
    }

    /// `fixture_git` really scrubs — the SCRUBBERS entry above claims it, so something must
    /// check it. Deleting the `isolate_git_env` call inside it fails here: measured, the removed
    /// set comes back empty against a nine-name oracle.
    ///
    /// The sentence this replaces named `scrub_repo_selection`, a call `fixture_git` has not
    /// contained since round 26 — so the pin a reader was told to verify by reverting could not
    /// be verified at all. A claimed pin that does not exist is worse than no claim, which is the
    /// shape this cluster has now paid for three times.
    #[test]
    fn the_test_fixtures_do_not_reach_another_repository() {
        super::fixture_env::assert_isolated(
            "gitrepo fixture",
            &fixture_git(Path::new("/somewhere"), &["status"]),
        );
    }

    /// Losing the race to create a branch is reported as that race, not as git's `cannot lock ref`:
    /// two concurrent `jkb task work` on one task both find no batch branch and both cut one. The
    /// other creator is put in the window between the existence check and `git branch`, which is
    /// where the loser of a real race is.
    #[test]
    fn losing_the_race_to_create_a_branch_names_the_race() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        fixture(dir);
        let err = super::create_branch_between(dir, "batch", "main", || {
            let out = fixture_git(dir, &["branch", "batch", "main"])
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        })
        .expect_err("the branch appeared under it, so this creation lost");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("created by another process while this one was creating it"),
            "{msg}"
        );
        assert!(!msg.contains("cannot lock ref"), "{msg}");

        // The other outcomes are unchanged: present beforehand is no creation and no error, and a
        // refusal with no branch appearing still carries git's own text.
        assert!(!super::create_branch(dir, "batch", "main").unwrap());
        let err = super::create_branch(dir, "fresh", "no-such-start").unwrap_err();
        assert!(!format!("{err:#}").contains("another process"), "{err:#}");
    }

    /// Unregistering one worktree — removed, or already gone — leaves a registration whose directory
    /// this side cannot see: the other side of the host/container bind's session.
    #[test]
    fn unregistering_one_worktree_leaves_the_other_side_s() {
        use super::{forget_worktree, worktree_add, worktree_remove};
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("r");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        for name in ["mine", "gone", "theirs"] {
            worktree_add(
                &dir,
                &dir.join(".jkb/work").join(name),
                &format!("task/{name}"),
                "main",
            )
            .unwrap();
        }
        let admin = dir.join(".git/worktrees/theirs");
        std::fs::write(
            admin.join("gitdir"),
            "/nonexistent/other-side/theirs/.git\n",
        )
        .unwrap();

        worktree_remove(&dir, &dir.join(".jkb/work/mine"), true).unwrap();
        assert!(admin.exists(), "a removal prunes nothing else");
        assert!(!dir.join(".git/worktrees/mine").exists());

        std::fs::remove_dir_all(dir.join(".jkb/work/gone")).unwrap();
        forget_worktree(&dir, &dir.join(".jkb/work/gone")).unwrap();
        assert!(admin.exists(), "nor does forgetting a vanished one");
        assert!(!dir.join(".git/worktrees/gone").exists());
        assert!(
            forget_worktree(&dir, &dir.join(".jkb/work/theirs")).is_err(),
            "a directory that is there is not forgotten"
        );
    }

    /// Build a throwaway repo exercising all three GitHub merge strategies plus an
    /// unmerged control. Each branch touches its own file so the merges do not conflict.
    fn fixture(dir: &Path) {
        let run = |args: &[&str]| {
            let ok = fixture_git(dir, args).output().unwrap();
            assert!(ok.status.success(), "git {args:?}: {ok:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("base.txt"), "base").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "base"]);
        // `deep/er` exists so `has_branch("deep")` has a prefix to be wrongly matched by.
        run(&["branch", "deep/er"]);
        for branch in ["mergecommit", "squash", "rebase", "unmerged"] {
            run(&["checkout", "-q", "-b", branch, "main"]);
            std::fs::write(dir.join(format!("{branch}.txt")), "one").unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-qm", &format!("{branch} c1")]);
            std::fs::write(dir.join(format!("{branch}.txt")), "one\ntwo").unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-qm", &format!("{branch} c2")]);
            run(&["checkout", "-q", "main"]);
        }
        run(&[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #1",
            "mergecommit",
        ]);
        run(&["merge", "-q", "--squash", "squash"]);
        run(&["commit", "-qm", "squashed change (#2)"]);
        run(&["checkout", "-q", "rebase"]);
        run(&["rebase", "-q", "main"]);
        run(&["checkout", "-q", "main"]);
        run(&["merge", "-q", "--ff-only", "rebase"]);
    }

    /// A count that could not be taken must not be reported as a count of none.
    ///
    /// Zero is a load-bearing answer: `land_blocker` reads it as "nothing to land" and the In
    /// Flight row prints it. A remote-only branch's bare name resolves to nothing, so `rev-list`
    /// exited non-zero, the failure was mapped to zero, and the row refused a landing the command
    /// then performed. Refusing here is what makes that shape unrepresentable, rather than
    /// something each of the four call sites has to remember to avoid.
    #[test]
    fn an_unmeasurable_commit_count_is_refused_rather_than_reported_as_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);
        assert_eq!(
            super::ahead_count(dir, "main", "unmerged").unwrap(),
            2,
            "a measurable count is still measured"
        );
        let err = super::ahead_count(dir, "main", "no-such-branch")
            .expect_err("an unresolvable revision was answered with a number");
        assert!(
            err.to_string().contains("does not resolve"),
            "the refusal must say what could not be measured: {err}"
        );
    }

    /// `branch_refs` answers with a ref that resolves, not merely with the branch's name.
    ///
    /// A branch living only under `refs/remotes/origin/` is live — the ordinary state after a
    /// pruned local ref — and every git question asked with its bare short name fails. Returning
    /// the resolved ref is what stops a caller asking a question the name cannot answer.
    #[test]
    fn branch_refs_names_a_remote_only_branch_by_a_ref_that_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        let run = |at: &Path, args: &[&str]| {
            let ok = fixture_git(at, args).output().unwrap();
            assert!(ok.status.success(), "git {args:?}: {ok:?}");
        };
        run(tmp.path(), &["init", "-q", "--bare", "remote.git"]);
        run(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
        run(&dir, &["push", "-q", "origin", "unmerged"]);
        run(&dir, &["branch", "-D", "unmerged"]);
        // `main` must exist BOTH locally and on the remote, or nothing competes and the
        // local-over-remote assertion below holds whichever way the preference is written.
        run(&dir, &["push", "-q", "origin", "main"]);

        let refs = super::branch_refs(&dir).unwrap();
        assert_eq!(
            refs.get("unmerged").map(String::as_str),
            Some("origin/unmerged"),
            "a pruned branch was named by something git cannot resolve: {refs:?}"
        );
        assert_eq!(
            refs.get("main").map(String::as_str),
            Some("main"),
            "a local branch must keep its own name, not be replaced by its remote copy: {refs:?}"
        );
        // And the ref it hands back is one the counting question accepts.
        assert_eq!(
            super::ahead_count(&dir, "main", &refs["unmerged"]).unwrap(),
            2
        );
    }

    /// "Is this a branch, and what is it called" — every answer, including the two that are not
    /// branches at all.
    ///
    /// The exact key must win over the `origin/`-stripped form, or a local branch genuinely named
    /// `origin/x` would be read as a remote copy of `x` and its land target recorded against the
    /// wrong branch. And a tag has to be distinguishable from a name nothing uses: the first is a
    /// caller naming the wrong kind of thing, the second is a branch waiting to be cut, and
    /// `jkb task work --onto` may legitimately do the latter.
    #[test]
    fn branch_name_answers_which_branch_a_spelling_refers_to() {
        use super::BranchName::{Is, NotABranch, Unknown};

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        let run = |at: &Path, args: &[&str]| {
            let ok = fixture_git(at, args).output().unwrap();
            assert!(ok.status.success(), "git {args:?}: {ok:?}");
        };
        run(tmp.path(), &["init", "-q", "--bare", "remote.git"]);
        run(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
        run(&dir, &["push", "-q", "origin", "unmerged"]);
        run(&dir, &["branch", "-D", "unmerged"]);
        run(&dir, &["tag", "v1.0", "main"]);
        // A local branch whose name happens to start with the remote's.
        run(&dir, &["branch", "origin/decoy", "main"]);

        let name = |n: &str| super::branch_name(&dir, n).unwrap();
        assert_eq!(name("main"), Is("main".to_owned()));
        assert_eq!(
            name("unmerged"),
            Is("unmerged".to_owned()),
            "a branch that survives only on the remote is still a branch"
        );
        assert_eq!(
            name("origin/unmerged"),
            Is("unmerged".to_owned()),
            "the remote-qualified spelling must canonicalize to the key the listing uses"
        );
        assert_eq!(
            name("origin/decoy"),
            Is("origin/decoy".to_owned()),
            "a local branch named `origin/…` was read as a remote copy of something else"
        );
        assert_eq!(name("v1.0"), NotABranch, "a tag was accepted as a branch");
        assert_eq!(name("HEAD"), NotABranch);
        assert_eq!(name("no-such-thing"), Unknown);
    }

    /// **A checkout that fails is an error carrying git's text, never a `Graft::Conflict`.** An
    /// untracked file in the land tree that `unmerged`'s checkout would overwrite stops the
    /// detach before any rebase runs; this used to come back as `Conflict`, which `jkb task land`
    /// renders as "does not rebase cleanly — fix the conflict" for a branch that has none.
    #[test]
    fn a_checkout_that_fails_is_not_reported_as_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);
        std::fs::write(dir.join("unmerged.txt"), "in the way").unwrap();

        let err = super::graft(dir, "unmerged", "main")
            .map(|(outcome, _)| outcome)
            .expect_err("a refused checkout is not an outcome of the graft");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a conflict") && msg.contains("would be overwritten"),
            "the refusal names itself and carries git's own reason: {msg}"
        );
        assert_eq!(
            current_branch(dir).unwrap().as_deref(),
            Some("main"),
            "the land tree is left on the target"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("unmerged.txt")).unwrap(),
            "in the way",
            "the file in the way is untouched"
        );
    }

    /// jkb's graft worktree for the fixture at `dir`, made ready for a landing onto `onto`.
    fn graft_tree(dir: &Path, onto: &str) -> std::path::PathBuf {
        let at = dir.join(".jkb").join("graft");
        super::prepare_graft_tree(dir, &at, onto).expect("graft tree ready");
        at
    }

    /// Graft `branch` onto `onto` in the graft worktree and advance `onto` in `dir`, which holds
    /// it, as a green landing does.
    fn land(dir: &Path, branch: &str, onto: &str) {
        let at = graft_tree(dir, onto);
        let (outcome, pre) = super::graft(&at, branch, onto).expect("grafted");
        let super::Graft::Rebased { grafted } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(
            super::advance(dir, onto, &pre, &grafted).expect("advanced"),
            super::Advance::Advanced
        );
    }

    /// A fixture whose graft commits: the rebase needs an identity, and jkb's git sees no global
    /// one.
    fn grafting_fixture(dir: &Path) {
        fixture(dir);
        for kv in [["user.name", "t"], ["user.email", "t@example.com"]] {
            assert!(fixture_git(dir, &["config", kv[0], kv[1]])
                .status()
                .unwrap()
                .success());
        }
    }

    fn rev_of(dir: &Path, reference: &str) -> String {
        let out = fixture_git(dir, &["rev-parse", reference])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = fixture_git(dir, args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// **The graft moves neither `onto` nor any checkout of it.** It happens in the graft
    /// worktree, which is left detached at the rebased result so the gate runs there, while `onto`
    /// still points at its pre-graft tip and the checkout holding it is still on it. It used to
    /// fast-forward `onto` here, before the gate; and the first reorder detached the target's own
    /// checkout for the gate.
    #[test]
    fn a_graft_leaves_the_target_and_its_checkout_where_they_were() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        grafting_fixture(dir);
        let before = rev_of(dir, "refs/heads/main");
        let branch_before = rev_of(dir, "refs/heads/unmerged");

        let at = graft_tree(dir, "main");
        let (outcome, pre) = super::graft(&at, "unmerged", "main").expect("grafted");
        let super::Graft::Rebased { grafted } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(pre, before);
        assert_eq!(
            rev_of(dir, "refs/heads/main"),
            before,
            "the target moved before any gate could run"
        );
        assert_eq!(
            current_branch(dir).unwrap().as_deref(),
            Some("main"),
            "the target's checkout was taken off it"
        );
        assert!(
            !dir.join("unmerged.txt").exists(),
            "the target's checkout shows the ungated graft"
        );
        assert_eq!(
            current_branch(&at).unwrap(),
            None,
            "the graft tree is detached"
        );
        assert_eq!(rev_of(&at, "HEAD"), grafted);
        assert!(
            at.join("unmerged.txt").exists(),
            "the gate sees the integrated result"
        );
        assert_eq!(
            rev_of(dir, "refs/heads/unmerged"),
            branch_before,
            "the graft moved the branch ref"
        );
    }

    /// **The advance is a compare-and-swap on the tip.** A target that moved while the gate ran is
    /// refused, with both commits named, and left exactly where the other writer put it — never
    /// fast-forwarded from there, and never rewound.
    #[test]
    fn an_advance_refuses_a_target_that_moved_during_the_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        grafting_fixture(dir);
        let at = graft_tree(dir, "main");
        let (outcome, pre) = super::graft(&at, "unmerged", "main").expect("grafted");
        let super::Graft::Rebased { grafted } = outcome else {
            panic!("{outcome:?}");
        };
        // Somebody moves `main` while the gate runs: to a commit the graft does not contain.
        let moved = rev_of(dir, "refs/heads/squash");
        git_ok(dir, &["update-ref", "refs/heads/main", &moved]);

        let outcome = super::advance(dir, "main", &pre, &grafted).expect("answered");
        let super::Advance::Refused { why } = outcome else {
            panic!("a moved target was advanced: {outcome:?}");
        };
        assert!(
            why.contains("moved while the gate ran") && why.contains(&moved[..8]),
            "{why}"
        );
        assert!(
            !why.contains("  "),
            "a wrapped literal kept its indentation: {why}"
        );
        assert_eq!(
            rev_of(dir, "refs/heads/main"),
            moved,
            "the concurrent move is kept, not clobbered"
        );
    }

    /// **The advance is a compare-and-swap on the checkout too.** A target checkout somebody
    /// switched to another branch during the gate is refused and left on that branch: the advance
    /// never runs `git switch`, so it neither moves `onto` nor takes their checkout back. Without
    /// the check, the fast-forward would run on whatever branch the checkout is on now.
    #[test]
    fn an_advance_refuses_a_target_checkout_that_is_no_longer_on_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        grafting_fixture(dir);
        let before = rev_of(dir, "refs/heads/main");
        let at = graft_tree(dir, "main");
        let (outcome, pre) = super::graft(&at, "unmerged", "main").expect("grafted");
        let super::Graft::Rebased { grafted } = outcome else {
            panic!("{outcome:?}");
        };
        // A branch at `main`'s tip, so `HEAD == pre` holds and only the branch check can refuse.
        git_ok(dir, &["switch", "-q", "-c", "elsewhere", "main"]);

        let outcome = super::advance(dir, "main", &pre, &grafted).expect("answered");
        let super::Advance::Refused { why } = outcome else {
            panic!("a checkout off the target was fast-forwarded: {outcome:?}");
        };
        assert!(why.contains("is no longer on main"), "{why}");
        assert!(
            !why.contains("  "),
            "a wrapped literal kept its indentation: {why}"
        );
        assert_eq!(rev_of(dir, "refs/heads/main"), before, "the target moved");
        assert_eq!(
            rev_of(dir, "refs/heads/elsewhere"),
            before,
            "the branch the checkout is on now was fast-forwarded instead"
        );
        assert_eq!(
            current_branch(dir).unwrap().as_deref(),
            Some("elsewhere"),
            "the checkout was switched back"
        );

        // Detached at the target's tip is refused the same way.
        git_ok(dir, &["switch", "-q", "--detach", "main"]);
        let outcome = super::advance(dir, "main", &pre, &grafted).expect("answered");
        assert!(
            matches!(outcome, super::Advance::Refused { .. }),
            "{outcome:?}"
        );
        assert_eq!(rev_of(dir, "refs/heads/main"), before);
    }

    /// **Whatever a landing left in the graft tree, the next one starts clean.** Tracked files a
    /// gate modified, a graft left detached by an interrupted gate, and a rebase stopped part-way
    /// are each discarded by `prepare_graft_tree`, which leaves the tree detached at the target's
    /// tip — and a registration whose directory was deleted is added again.
    #[test]
    fn preparing_the_graft_tree_discards_whatever_an_earlier_landing_left() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        grafting_fixture(dir);
        let tip = rev_of(dir, "refs/heads/main");
        let clean_at_tip = |at: &Path| {
            assert_eq!(
                current_branch(at).unwrap(),
                None,
                "the graft tree is on a branch"
            );
            assert_eq!(rev_of(at, "HEAD"), tip);
            assert!(
                is_dirty(at, dir).unwrap().is_no(),
                "the graft tree is dirty"
            );
        };

        // An ungated graft and a gate's tracked edit.
        let at = graft_tree(dir, "main");
        let (outcome, _) = super::graft(&at, "unmerged", "main").expect("grafted");
        assert!(matches!(outcome, super::Graft::Rebased { .. }));
        std::fs::write(at.join("base.txt"), "the gate wrote this").unwrap();
        assert_eq!(graft_tree(dir, "main"), at);
        clean_at_tip(&at);

        // A rebase stopped on a conflict, as a landing killed mid-rebase leaves it.
        git_ok(&at, &["switch", "-q", "--detach", "refs/heads/unmerged"]);
        git_ok(dir, &["branch", "clash", "main"]);
        git_ok(
            dir,
            &[
                "worktree",
                "add",
                "-q",
                dir.join("c").to_str().unwrap(),
                "clash",
            ],
        );
        let c = dir.join("c");
        std::fs::write(c.join("unmerged.txt"), "a different one").unwrap();
        git_ok(&c, &["add", "-A"]);
        git_ok(&c, &["commit", "-qm", "clash"]);
        assert!(!fixture_git(&at, &["rebase", "refs/heads/clash"])
            .output()
            .unwrap()
            .status
            .success());
        graft_tree(dir, "main");
        clean_at_tip(&at);
        let (outcome, _) =
            super::graft(&at, "unmerged", "main").expect("grafted after a stopped rebase");
        assert!(
            matches!(outcome, super::Graft::Rebased { .. }),
            "{outcome:?}"
        );

        // A registration whose directory is gone.
        std::fs::remove_dir_all(&at).unwrap();
        graft_tree(dir, "main");
        clean_at_tip(&at);
    }

    /// **A directory at the graft path that git does not register is refused, not reused.** It is
    /// not known to be jkb's, so it is neither reset nor removed, and the refusal says what to do.
    #[test]
    fn an_unregistered_directory_at_the_graft_path_is_refused_with_its_remedy() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        grafting_fixture(dir);
        let at = dir.join(".jkb").join("graft");
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join("keep.txt"), "someone's").unwrap();

        let err = super::prepare_graft_tree(dir, &at, "main").expect_err("reused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("git does not know it as a worktree")
                && msg.contains("move it out of the way"),
            "{msg}"
        );
        assert_eq!(
            std::fs::read_to_string(at.join("keep.txt")).unwrap(),
            "someone's"
        );
    }

    /// **A symlink at the graft path is refused before any git command runs through it.** Linked to
    /// the main checkout, it canonicalizes to a registered worktree, and the reset and clean would
    /// otherwise discard the user's own tracked edit and delete their untracked file.
    #[test]
    fn a_symlink_at_the_graft_path_is_refused_before_git_runs_through_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        grafting_fixture(dir);
        std::fs::create_dir_all(dir.join(".jkb")).unwrap();
        let at = dir.join(".jkb").join("graft");
        std::os::unix::fs::symlink(dir, &at).unwrap();
        std::fs::write(dir.join("only-copy.txt"), "untracked\n").unwrap();
        std::fs::write(dir.join("base.txt"), "edited").unwrap();

        let err = super::prepare_graft_tree(dir, &at, "main").expect_err("used through the link");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("is a symlink") && msg.contains("move it out of the way"),
            "{msg}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("only-copy.txt")).unwrap(),
            "untracked\n",
            "the user's untracked file was deleted"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("base.txt")).unwrap(),
            "edited",
            "the user's tracked edit was discarded"
        );
        assert_eq!(current_branch(dir).unwrap().as_deref(), Some("main"));
    }

    /// A conflict aborts the rebase and leaves the target and its checkout untouched; the graft
    /// tree is not switched anywhere, since it is on no branch to go back to.
    #[test]
    fn a_conflicting_graft_moves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        grafting_fixture(dir);
        std::fs::write(dir.join("unmerged.txt"), "main's own").unwrap();
        git_ok(dir, &["add", "-A"]);
        git_ok(dir, &["commit", "-qm", "main clashes"]);
        let before = rev_of(dir, "refs/heads/main");

        let at = graft_tree(dir, "main");
        let (outcome, pre) = super::graft(&at, "unmerged", "main").expect("answered");
        assert_eq!(outcome, super::Graft::Conflict);
        assert_eq!(pre, before);
        assert_eq!(rev_of(dir, "refs/heads/main"), before);
        assert_eq!(current_branch(dir).unwrap().as_deref(), Some("main"));
        assert!(
            !fixture_git(&at, &["rebase", "--show-current-patch"])
                .output()
                .unwrap()
                .status
                .success(),
            "the rebase was left in progress"
        );
    }

    /// The graft counts the BRANCH's commits, not a same-named tag's. A tag `unmerged` left at
    /// `main`'s tip outranks `refs/heads/unmerged` in `rev-list`'s lookup, so the branch's two
    /// commits counted as none and the landing was refused as having nothing to land.
    #[test]
    fn a_tag_named_like_the_branch_does_not_stand_in_for_it_in_a_graft() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);
        let git_ok = |args: &[&str]| assert!(fixture_git(dir, args).status().unwrap().success());
        // The rebase commits, and the graft's own git sees no global identity.
        git_ok(&["config", "user.name", "t"]);
        git_ok(&["config", "user.email", "t@example.com"]);
        git_ok(&["tag", "unmerged", "main"]);

        land(dir, "unmerged", "main");
        assert!(
            dir.join("unmerged.txt").exists(),
            "the branch's work is on main"
        );
    }

    /// The graft rebases onto the TARGET BRANCH, not a same-named tag elsewhere. A tag `main` at
    /// `squash`'s tip outranks `refs/heads/main`, so the branch was rebased onto commits `main`
    /// never had, and the fast-forward then refused.
    #[test]
    fn a_tag_named_like_the_target_does_not_stand_in_for_it_in_a_graft() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);
        let git_ok = |args: &[&str]| assert!(fixture_git(dir, args).status().unwrap().success());
        // The rebase commits, and the graft's own git sees no global identity.
        git_ok(&["config", "user.name", "t"]);
        git_ok(&["config", "user.email", "t@example.com"]);
        git_ok(&["tag", "main", "squash"]);

        land(dir, "unmerged", "main");
        let log = fixture_git(dir, &["log", "--format=%s", "refs/heads/main"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("unmerged c2"), "{log}");
        assert!(
            !log.contains("squash c1"),
            "the tag's line was not landed: {log}"
        );
    }

    #[test]
    fn a_part_way_removal_is_told_apart_from_work_in_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);
        assert_eq!(
            deletions_only(dir, dir).unwrap(),
            super::Deletions::NotOnly,
            "a clean tree owes no advice"
        );

        // The incident's shape: tracked files missing, nothing staged, nothing untracked.
        std::fs::remove_file(dir.join("base.txt")).unwrap();
        assert_eq!(
            deletions_only(dir, dir).unwrap(),
            super::Deletions::Only(1),
            "deletions alone are recoverable with `git restore .`"
        );

        // A PROBE THAT DID NOT RUN is not a report of real work. This used to be the same
        // `None` as the case below, so a part-way removal whose probe failed was met with
        // "commit them" — the 62,421-deleted-lines advice this question exists to prevent.
        // A SEPARATE tempdir, not a subdirectory of the fixture: git's discovery walks up, so a
        // directory inside the repo is answered *by the repo* — the first version of this asserted
        // `Unknown` and got `Only(1)` from the enclosing tree, which is the same trap
        // `worktree_identity` exists for.
        let outside = tempfile::tempdir().unwrap();
        let not_a_repo = outside.path().join("elsewhere");
        std::fs::create_dir_all(&not_a_repo).unwrap();
        assert_eq!(
            deletions_only(&not_a_repo, outside.path()).unwrap(),
            super::Deletions::Unknown,
            "git declined to answer, and that is its own value"
        );

        // THE TRAP THE FENCE EXISTS FOR, asked inside the repo where it actually bites: a
        // directory that is not a worktree of its own is answered BY THE REPO AROUND IT, because
        // `git -C <dir>` is not a fence and discovery walks up. Unfenced, this reported the
        // enclosing checkout's one deletion as the subdirectory's own — which is precisely how a
        // wrecked session tree would have had the main checkout's dirt read as its own, and why
        // the case above had to be written in a separate tempdir to say anything at all.
        let inside = dir.join("subdir");
        std::fs::create_dir_all(&inside).unwrap();
        assert_eq!(
            deletions_only(&inside, dir).unwrap(),
            super::Deletions::Unknown,
            "a directory that does not answer git for itself must report nothing, not its \
             parent's dirt"
        );

        // One real edit beside them and it is work again — the advice must flip back.
        std::fs::write(dir.join("new.txt"), "mine").unwrap();
        assert_eq!(
            deletions_only(dir, dir).unwrap(),
            super::Deletions::NotOnly,
            "an untracked file beside the deletions is work, and must not be restored over"
        );
    }

    /// **Only ordinary work is worth no caveat.** The two readers of this renderer — `jkb task
    /// land` and the disposal sweep — both say "it has uncommitted changes" and then append what
    /// this returns, so a `None` here IS the bare sentence. `Unknown` returning `None` would put
    /// that sentence, unqualified, on a tree nobody established anything about, which is the
    /// collapse three-valued [`Deletions`] exists to prevent and the one both callers had.
    #[test]
    fn only_real_work_is_worth_no_caveat() {
        use super::Deletions;
        assert_eq!(
            Deletions::NotOnly.caveat(),
            None,
            "real work needs nothing added to `it has uncommitted changes`"
        );
        let only = Deletions::Only(2)
            .caveat()
            .expect("a part-way removal is caveated");
        assert!(
            only.contains('2') && only.contains("part-way removal"),
            "it says how many and what they are: {only}"
        );
        let unknown = Deletions::Unknown
            .caveat()
            .expect("an unanswered probe is caveated — it must not read as work");
        assert!(
            unknown.contains("unestablished"),
            "and it says the question went unanswered rather than describing the dirt: {unknown}"
        );
        // The property, rather than the wordings: the three do not share a rendering.
        assert_ne!(
            only, unknown,
            "two different observations, two different sentences"
        );
    }

    #[test]
    fn repo_key_branch_and_trunk_are_discovered() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fixture(dir);
        assert_eq!(current_branch(dir).unwrap().as_deref(), Some("main"));
        // No remote here, so trunk falls through to the local `main`.
        assert_eq!(trunk(dir).unwrap().as_deref(), Some("main"));
        assert!(key(dir).unwrap().is_some());
    }

    /// A `git status` that ran and FAILED is `Unknown`, never `No`.
    ///
    /// This is the whole reason `is_dirty` is three-valued. It returned `Result<bool>` and
    /// mapped a non-zero exit to `false` — *clean* — and all eight callers are guards standing
    /// in front of something destructive: `reset --hard`, the archiving rename, `git switch`
    /// across branches, and the land graft itself. A worktree whose `.git` has been unlinked
    /// part-way (the incident `deletions_only` documents, still reachable from a stale binary
    /// or a hand-run `git worktree remove`) answered "clean" to every one of them.
    #[test]
    fn a_git_status_that_could_not_run_is_unknown_not_clean() {
        let t = tempfile::tempdir().expect("tempdir");

        // Not a git repository at all, which is what an unlinked `.git` leaves behind.
        let answer = is_dirty(t.path(), t.path()).expect("git is executable");
        assert_eq!(
            answer,
            Fact::Unknown,
            "a directory git cannot read is not a clean checkout"
        );
        assert!(
            !answer.is_no(),
            "and every guard asks `is_no()`, which must therefore be false here — this is the \
             assertion that fails if `is_dirty` ever collapses back to a bool"
        );
    }

    /// `git clean -xdf` takes `.jkb/work` with it, and a landing must still complete.
    ///
    /// THE ANCHOR IS WHY THIS PASSES. `.jkb/` is in `.git/info/exclude`, so git treats the whole
    /// directory as junk and removes it — while still listing the worktrees under it as
    /// prunable, so `session::discover` returns them and `jkb task land` asks about a checkout
    /// whose parent is gone too. Anchored on the worktree's own PARENT — which is what an owner
    /// id must do, having nothing better — that reads `Unknown`, `is_dirty` reads `Unknown`, and
    /// `land_blocker` refuses for ever while printing a `git -C <worktree> status` remedy about a
    /// directory that is not there. Anchored on the repo root, which no ordinary operation
    /// removes, the absence is established and the landing completes through
    /// `Disposal::AlreadyGone` — the behaviour `a_worktree_directory_that_is_gone_...` above
    /// records as already fixed once.
    #[test]
    fn a_session_whose_whole_work_directory_was_cleaned_is_absent_not_unknown() {
        let t = tempfile::tempdir().expect("tempdir");
        let root = t.path();
        fixture(root);
        let wt = root.join(".jkb/work/x");
        std::fs::create_dir_all(&wt).expect("mkdir");

        // What `git clean -xdf` leaves: the excluded directory and everything under it, gone.
        std::fs::remove_dir_all(root.join(".jkb")).expect("clean");
        assert!(
            !wt.parent().expect("parent").exists(),
            "the premise — the PARENT is gone too, which is the whole difficulty"
        );

        assert_eq!(
            super::worktree_identity(&wt, root).expect("identity"),
            super::WorktreeIdentity::Absent,
            "the repo root is visible, so the absence is established"
        );
        assert_eq!(
            super::is_dirty(&wt, root).expect("is_dirty"),
            Fact::No,
            "and nothing there holds uncommitted work, so the landing is not refused"
        );
    }

    /// `git -C <dir>` is NOT a fence: git's discovery walks up.
    ///
    /// A session worktree whose `.git` file is gone is answered by the enclosing repository —
    /// `status` exits 0 reporting the MAIN checkout — so the state the three-valued rewrite was
    /// written for produced a confident answer about a different tree. `is_dirty` must call that
    /// `Unknown`, and `worktree_head` must refuse to borrow the parent's HEAD as a session's
    /// identity, or the sweep reads it as a different session reusing the name and holds it.
    #[test]
    fn a_worktree_whose_git_link_is_gone_does_not_answer_with_its_parents_state() {
        let t = tempfile::tempdir().expect("tempdir");
        let root = t.path();
        fixture(root);

        // A linked worktree, then its `.git` file removed — a part-way `git worktree remove`.
        let wt = root.join(".jkb/work/x");
        std::fs::create_dir_all(wt.parent().expect("parent")).expect("mkdir");
        let add = fixture_git(root, &["worktree", "add", "-q", "--detach"])
            .arg(&wt)
            .output()
            .expect("git worktree add");
        assert!(add.status.success(), "{add:?}");

        assert_eq!(
            super::worktree_identity(&wt, root).expect("identity"),
            super::WorktreeIdentity::Own,
            "the intact worktree answers for itself"
        );
        let own_head = super::worktree_head(&wt, root).expect("head");
        assert!(own_head.is_some(), "and has a HEAD of its own");

        std::fs::remove_file(wt.join(".git")).expect("unlink .git");

        // The measured half: git still answers, from the enclosing repository.
        let leaked = super::git(&wt, &["rev-parse", "--show-toplevel"]).expect("git");
        assert!(
            leaked.is_some(),
            "git answers a gutted worktree from its parent — if this ever stops being true the \
             guard below is testing nothing"
        );

        assert_eq!(
            super::worktree_identity(&wt, root).expect("identity"),
            super::WorktreeIdentity::Foreign,
            "an answer sourced from the enclosing repo is not about this tree"
        );
        assert_eq!(
            super::is_dirty(&wt, root).expect("is_dirty"),
            Fact::Unknown,
            "so dirtiness is unestablished, not the parent's cleanliness"
        );
        assert_eq!(
            super::worktree_head(&wt, root).expect("head"),
            None,
            "and a session never borrows its parent's HEAD as its own identity"
        );
    }

    /// An absent directory is an ESTABLISHED observation, not an unobtainable answer.
    ///
    /// Git keeps listing a worktree whose directory was removed (`prunable`), so `session::
    /// discover` still returns it. Spelling that `Unknown` made `land_blocker` refuse for ever a
    /// landing that used to complete through `Disposal::AlreadyGone` — and the remedy it printed
    /// (`git -C <gone dir> status`) could not be run.
    #[test]
    fn a_worktree_directory_that_is_gone_is_not_dirty_rather_than_unknown() {
        let t = tempfile::tempdir().expect("tempdir");
        let gone = t.path().join("removed");
        assert_eq!(
            super::worktree_identity(&gone, t.path()).expect("identity"),
            super::WorktreeIdentity::Absent
        );
        assert_eq!(
            super::is_dirty(&gone, t.path()).expect("is_dirty"),
            Fact::No,
            "nothing there holds no uncommitted work"
        );
    }

    /// A path that cannot be stat'd is not a PROVEN absence.
    ///
    /// `Path::exists()` answers `false` for any stat error, so an untraversable parent made
    /// `worktree_identity` say `Absent`, which `is_dirty` spells `Fact::No` — "proven clean", the
    /// one answer this module exists to stop manufacturing. It then feeds `land`'s post-gate
    /// check, `abandon_session`'s ensure, `land_dir_for`'s pre-`switch` check and the sweep's
    /// identity guard, none of which can tell it from a real observation.
    #[test]
    fn a_path_that_cannot_be_stat_ed_is_not_a_proven_absence() {
        use std::os::unix::fs::PermissionsExt;

        let t = tempfile::tempdir().expect("tempdir");
        let parent = t.path().join("locked");
        let wt = parent.join("wt");
        std::fs::create_dir_all(&wt).expect("mkdir");

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");
        // Everything is measured while the parent is untraversable, then it is restored so the
        // tempdir can be cleaned up whatever the assertions do.
        let stat = std::fs::metadata(&wt).err().map(|e| e.kind());
        // Anchored on the tempdir root, which is plainly VISIBLE — so the only thing that
        // fails is the stat of `wt` itself, and the answer must still not be a proven absence.
        let identity = super::worktree_identity(&wt, t.path());
        let dirty = super::is_dirty(&wt, t.path());
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).expect("restore");

        // Asserted, not assumed: as root the chmod does not bite, and without this the test would
        // pass having observed an ordinary readable directory.
        assert_eq!(
            stat,
            Some(std::io::ErrorKind::PermissionDenied),
            "the premise — stat must actually fail, or this test is about nothing"
        );
        assert_ne!(
            identity.expect("identity"),
            super::WorktreeIdentity::Absent,
            "nobody established that it is gone; the stat never came back"
        );
        assert_eq!(
            dirty.expect("is_dirty"),
            Fact::Unknown,
            "and an unreadable path must never read as PROVEN clean"
        );
    }

    /// A branch question git could not answer is `Unknown`, never "no such branch".
    ///
    /// `show-ref --verify --quiet` exits non-zero for BOTH "no such ref" and "this repository
    /// cannot be read", and `git` collapses every non-zero exit — so an unreadable repo reported
    /// a deleted branch and `land` printed "removed its branch".
    #[test]
    fn an_unreadable_repo_does_not_report_a_branch_as_deleted() {
        let t = tempfile::tempdir().expect("tempdir");
        let root = t.path();
        fixture(root);

        assert_eq!(
            super::has_branch(root, "squash").expect("has_branch"),
            Fact::Yes
        );
        assert_eq!(
            super::has_branch(root, "no-such-branch").expect("has_branch"),
            Fact::No,
            "a branch that really is absent is PROVEN absent"
        );
        // A `for-each-ref` pattern also matches deeper refs, so an exact comparison is required.
        assert_eq!(
            super::has_branch(root, "deep").expect("has_branch"),
            Fact::No,
            "`refs/heads/deep` must not be reported present by `refs/heads/deep/er`"
        );

        // Now make the repository unreadable.
        std::fs::write(root.join(".git/HEAD"), "not a ref\n").expect("corrupt HEAD");
        std::fs::remove_dir_all(root.join(".git/refs")).expect("remove refs");
        let _ = std::fs::remove_file(root.join(".git/packed-refs"));
        assert_eq!(
            super::has_branch(root, "squash").expect("has_branch"),
            Fact::Unknown,
            "git could not answer, which is not the same as the branch being gone"
        );
    }

    #[test]
    fn the_config_allowlist_takes_repository_shape_and_nothing_that_runs_a_program() {
        use super::config_key_allowed;
        // Every local key found across the operator's nine real repos (measured 2026-09-25).
        for ok in [
            "core.repositoryformatversion",
            "core.filemode",
            "core.bare",
            "core.logallrefupdates",
            "core.ignorecase",
            "core.precomposeunicode",
            "remote.origin.url",
            "remote.origin.fetch",
            "branch.main.remote",
            "branch.main.merge",
            "branch.task/x.vscode-merge-base",
            "extensions.relativeworktrees",
            "worktree.userelativepaths",
            "user.email",
            "pull.rebase",
        ] {
            assert!(config_key_allowed(ok), "{ok}");
        }
        for runs in [
            "core.fsmonitor",
            "core.hookspath",
            "core.sshcommand",
            "core.pager",
            "core.editor",
            "core.askpass",
            "core.gitproxy",
            "sequence.editor",
            "include.path",
            "includeif.gitdir:/x.path",
            "filter.x.clean",
            "filter.lfs.process",
            "diff.external",
            "diff.x.textconv",
            "diff.x.command",
            "merge.x.driver",
            "credential.helper",
            "credential.https://h.helper",
            "gpg.program",
            "gpg.ssh.program",
            "alias.st",
            "pager.log",
            "submodule.x.update",
            "remote.origin.uploadpack",
            "remote.origin.receivepack",
            "url.ext::sh.insteadof",
            "protocol.ext.allow",
            "core.remote.fsmonitor",
            // Not programs, but they point a landing's checkout at `$HOME` and hide what it left.
            "core.worktree",
            "status.showuntrackedfiles",
            "status.x",
        ] {
            assert!(!config_key_allowed(runs), "{runs} must be refused");
        }
    }

    /// A planted `core.fsmonitor` is what a plain `git status` on the host would run (measured). jkb's
    /// git refuses the repository instead, naming the key and its file, and the program never runs.
    #[test]
    fn a_repository_whose_own_config_names_a_program_is_refused_before_git_runs() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("r");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        let marker = t.path().join("ran");
        let evil = t.path().join("evil.sh");
        std::fs::write(&evil, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(
            super::root(&dir).unwrap().is_some(),
            "a clean repository runs"
        );
        let planted = t.path().join("r2");
        std::fs::create_dir_all(&planted).unwrap();
        fixture(&planted);
        let set = fixture_git(
            &planted,
            &["config", "core.fsmonitor", evil.to_str().unwrap()],
        )
        .status()
        .unwrap();
        assert!(set.success());
        let e = super::root(&planted).unwrap_err().to_string();
        assert!(
            e.contains("core.fsmonitor") && e.contains(".git/config"),
            "{e}"
        );
        let e = super::git_run(&planted, &["status"])
            .unwrap_err()
            .to_string();
        assert!(e.contains("core.fsmonitor"), "{e}");
        assert!(!marker.exists(), "the planted program never ran");
    }

    /// Apple's git lists its Xcode-owned layer at `unknown` scope (measured on git 2.50.1, Apple
    /// Git-155): not the repository's, so not judged — while an `unknown` file inside the repository,
    /// and every `local` or `worktree` entry, still is.
    #[test]
    fn an_unknown_scope_is_judged_only_inside_the_repository() {
        let t = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(t.path()).unwrap();
        let apple = "/Library/Developer/CommandLineTools/usr/share/git-core/gitconfig";
        assert!(!super::judged_scope("unknown", apple, Some(&repo)));
        let inside = repo.join(".git/elsewhere");
        assert!(super::judged_scope(
            "unknown",
            inside.to_str().unwrap(),
            Some(&repo)
        ));
        // Outside a repository, or in one git cannot read, Apple's layer is still Apple's: a probe
        // there must get git's own "cannot answer", not the audit's refusal (found on the host).
        assert!(
            !super::judged_scope("unknown", apple, None),
            "no repository"
        );
        for scope in ["local", "worktree"] {
            assert!(super::judged_scope(scope, apple, Some(&repo)), "{scope}");
        }
        for scope in ["system", "global", "command", ""] {
            assert!(!super::judged_scope(scope, apple, Some(&repo)), "{scope}");
        }
    }

    /// The audit is asked fresh every call: a config planted after a directory passed is refused on
    /// the next call, not waved through by a per-process memory of the first answer.
    #[test]
    fn a_config_planted_after_a_directory_passed_is_refused_on_the_next_call() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("r");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        super::audit_repo_config(&dir).expect("clean at first");
        assert!(
            fixture_git(&dir, &["config", "filter.x.clean", "touch /tmp/x"])
                .status()
                .unwrap()
                .success()
        );
        let e = super::audit_repo_config(&dir).unwrap_err().to_string();
        assert!(e.contains("filter.x.clean"), "{e}");
    }

    /// A `commondir` file redirects git's config and hooks (measured on git 2.51.1); jkb refuses the
    /// directory and says where it points, whatever that directory's config holds.
    #[test]
    fn a_git_directory_redirected_by_commondir_is_refused() {
        // EVERY FAILURE HERE NAMES ITS SUBJECT. A bare `unwrap` on the copy reported only
        // `NotFound`, which is the one thing that cannot be true of a path `read_dir` has just
        // listed -- unless it is a dangling symlink, or a directory reached as a non-dir, or the
        // destination's parent is missing. Those are three different bugs with one message, and
        // telling them apart cost several rounds across two machines because the panic named
        // neither path. The entry type is in there because `read_dir` does not follow symlinks
        // while `fs::copy` does, which is exactly how the three cases diverge.
        fn copy_tree(from: &Path, to: &Path) {
            std::fs::create_dir_all(to)
                .unwrap_or_else(|e| panic!("create_dir_all {}: {e}", to.display()));
            let entries = std::fs::read_dir(from)
                .unwrap_or_else(|e| panic!("read_dir {}: {e}", from.display()));
            for e in entries.flatten() {
                let dest = to.join(e.file_name());
                let ty = e
                    .file_type()
                    .unwrap_or_else(|err| panic!("file_type {}: {err}", e.path().display()));
                if ty.is_dir() {
                    copy_tree(&e.path(), &dest);
                } else {
                    std::fs::copy(e.path(), &dest).unwrap_or_else(|err| {
                        // A SOURCE THAT HAS VANISHED IS SKIPPED. This walks a LIVE git directory:
                        // git writes transient files under .git/objects (pack temporaries, and
                        // whatever its background maintenance leaves behind), so a path `read_dir`
                        // listed a moment ago can be gone by the time `fs::copy` opens it. The
                        // redirect this test asserts on does not depend on the object set being
                        // complete, so skipping one is harmless -- whereas failing on it is a
                        // flake, and it is the one that has been failing on macOS in the full
                        // parallel suite while passing under a filter and in the container.
                        //
                        // NARROW ON PURPOSE: only NotFound, and only when the source really is
                        // gone when asked again. A missing destination parent, a directory reached
                        // as a non-dir, or any other error still panics below naming both paths --
                        // so this cannot swallow the two bugs the message was added to tell apart.
                        if err.kind() == std::io::ErrorKind::NotFound && !e.path().exists() {
                            return 0;
                        }
                        panic!(
                            "copy {} -> {}: {err} (dir={} file={} symlink={}, target exists={})",
                            e.path().display(),
                            dest.display(),
                            ty.is_dir(),
                            ty.is_file(),
                            ty.is_symlink(),
                            e.path().exists(),
                        )
                    });
                }
            }
        }
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("r");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        let elsewhere = t.path().join("common");
        std::fs::create_dir_all(elsewhere.join("hooks")).unwrap();
        for part in ["config", "HEAD"] {
            std::fs::copy(dir.join(".git").join(part), elsewhere.join(part)).unwrap();
        }
        for part in ["objects", "refs"] {
            copy_tree(&dir.join(".git").join(part), &elsewhere.join(part));
        }
        std::fs::write(
            dir.join(".git/commondir"),
            format!("{}\n", elsewhere.display()),
        )
        .unwrap();
        let e = super::audit_repo_config(&dir).unwrap_err().to_string();
        assert!(
            e.contains("takes its config and hooks from") && e.contains("common"),
            "{e}"
        );
    }

    /// A submodule's own config is jkb's to report, not to refuse: jkb's git never enters a submodule,
    /// but yours does. Only real submodule git directories are read — a loose ref that happens to be
    /// named `config` is not a config — and git's own `core.worktree` back into the superproject passes.
    #[test]
    fn submodule_configs_are_reported_by_the_scan_and_never_block_jkb() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("r");
        let module = dir.join(".git/modules/sub");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        std::fs::create_dir_all(module.join("refs/remotes/origin/feature")).unwrap();
        std::fs::write(module.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            module.join("refs/remotes/origin/feature/config"),
            "0123456789012345678901234567890123456789\n",
        )
        .unwrap();
        let cfg = module.join("config");
        std::fs::write(
            &cfg,
            "[core]\n\trepositoryformatversion = 0\n\tworktree = ../../../sub\n",
        )
        .unwrap();
        assert_eq!(super::module_findings(&dir), Vec::<String>::new());
        std::fs::write(&cfg, "[core]\n\tworktree = ../../../../../..\n").unwrap();
        let found = super::module_findings(&dir).join("; ");
        assert!(found.contains("core.worktree outside"), "{found}");
        std::fs::write(&cfg, "[filter \"x\"]\n\tclean = touch /tmp/x\n").unwrap();
        let found = super::module_findings(&dir).join("; ");
        assert!(found.contains("filter.x.clean"), "{found}");
        super::audit_repo_config(&dir).expect("jkb's own git never reads it");
        // A submodule whose `.git` is a git directory of its own, in the checkout, is judged too.
        std::fs::write(&cfg, "[core]\n\trepositoryformatversion = 0\n").unwrap();
        std::fs::write(
            dir.join(".gitmodules"),
            "[submodule \"inline\"]\n\tpath = inline\n",
        )
        .unwrap();
        let inline = dir.join("inline/.git");
        std::fs::create_dir_all(&inline).unwrap();
        std::fs::write(inline.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            inline.join("config"),
            "[filter \"z\"]\n\tclean = touch /tmp/z\n",
        )
        .unwrap();
        let found = super::module_findings(&dir).join("; ");
        assert!(found.contains("filter.z.clean"), "{found}");
        std::fs::remove_dir_all(dir.join("inline")).unwrap();
        std::fs::remove_file(dir.join(".gitmodules")).unwrap();
        std::fs::write(&cfg, "[filter \"x\"]\n\tclean = touch /tmp/x\n").unwrap();
        std::fs::remove_file(&cfg).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(t.path().join("elsewhere"), &cfg).unwrap();
            let found = super::module_findings(&dir).join("; ");
            assert!(found.contains("is a symlink"), "{found}");
        }
    }

    /// Git you run in the superproject enters submodules of submodules and follows a symlinked `.git`
    /// (measured on git 2.51.1, review round 4), so the scan follows each `.gitmodules` down, reads an
    /// un-absorbed git directory's own `modules/`, and reports a symlink wherever git would follow one.
    #[test]
    fn the_scan_follows_nested_submodules_and_reports_symlinked_git_directories() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("r");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        let gitdir = |at: &Path, config: &str| {
            std::fs::create_dir_all(at).unwrap();
            std::fs::write(at.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            std::fs::write(at.join("config"), config).unwrap();
        };
        let clean = "[core]\n\trepositoryformatversion = 0\n";
        // `sub` is absorbed; its nested `n` keeps its git directory inline, in the checkout.
        gitdir(&dir.join(".git/modules/sub"), clean);
        std::fs::write(
            dir.join(".gitmodules"),
            "[submodule \"sub\"]\n\tpath = sub\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        assert_eq!(super::module_findings(&dir), Vec::<String>::new());
        std::fs::write(
            dir.join("sub/.gitmodules"),
            "[submodule \"n\"]\n\tpath = n\n",
        )
        .unwrap();
        gitdir(
            &dir.join("sub/n/.git"),
            "[filter \"q\"]\n\tclean = touch /tmp/q\n",
        );
        let found = super::module_findings(&dir).join("; ");
        assert!(found.contains("filter.q.clean"), "{found}");
        // An un-absorbed git directory's own `modules/` holds its submodules' git directories.
        std::fs::write(dir.join("sub/n/.git/config"), clean).unwrap();
        gitdir(
            &dir.join("sub/n/.git/modules/deep"),
            "[filter \"w\"]\n\tclean = touch /tmp/w\n",
        );
        let found = super::module_findings(&dir).join("; ");
        assert!(found.contains("filter.w.clean"), "{found}");
        std::fs::remove_dir_all(dir.join("sub/n")).unwrap();
        std::fs::remove_file(dir.join("sub/.gitmodules")).unwrap();
        assert_eq!(super::module_findings(&dir), Vec::<String>::new());
        #[cfg(unix)]
        {
            // A symlinked `.git` — git follows it to whatever config is there.
            let elsewhere = t.path().join("elsewhere");
            gitdir(&elsewhere, "[filter \"y\"]\n\tclean = touch /tmp/y\n");
            std::fs::remove_file(dir.join("sub/.git")).unwrap();
            std::os::unix::fs::symlink(&elsewhere, dir.join("sub/.git")).unwrap();
            let found = super::module_findings(&dir).join("; ");
            assert!(found.contains("sub/.git is a symlink"), "{found}");
            std::fs::remove_file(dir.join("sub/.git")).unwrap();
            std::fs::write(dir.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
            // And a symlink among the absorbed git directories.
            std::os::unix::fs::symlink(&elsewhere, dir.join(".git/modules/other")).unwrap();
            let found = super::module_findings(&dir).join("; ");
            assert!(found.contains("modules/other is a symlink"), "{found}");
        }
    }

    /// A `.gitmodules` the container wrote is hostile input: a path at or above its own checkout is
    /// reported and not followed (`path = .` twenty times over would otherwise grow as 20^depth), a
    /// `.git` file naming a git directory the walk never reaches is judged anyway, and a symlinked
    /// `modules/` is reported rather than read through (review round 5).
    #[test]
    fn the_scan_holds_against_a_hostile_gitmodules() {
        use std::fmt::Write as _;
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("r");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        let gitdir = |at: &Path, config: &str| {
            std::fs::create_dir_all(at).unwrap();
            std::fs::write(at.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            std::fs::write(at.join("config"), config).unwrap();
        };
        let clean = "[core]\n\trepositoryformatversion = 0\n";
        // An un-absorbed submodule whose `.gitmodules` names itself twenty times, and the root.
        gitdir(&dir.join("x/.git"), clean);
        std::fs::write(dir.join(".gitmodules"), "[submodule \"x\"]\n\tpath = x\n").unwrap();
        let mut gm = String::new();
        for i in 0..20 {
            let _ = write!(gm, "[submodule \"s{i}\"]\n\tpath = .\n");
        }
        gm.push_str("[submodule \"up\"]\n\tpath = ..\n");
        std::fs::write(dir.join("x/.gitmodules"), gm).unwrap();
        let found = super::module_findings(&dir).join("; ");
        assert!(
            found.contains("names `.`, outside its own checkout"),
            "{found}"
        );
        assert!(
            found.contains("names `..`, outside its own checkout"),
            "{found}"
        );
        std::fs::remove_dir_all(dir.join("x")).unwrap();
        // A `.git` file naming a git directory inside a judged one, where the walk does not look.
        gitdir(&dir.join(".git/modules/sub"), clean);
        gitdir(
            &dir.join(".git/modules/sub/evil"),
            "[filter \"e\"]\n\tclean = touch /tmp/e\n",
        );
        std::fs::write(
            dir.join(".gitmodules"),
            "[submodule \"sub\"]\n\tpath = sub\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/.git"), "gitdir: ../.git/modules/sub/evil\n").unwrap();
        let found = super::module_findings(&dir).join("; ");
        assert!(found.contains("filter.e.clean"), "{found}");
        // A `commondir` in a submodule's git directory takes its config from somewhere else.
        std::fs::write(dir.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        let evil = t.path().join("evil");
        gitdir(&evil, "[filter \"c\"]\n\tclean = touch /tmp/c\n");
        std::fs::write(
            dir.join(".git/modules/sub/commondir"),
            format!("{}\n", evil.display()),
        )
        .unwrap();
        let found = super::module_findings(&dir).join("; ");
        assert!(found.contains("redirects its config and hooks"), "{found}");
        std::fs::remove_file(dir.join(".git/modules/sub/commondir")).unwrap();
        #[cfg(unix)]
        {
            // An un-absorbed git directory whose `modules/` is a symlink.
            std::fs::remove_dir_all(dir.join("sub")).unwrap();
            gitdir(&dir.join("sub/.git"), clean);
            let elsewhere = t.path().join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, dir.join("sub/.git/modules")).unwrap();
            let found = super::module_findings(&dir).join("; ");
            assert!(found.contains(".git/modules is a symlink"), "{found}");
        }
    }

    /// A hook in a submodule's own `hooks/` runs for git you run there (review rounds 7–9): reported,
    /// unless it is git's sample or your template's byte for byte — a template that is not beside the
    /// repositories — and a `hooks/` the scan cannot list is reported too.
    #[test]
    fn the_scan_reports_a_hook_planted_in_a_submodule() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("r");
        std::fs::create_dir_all(&dir).unwrap();
        fixture(&dir);
        let module = dir.join(".git/modules/sub");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            module.join("config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .unwrap();
        let hooks = dir.join(".git/modules/sub/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-commit.sample"), "#!/bin/sh\n").unwrap();
        assert!(
            !super::module_findings(&dir)
                .join("; ")
                .contains("is a hook"),
            "a sample is not a hook"
        );
        std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\ntouch /tmp/h\n").unwrap();
        let found = super::module_findings(&dir).join("; ");
        assert!(found.contains("pre-commit is a hook"), "{found}");
        // One git copied from your `init.templateDir` is yours, byte for byte; an edit is not.
        // Your template lives outside the tree the repositories are in.
        let home = tempfile::tempdir().unwrap();
        let template = home.path().join("template/hooks");
        std::fs::create_dir_all(&template).unwrap();
        std::fs::write(template.join("pre-commit"), "#!/bin/sh\ntouch /tmp/h\n").unwrap();
        let with = |dir: &Path| super::findings_with(dir, Some(template.clone())).join("; ");
        assert!(!with(&dir).contains("is a hook"), "{}", with(&dir));
        std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\ntouch /tmp/other\n").unwrap();
        assert!(
            with(&dir).contains("pre-commit is a hook"),
            "{}",
            with(&dir)
        );
        // Not a template beside the repositories, though: the container writes that too.
        let beside = dir.parent().unwrap().join("dotfiles/template/hooks");
        std::fs::create_dir_all(&beside).unwrap();
        std::fs::copy(hooks.join("pre-commit"), beside.join("pre-commit")).unwrap();
        let found = super::findings_with(&dir, Some(beside.clone())).join("; ");
        assert!(found.contains("pre-commit is a hook"), "{found}");
        let clean = t.path().join("clean");
        std::fs::create_dir_all(&clean).unwrap();
        fixture(&clean);
        assert_eq!(
            super::findings_with(&clean, Some(beside.clone())),
            Vec::<String>::new(),
            "no standing finding against a repository with no hook"
        );
        #[cfg(unix)]
        {
            // Nor one hook of your real template symlinked into the repositories' tree.
            std::fs::remove_file(template.join("pre-commit")).unwrap();
            std::os::unix::fs::symlink(beside.join("pre-commit"), template.join("pre-commit"))
                .unwrap();
            let found = with(&dir);
            assert!(found.contains("pre-commit is a hook"), "{found}");
            assert!(
                found.contains("move your template out"),
                "and says why: {found}"
            );
        }
        #[cfg(unix)]
        {
            // A `hooks/` git can search but the scan cannot list still runs a hook by name.
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o311)).unwrap();
            if std::fs::read_dir(&hooks).is_err() {
                let found = super::module_findings(&dir).join("; ");
                assert!(found.contains("hooks cannot be listed"), "{found}");
            }
            std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Your template is read as git reads it when it copies one in, `[include]`s and all (review
    /// round 9: a dotfiles include read as no template, and every hook it installed as planted).
    #[test]
    fn the_template_is_found_through_an_include() {
        let t = tempfile::tempdir().unwrap();
        let (global, included) = (t.path().join("gitconfig"), t.path().join("included"));
        std::fs::write(
            &global,
            format!("[include]\n\tpath = {}\n", included.display()),
        )
        .unwrap();
        let template = t.path().join("tpl");
        std::fs::write(
            &included,
            format!("[init]\n\ttemplateDir = {}\n", template.display()),
        )
        .unwrap();
        assert_eq!(
            super::template_hooks(t.path(), Some(&global)),
            Some(template.join("hooks"))
        );
    }

    /// A session's own `.gitmodules` is the one git reads there: a submodule its branch adds, which
    /// the main checkout has never heard of, is scanned from the session.
    #[test]
    fn the_scan_reads_a_session_s_own_gitmodules() {
        let t = tempfile::tempdir().unwrap();
        let main = t.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        fixture(&main);
        let session = main.join(".jkb/work/s");
        assert!(fixture_git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "s",
                session.to_str().unwrap()
            ]
        )
        .status()
        .unwrap()
        .success());
        std::fs::write(
            session.join(".gitmodules"),
            "[submodule \"only\"]\n\tpath = only\n",
        )
        .unwrap();
        let inline = session.join("only/.git");
        std::fs::create_dir_all(&inline).unwrap();
        std::fs::write(inline.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            inline.join("config"),
            "[filter \"s\"]\n\tclean = touch /tmp/s\n",
        )
        .unwrap();
        assert!(!main.join(".gitmodules").exists());
        let found = super::module_findings(&session).join("; ");
        assert!(found.contains("filter.s.clean"), "{found}");
    }

    /// Measured on git 2.51.1: a superproject `git status` runs a clean filter planted in a
    /// submodule's own config. jkb's git does not enter the submodule, so it runs nothing — and still
    /// sees the submodule's commit move.
    #[test]
    fn jkb_s_git_does_not_run_a_submodule_s_config() {
        let t = tempfile::tempdir().unwrap();
        let (child, sup) = (t.path().join("child"), t.path().join("sup"));
        let marker = t.path().join("ran");
        for d in [&child, &sup] {
            std::fs::create_dir_all(d).unwrap();
            fixture(d);
        }
        std::fs::write(child.join("f"), "aaaa\n").unwrap();
        std::fs::write(child.join(".gitattributes"), "* filter=y\n").unwrap();
        let run = |dir: &Path, args: &[&str]| {
            assert!(
                fixture_git(dir, args).status().unwrap().success(),
                "{args:?}"
            );
        };
        run(&child, &["add", "-A"]);
        run(&child, &["commit", "-qm", "c"]);
        run(
            &sup,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                child.to_str().unwrap(),
                "sub",
            ],
        );
        run(&sup, &["commit", "-qm", "s"]);
        let cfg = sup.join(".git/modules/sub/config");
        let mut text = std::fs::read_to_string(&cfg).unwrap();
        {
            use std::fmt::Write as _;
            let _ = write!(
                text,
                "[filter \"y\"]\n\tclean = touch {}; cat\n",
                marker.display()
            );
        }
        std::fs::write(&cfg, text).unwrap();
        // Same size, new mtime: git must hash it, through the filter, to know.
        std::fs::write(sup.join("sub/f"), "bbbb\n").unwrap();
        let _ = super::git_run(&sup, &["status", "--porcelain"]);
        assert!(!marker.exists(), "the submodule's filter ran");
        // A tracked `.gitmodules` asking git to look inside anyway does not outrank the command line.
        let mut gm = std::fs::read_to_string(sup.join(".gitmodules")).unwrap();
        gm.push_str("\tignore = none\n");
        std::fs::write(sup.join(".gitmodules"), gm).unwrap();
        std::fs::write(sup.join("sub/f"), "cccc\n").unwrap();
        let _ = super::git_run(&sup, &["status", "--porcelain"]);
        let _ = super::git_run(&sup, &["diff", "--name-only"]);
        assert!(
            !marker.exists(),
            "`.gitmodules` put git back into the submodule"
        );
        // A landing's checkout and switch report local changes — which enters the submodule — unless
        // told not to.
        run(&sup, &["branch", "other"]);
        std::fs::write(sup.join("sub/f"), "dddd\n").unwrap();
        let _ = super::git_run(&sup, &["checkout", "--detach", "other"]);
        let _ = super::git_run(&sup, &["switch", "other"]);
        assert!(!marker.exists(), "checkout or switch entered the submodule");
        // The compare-and-delete the disposal sweep tidies a branch with is ref plumbing alone.
        run(&sup, &["branch", "tidied"]);
        let tip = super::rev(&sup, "refs/heads/tidied").unwrap().unwrap();
        std::fs::write(sup.join("sub/f"), "eeee\n").unwrap();
        assert!(super::delete_branch_at(&sup, "tidied", &tip).unwrap());
        assert!(!marker.exists(), "update-ref entered the submodule");
        // The graft tree's `clean -fd` removes the superproject's untracked file and neither enters
        // the submodule nor deletes inside it.
        std::fs::write(sup.join("stray.txt"), "x").unwrap();
        std::fs::write(sup.join("sub/inside.txt"), "x").unwrap();
        std::fs::write(sup.join("sub/f"), "ffff\n").unwrap();
        assert!(super::git_run(&sup, &["clean", "-fd"]).unwrap().0);
        assert!(!sup.join("stray.txt").exists(), "clean removed nothing");
        assert!(
            sup.join("sub/inside.txt").exists(),
            "clean descended into the submodule"
        );
        assert!(!marker.exists(), "clean entered the submodule");
        let e = super::git_run(&sup, &["stash"]).unwrap_err().to_string();
        assert!(e.contains("does not run `git stash`"), "{e}");
    }

    /// The delete is conditional on the tip the caller read: a branch that moved since is kept,
    /// with the commit that moved it, and one still at that tip is deleted.
    #[test]
    fn delete_branch_at_keeps_a_branch_that_moved_since_it_was_read() {
        let t = tempfile::tempdir().unwrap();
        fixture(t.path());
        let read = super::rev(t.path(), "refs/heads/unmerged")
            .unwrap()
            .unwrap();
        let moved = super::rev(t.path(), "refs/heads/mergecommit")
            .unwrap()
            .unwrap();
        assert_ne!(read, moved, "the premise: two different commits");
        let ok = fixture_git(t.path(), &["update-ref", "refs/heads/unmerged", &moved])
            .output()
            .unwrap();
        assert!(ok.status.success(), "{ok:?}");

        assert!(!super::delete_branch_at(t.path(), "unmerged", &read).unwrap());
        assert_eq!(
            super::rev(t.path(), "refs/heads/unmerged").unwrap(),
            Some(moved.clone()),
            "the moved branch survives, where it moved to"
        );
        assert!(super::delete_branch_at(t.path(), "unmerged", &moved).unwrap());
        assert!(super::has_branch(t.path(), "unmerged").unwrap().is_no());
        // Never resolved as a revision: `HEAD` would match whatever the branch is at.
        let e = super::delete_branch_at(t.path(), "deep/er", "HEAD").unwrap_err();
        assert!(e.to_string().contains("not a full object id"), "{e}");
    }

    /// A submodule checked out inside a session worktree has a git directory of its own, under the
    /// repository's `.git/worktrees/<s>/modules`, and passes; a stray repository nested there does not.
    #[test]
    fn a_submodule_in_a_session_passes_and_a_stray_nested_repository_does_not() {
        let t = tempfile::tempdir().unwrap();
        let (child, main) = (t.path().join("child"), t.path().join("main"));
        for d in [&child, &main] {
            std::fs::create_dir_all(d).unwrap();
            fixture(d);
        }
        let run = |dir: &Path, args: &[&str]| {
            assert!(
                fixture_git(dir, args).status().unwrap().success(),
                "{args:?}"
            );
        };
        run(
            &main,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                child.to_str().unwrap(),
                "sub",
            ],
        );
        run(&main, &["commit", "-qm", "s"]);
        let session = main.join(".jkb/work/s");
        run(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "s",
                session.to_str().unwrap(),
            ],
        );
        run(
            &session,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "-q",
                "--init",
            ],
        );
        super::audit_repo_config(&session.join("sub")).expect("its own submodule");
        super::audit_repo_config(&session).expect("and the session itself");
        assert_eq!(super::module_findings(&session), Vec::<String>::new());
        assert_eq!(super::module_findings(&main), Vec::<String>::new());
        run(
            &child,
            &["config", "core.worktree", t.path().to_str().unwrap()],
        );
        let e = super::audit_repo_config(&child).unwrap_err().to_string();
        assert!(
            e.contains("core.worktree"),
            "outside a submodule it is refused: {e}"
        );
        let stray = session.join("vendor/lib");
        std::fs::create_dir_all(&stray).unwrap();
        fixture(&stray);
        let e = super::audit_repo_config(&stray).unwrap_err().to_string();
        assert!(e.contains("takes its config and hooks from"), "{e}");
    }

    /// A jkb session's `.git` file pointed at a git directory laid out as a worktree of some other,
    /// planted repository: consistent to git, and still not its own repository's.
    #[test]
    fn a_session_whose_common_directory_is_not_its_repository_s_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let main = t.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        fixture(&main);
        let session = main.join(".jkb/work/s");
        assert!(fixture_git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "s",
                session.to_str().unwrap()
            ]
        )
        .status()
        .unwrap()
        .success());
        super::audit_repo_config(&session).expect("a real session passes");
        let rogue = t.path().join("rogue");
        std::fs::create_dir_all(&rogue).unwrap();
        fixture(&rogue);
        assert!(fixture_git(
            &rogue,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "r",
                t.path().join("rw").to_str().unwrap()
            ]
        )
        .status()
        .unwrap()
        .success());
        std::fs::write(
            session.join(".git"),
            format!("gitdir: {}\n", rogue.join(".git/worktrees/rw").display()),
        )
        .unwrap();
        let e = super::audit_repo_config(&session).unwrap_err().to_string();
        assert!(e.contains("takes its config and hooks from"), "{e}");
    }

    /// jkb's own git calls run no hooks and no fsmonitor, whatever the repository says.
    #[test]
    fn jkb_git_runs_hooks_off() {
        let cmd = git_cmd(Path::new("/somewhere"), &["status"]);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1] == "core.hooksPath=/dev/null"),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1] == "core.fsmonitor=false"),
            "{args:?}"
        );
    }
}
