//! Install jkb's bundled Claude Code assets — slash commands and workflows — so they
//! travel with the `jkb` binary and are available on any machine you install it on, not
//! just inside this repo.
//!
//! The markdown/JS lives in the repo's `.claude/{commands,workflows}/` (committed so it
//! ships across machines) and is embedded here at compile time, mirroring how
//! `service.rs` ships its unit files. `jkb commands install` writes each asset into the
//! user's Claude Code config directory; `uninstall` removes them; `list` is a dry run.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Filename prefix for **every** installed asset, so `/jkb-<name>` is unambiguous and a later
/// `uninstall` knows exactly which files are ours.
///
/// **Workflows carry it too, and that is the whole guard.** `~/.claude/workflows/` is shared with
/// everything else the user runs, and both writers here are unconditional — the auto-install
/// fires on any `jkb` invocation whose stamp does not match and `fs::write`s the set with no
/// existence check, no prompt and no backup, while `uninstall` `remove_file`s it and prints
/// "removed" having never checked it wrote that file. Under a bare stem that destroys a user's
/// own `code-review.js`, a name far likelier to be taken than `task-swarm`. A prefixed name is
/// just as predictable for a `scriptPath` — which is the only reason the bare one was kept — and
/// it cannot collide.
///
/// **It is spelled once, in [`asset_path`].** It used to be an argument six call sites passed,
/// which is the shape this project keeps failing at: reverting it to `""` in the auto-install's
/// two calls only — the one path a user cannot avoid, and the one that destroyed the file above —
/// left the entire suite green, because the test written to catch exactly that ran `install_into`
/// and `uninstall_from` and nothing ran the auto path. No caller can spell a config-dir path now,
/// so there is no per-call-site rule left to remember or to test for.
const ASSET_PREFIX: &str = "jkb-";

/// Bundled slash commands as `(invocation stem, embedded markdown)`.
///
/// **The set is closed under its own references**, and the two tests below are what hold it that
/// way: a command that names a workflow by path, or another command by slash name, is useless
/// without it. `/jkb-review-log` shipped for a release defering its central step to `/review` —
/// which was not bundled, and whose `code-review.js` was not either, so all three of its fallback
/// paths missed on every machine outside this repo and the command dead-ended at the step it
/// exists for.
const BUNDLED_COMMANDS: &[(&str, &str)] = &[
    (
        "design-pass",
        include_str!("../../../.claude/commands/design-pass.md"),
    ),
    (
        "next-task",
        include_str!("../../../.claude/commands/next-task.md"),
    ),
    (
        "review",
        include_str!("../../../.claude/commands/review.md"),
    ),
    (
        "review-log",
        include_str!("../../../.claude/commands/review-log.md"),
    ),
    (
        "task-swarm",
        include_str!("../../../.claude/commands/task-swarm.md"),
    ),
];

/// Bundled workflows as `(script stem, embedded JavaScript)`.
const BUNDLED_WORKFLOWS: &[(&str, &str)] = &[
    (
        "code-review",
        include_str!("../../../.claude/workflows/code-review.js"),
    ),
    (
        "task-swarm",
        include_str!("../../../.claude/workflows/task-swarm.js"),
    ),
];

/// One kind of bundled asset: the config-directory subdirectory it installs into, the extension
/// it carries there, and the set it ships. Iterating this is what keeps every verb — install,
/// auto-install, uninstall, list — over the same files under the same names.
struct Kind {
    dir: &'static str,
    ext: &'static str,
    /// What a user types to reach one: `/` for a slash command, nothing for a workflow (named by
    /// path or by `meta.name`). Display only.
    sigil: &'static str,
    set: &'static [(&'static str, &'static str)],
}

const KINDS: &[Kind] = &[
    Kind {
        dir: "commands",
        ext: "md",
        sigil: "/",
        set: BUNDLED_COMMANDS,
    },
    Kind {
        dir: "workflows",
        ext: "js",
        sigil: "",
        set: BUNDLED_WORKFLOWS,
    },
];

/// What a bundled `stem` is called once installed — the one application of [`ASSET_PREFIX`], so
/// what jkb writes and what jkb tells you it wrote cannot disagree.
fn installed_name(stem: &str) -> String {
    format!("{ASSET_PREFIX}{stem}")
}

/// **The only place a path in the user's config directory is spelled.** Every writer, remover and
/// reader goes through it, so [`ASSET_PREFIX`] cannot be applied by four verbs and forgotten by a
/// fifth. See that constant for the regression this shape exists to make impossible.
fn asset_path(base: &Path, kind: &Kind, stem: &str) -> PathBuf {
    base.join(kind.dir)
        .join(format!("{}.{}", installed_name(stem), kind.ext))
}

/// Where a `jkb` from before the prefix rename wrote the same asset. Read-only: see
/// [`legacy_siblings`].
fn legacy_path(base: &Path, kind: &Kind, stem: &str) -> PathBuf {
    base.join(kind.dir).join(format!("{stem}.{}", kind.ext))
}

/// The Claude Code config base: `$CLAUDE_CONFIG_DIR` if set, else `$HOME/.claude`.
fn base_dir() -> Result<PathBuf> {
    if let Some(base) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(base));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    Ok(home.join(".claude"))
}

/// Write every bundled asset into `base` and stamp the bundle reconciled. Prints each write when
/// `verbose` (an explicit `install`); stays silent for the auto-install path.
///
/// The two writers differ **only** in that flag, and share this body for the reason
/// [`ASSET_PREFIX`] gives: they used to be two copies of the same six-argument sequence, and a
/// change to one of them was invisible to every test.
fn write_all(base: &Path, verbose: bool) -> Result<()> {
    for kind in KINDS {
        let dir = base.join(kind.dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for (stem, body) in kind.set {
            let path = asset_path(base, kind, stem);
            std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
            if verbose {
                println!("wrote {}", path.display());
            }
        }
    }
    std::fs::write(stamp_path(base), fingerprint()).context("writing asset stamp")?;
    Ok(())
}

/// Path of the stamp recording which bundled-asset version has been reconciled into the
/// config dir, so auto-install only acts when the shipped bundle actually changes.
fn stamp_path(base: &Path) -> PathBuf {
    base.join(".jkb-assets")
}

/// A stable fingerprint of the bundled asset set; changes whenever the binary ships different
/// commands or workflows — **or writes the same ones under different names**.
fn fingerprint() -> String {
    hash_of_bundle(ASSET_PREFIX)
}

/// The stamp's input, with the prefix taken as an argument so a test can show the installed
/// **name** is part of it.
///
/// It hashes the *installed path*, not the stem. Hashing the stem alone made a prefix regression
/// self-perpetuating: a config directory written under the wrong names still matched the stamp,
/// so the auto-install — the one thing that would have corrected it — read as already reconciled
/// and never ran again.
fn hash_of_bundle(prefix: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for kind in KINDS {
        for (stem, body) in kind.set {
            format!("{}/{prefix}{stem}.{}", kind.dir, kind.ext).hash(&mut h);
            body.hash(&mut h);
        }
    }
    format!("{:016x}", h.finish())
}

/// Remove every bundled asset from `base`, reporting each.
fn remove_all(base: &Path) -> Result<()> {
    for kind in KINDS {
        for (stem, _) in kind.set {
            let path = asset_path(base, kind, stem);
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                println!("removed {}", path.display());
            } else {
                println!("not installed: {}", path.display());
            }
        }
    }
    Ok(())
}

/// Files sharing a bundled asset's name **without** the prefix — what a `jkb` from before the
/// rename installed into these same directories.
///
/// Reported, never removed. jkb can no longer prove those bytes are its own: a user's own
/// `code-review.js` is the same filename in the same directory, and the whole point of the rename
/// is that jkb stopped touching files it did not write. But leaving them unmentioned makes the
/// state both permanent and invisible — `list` shows only prefixed names and `uninstall` prints
/// "removed." — so they are named, with what is known about them, and the user decides.
fn legacy_siblings(base: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for kind in KINDS {
        for (stem, _) in kind.set {
            let path = legacy_path(base, kind, stem);
            if path.exists() {
                found.push(path);
            }
        }
    }
    found
}

/// Print [`legacy_siblings`], if any. Silent when there are none.
fn report_legacy(base: &Path) {
    let found = legacy_siblings(base);
    if found.is_empty() {
        return;
    }
    println!(
        "\n{} unprefixed file(s) share a name with a bundled asset. A jkb from before the \
         `{ASSET_PREFIX}` rename installed files under these names; this jkb neither writes nor \
         removes them, because they may equally be your own. Delete by hand if they are not \
         yours:",
        found.len()
    );
    for path in found {
        println!("  {}", path.display());
    }
}

/// Write every bundled command and workflow into the Claude Code config directory.
///
/// # Errors
/// Returns an error if `HOME` is unset or an asset can't be written.
pub fn install() -> Result<()> {
    install_into(&base_dir()?)
}

/// The body of [`install`], against an explicit config base so a test can watch what it writes.
///
/// Split out for exactly that, and [`ensure_into`] is split out for the same reason: the harm
/// this module can do is *what lands in a real directory*, and a test that reads only the
/// bundled bodies cannot see it.
fn install_into(base: &Path) -> Result<()> {
    write_all(base, true)?;
    println!(
        "installed {} command(s) and {} workflow(s).",
        BUNDLED_COMMANDS.len(),
        BUNDLED_WORKFLOWS.len(),
    );
    println!(
        "commands: {}",
        BUNDLED_COMMANDS
            .iter()
            .map(|(s, _)| format!("/{}", installed_name(s)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("reopen Claude Code (or start a new session) to pick them up.");
    Ok(())
}

/// Remove every bundled command and workflow from the Claude Code config directory.
///
/// # Errors
/// Returns an error if `HOME` is unset or an asset can't be removed.
pub fn uninstall() -> Result<()> {
    uninstall_from(&base_dir()?)
}

/// The body of [`uninstall`], against an explicit config base. See [`install_into`].
fn uninstall_from(base: &Path) -> Result<()> {
    remove_all(base)?;
    if base.exists() {
        // Mark the current bundle reconciled so auto-install won't re-add these until the
        // binary ships a different set.
        std::fs::write(stamp_path(base), fingerprint()).context("writing asset stamp")?;
        println!(
            "removed. Auto-install won't re-add them until the next `jkb` upgrade; set \
             JKB_NO_AUTO_COMMANDS=1 to disable auto-install entirely."
        );
        // …but "removed" is only true of what this jkb writes. A pre-rename jkb's files are
        // still there, and this is the moment the user believes they are gone.
        report_legacy(base);
    }
    Ok(())
}

/// Best-effort: keep the bundled assets present and current in the user's Claude Code
/// config dir, so they're available without an explicit `jkb commands install`. No-ops
/// (and never propagates an error to the caller) when `JKB_NO_AUTO_COMMANDS` is set, when
/// the config dir doesn't exist (Claude Code not in use — never created here), or when the
/// shipped bundle is already reconciled. Silent on success, so it is safe to call before
/// any command including `jkb mcp` (whose stdout is the MCP protocol).
pub fn ensure_installed() {
    if std::env::var_os("JKB_NO_AUTO_COMMANDS").is_some() {
        return;
    }
    let _ = base_dir().and_then(|base| ensure_into(&base));
}

/// The body of [`ensure_installed`], against an explicit config base. This is the writer that
/// runs on **every** non-`commands` `jkb` invocation — the one a user cannot avoid, and the one
/// that overwrote a user's own `~/.claude/workflows/code-review.js`. See [`install_into`] for why
/// it is reachable from a test at all.
fn ensure_into(base: &Path) -> Result<()> {
    if !base.exists() {
        return Ok(());
    }
    if std::fs::read_to_string(stamp_path(base)).ok().as_deref() == Some(fingerprint().as_str()) {
        return Ok(());
    }
    write_all(base, false)
}

/// List the bundled assets and where `install` would write them (a dry run).
///
/// # Errors
/// Returns an error if `HOME` is unset (so the target directory can't be resolved).
pub fn list() -> Result<()> {
    let base = base_dir()?;
    println!("config directory: {}", base.display());
    for kind in KINDS {
        println!("{} ({}):", kind.dir, base.join(kind.dir).display());
        for (stem, _) in kind.set {
            let mark = mark(&asset_path(&base, kind, stem));
            println!("  {}{}  ({mark})", kind.sigil, installed_name(stem));
        }
    }
    // A dry run that showed only what jkb writes would leave a pre-rename jkb's files invisible
    // here as well as at `uninstall` — nothing would ever mention them again.
    report_legacy(&base);
    Ok(())
}

/// `"installed"` if `path` exists, else `"bundled"`.
fn mark(path: &Path) -> &'static str {
    if path.exists() {
        "installed"
    } else {
        "bundled"
    }
}

#[cfg(test)]
mod tests {
    use super::{ASSET_PREFIX, BUNDLED_COMMANDS, BUNDLED_WORKFLOWS};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn stems(set: &'static [(&'static str, &'static str)]) -> BTreeSet<&'static str> {
        set.iter().map(|(stem, _)| *stem).collect()
    }

    /// The stems of every `*.{ext}` asset in the repo's `.claude/{kind}/`.
    ///
    /// Read from the directory rather than listed here: a second hand-written list would drift
    /// from the first, which is the failure these tests exist to catch one level up.
    fn repo_stems(kind: &str, ext: &str) -> BTreeSet<String> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.claude")
            .join(kind);
        std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some(ext))
            .filter_map(|path| Some(path.file_stem()?.to_string_lossy().into_owned()))
            .collect()
    }

    /// Every `{dir}/<stem>.{ext}` path `body` names, as `(repo_local, stem)`.
    ///
    /// `repo_local` is true only for a `./.claude/{dir}/…` reference — the in-repo fallback every
    /// launcher lists last, which reads this repo's own working tree and therefore uses the bare
    /// stem. Every other reference resolves inside the user's config directory, where the file is
    /// whatever [`super::install`] wrote it as; the two are told apart here so the test below can
    /// hold each to the name that actually exists at that path.
    fn path_refs(body: &str, dir: &str, ext: &str) -> BTreeSet<(bool, String)> {
        let suffix = format!(".{ext}");
        let marker = format!("{dir}/");
        let mut out = BTreeSet::new();
        let mut from = 0;
        while let Some(offset) = body[from..].find(&marker) {
            let at = from + offset;
            let tail = &body[at + marker.len()..];
            let stem: String = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !stem.is_empty() && tail[stem.len()..].starts_with(&suffix) {
                out.insert((body[..at].ends_with("./.claude/"), stem));
            }
            from = at + marker.len();
        }
        out
    }

    /// Whether `body` names the slash command `stem` — `/stem` on both word boundaries, so
    /// `workflows/code-review.js` is not read as a reference to `/code-review` and `/review-log`
    /// is not read as one to `/review`.
    fn names_command(body: &str, stem: &str) -> bool {
        let needle = format!("/{stem}");
        let mut from = 0;
        while let Some(offset) = body[from..].find(&needle) {
            let at = from + offset;
            let before = body[..at].chars().next_back();
            let after = body[at + needle.len()..].chars().next();
            let ident = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
            if !matches!(before, Some(c) if ident(c) || c == '/' || c == '.')
                && !matches!(after, Some(c) if ident(c))
            {
                return true;
            }
            from = at + 1;
        }
        false
    }

    /// **The bundle is closed under the paths it names, under the names it is installed as.**
    /// `jkb commands install` is the whole point of `.claude/` being embedded — an asset that
    /// reaches for a sibling the installer did not write dead-ends on every machine that is not
    /// this repo, and does so silently, because the command itself installs and runs.
    ///
    /// Both halves of that are checked, because being in the bundle is not enough: the installer
    /// writes `{ASSET_PREFIX}<stem>`, so a config-directory path spelled with the bare stem names
    /// a file nothing puts there. Only the trailing `./.claude/…` fallback, which reads this
    /// repo's own working tree, may use it.
    #[test]
    fn a_bundled_asset_names_no_workflow_or_command_the_bundle_omits() {
        let sets: [(&str, &str, BTreeSet<&str>); 2] = [
            ("workflows", "js", stems(BUNDLED_WORKFLOWS)),
            ("commands", "md", stems(BUNDLED_COMMANDS)),
        ];
        for (stem, body) in BUNDLED_COMMANDS.iter().chain(BUNDLED_WORKFLOWS.iter()) {
            for (dir, ext, bundled) in &sets {
                for (repo_local, named) in path_refs(body, dir, ext) {
                    let want = if repo_local {
                        Some(named.as_str())
                    } else {
                        named.strip_prefix(ASSET_PREFIX)
                    };
                    let Some(want) = want else {
                        panic!(
                            "`{stem}` reaches for `{dir}/{named}.{ext}` outside this repo, but \
                             `jkb commands install` writes `{ASSET_PREFIX}{named}.{ext}` — spell \
                             the installed name"
                        );
                    };
                    assert!(
                        bundled.contains(want),
                        "`{stem}` reaches for `{dir}/{named}.{ext}`, which `jkb commands install` \
                         does not write — add `{want}` to the bundle"
                    );
                }
            }
        }
    }

    /// …and under the slash commands it names, **as the user will be able to type them**.
    ///
    /// `/jkb-review-log` deferred its central step to `/review`, which was not bundled; the
    /// reference is by name rather than by path, so the check above cannot see it. Which names are
    /// *ours* comes from the repo's own `.claude/commands/`, so a host command like
    /// `/security-review` is not claimed.
    ///
    /// The bare stem is a **failure**, not merely an unbundled name. `install` writes
    /// `{ASSET_PREFIX}<stem>.md`, so `/design-pass` resolves on exactly one machine in the world —
    /// this repo's checkout — while the command telling you to run it ships everywhere. Checking
    /// the repo-side stem is what let five of those through: the name was in the list, so the
    /// assertion held, and its own message named a file the installer does not write.
    #[test]
    fn a_bundled_command_names_the_slash_commands_the_installer_writes() {
        let bundled = stems(BUNDLED_COMMANDS);
        let ours = repo_stems("commands", "md");
        assert!(
            ours.contains("review"),
            "the repo's command directory was not found, so this test would pass vacuously: {ours:?}"
        );
        for (stem, body) in BUNDLED_COMMANDS.iter().chain(BUNDLED_WORKFLOWS.iter()) {
            for named in &ours {
                assert!(
                    !names_command(body, named),
                    "`{stem}` tells the reader to run `/{named}`, which exists only inside this \
                     repo — `jkb commands install` writes it as `/{ASSET_PREFIX}{named}`"
                );
                let installed = format!("{ASSET_PREFIX}{named}");
                assert!(
                    !names_command(body, &installed) || bundled.contains(named.as_str()),
                    "`{stem}` tells the reader to run `/{installed}`, which \
                     `jkb commands install` does not write — add `{named}` to BUNDLED_COMMANDS"
                );
            }
        }
    }

    /// **Every writer prefixes every asset, and none of them touches a file jkb did not write.**
    ///
    /// The one test that watches the config directory rather than the bundled bodies, and it runs
    /// **each writer by name**. Both are unconditional — the auto-install fires on any `jkb`
    /// invocation whose stamp does not match and `fs::write`s the whole set with no existence
    /// check, no prompt and no backup, and `uninstall` `remove_file`s it while printing "removed"
    /// — so under the bare stem workflows used to carry, `jkb <anything>` destroyed a user's own
    /// `~/.claude/workflows/code-review.js` and `jkb commands uninstall` then deleted it.
    ///
    /// Its first version ran `install_into` and `uninstall_from` only, and reverting the prefix
    /// in the **auto-install** — the path that caused the harm, and the only one a user cannot
    /// avoid — left the entire suite green. Covering one writer of two is what the writers now
    /// sharing [`super::write_all`] makes unnecessary; running both anyway is what makes a third
    /// writer, added later without that discipline, fail here rather than in someone's config
    /// directory.
    ///
    /// The user's file is checked after **both** verbs, because they fail differently: a write
    /// overwrites it, `uninstall` deletes it, and a prefix dropped from only one still loses it.
    #[test]
    fn every_writer_prefixes_every_asset_and_none_touches_a_file_jkb_did_not_write() {
        const MINE: &str = "// my own review workflow, not jkb's\n";
        type Writer = fn(&std::path::Path) -> anyhow::Result<()>;
        // Both writers, named: `install` is the explicit one, `ensure` is the one that runs on
        // every non-`commands` invocation of the binary.
        let writers: [(&str, Writer); 2] = [
            ("jkb commands install", super::install_into),
            ("auto-install (any jkb invocation)", super::ensure_into),
        ];

        let dir = tempfile::tempdir().expect("temp config dir");
        let base = dir.path();
        let workflows = base.join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        // A name that collides with a bundled stem — likelier to be chosen for `code-review` than
        // for anything else jkb ships, which is what makes this data loss rather than a naming
        // preference.
        let theirs = workflows.join("code-review.js");

        // Read through a missing file rather than unwrapping it: `uninstall`'s failure mode is
        // deletion, so an unwrap here would panic on the read and never reach the sentence that
        // says what was lost.
        let survives = || std::fs::read_to_string(&theirs).unwrap_or_default();

        for (writer, write) in writers {
            std::fs::write(&theirs, MINE).unwrap();
            // The auto-install no-ops on a matching stamp, and the previous iteration left one.
            // Without this it would write nothing and pass every assertion below vacuously.
            let _ = std::fs::remove_file(super::stamp_path(base));

            // THE HARM FIRST, so a build with the prefix dropped fails on the file it destroyed
            // rather than on one of our own filenames being absent.
            write(base).unwrap_or_else(|e| panic!("{writer}: {e}"));
            assert_eq!(
                survives(),
                MINE,
                "`{writer}` overwrote a workflow of the user's own that it never wrote"
            );

            // …and that our own assets really are there under the installed name, so the
            // assertion above cannot be satisfied by a writer that writes nothing at all.
            for kind in super::KINDS {
                for (stem, _) in kind.set {
                    let path = super::asset_path(base, kind, stem);
                    assert!(
                        path.exists(),
                        "`{writer}` wrote no {}, so {stem} went in under some other name",
                        path.display()
                    );
                }
            }

            super::uninstall_from(base).expect("uninstall");
            assert_eq!(
                survives(),
                MINE,
                "`jkb commands uninstall` deleted a workflow of the user's own that it never \
                 wrote (installed by `{writer}`)"
            );
            for kind in super::KINDS {
                for (stem, _) in kind.set {
                    assert!(
                        !super::asset_path(base, kind, stem).exists(),
                        "uninstall left {stem} behind"
                    );
                }
            }
        }
    }

    /// **The stamp covers the name each asset is installed under, not just its stem and body.**
    ///
    /// Without this the prefix regression above is *self-perpetuating*: a config directory
    /// written under the wrong names still matches the stamp, so the auto-install — the only
    /// thing that would put the right files there — reads as already reconciled and never runs
    /// again. A user who took one bad build would keep the damaged directory across every
    /// upgrade that did not also change an asset's contents.
    #[test]
    fn the_asset_stamp_changes_when_the_installed_names_do() {
        assert_ne!(
            super::hash_of_bundle(ASSET_PREFIX),
            super::hash_of_bundle(""),
            "the stamp is blind to the name each asset is written under, so a config directory \
             holding the wrong names reads as reconciled and is never corrected"
        );
        assert_eq!(
            super::fingerprint(),
            super::hash_of_bundle(ASSET_PREFIX),
            "the stamp is not taken over the names the installer actually writes"
        );
    }

    /// **A file jkb wrote before the prefix rename is reported, not removed and not ignored.**
    ///
    /// `uninstall` prints "removed." and `list` shows only prefixed names, so an earlier jkb's
    /// `code-review.js` would otherwise be permanent *and* invisible — and it is also what makes
    /// resolving a workflow by name ambiguous. jkb cannot prove those bytes are its own (that is
    /// the whole reason for the rename), so it says what it knows and leaves the file alone.
    #[test]
    fn a_file_an_older_jkb_installed_is_reported_and_left_alone() {
        const OLD: &str = "// installed by a jkb from before the rename\n";
        let dir = tempfile::tempdir().expect("temp config dir");
        let base = dir.path();
        std::fs::create_dir_all(base.join("workflows")).unwrap();
        let stale = super::legacy_path(base, &super::KINDS[1], "code-review");
        std::fs::write(&stale, OLD).unwrap();

        super::install_into(base).expect("install");
        super::uninstall_from(base).expect("uninstall");
        assert_eq!(
            std::fs::read_to_string(&stale).unwrap_or_default(),
            OLD,
            "an unprefixed file was removed — jkb cannot prove those bytes are its own"
        );
        assert_eq!(
            super::legacy_siblings(base),
            vec![stale],
            "the unprefixed file an older jkb left is not reported, so `uninstall`'s \"removed.\" \
             and `list` both describe a config directory that is not the one on disk"
        );
    }

    /// Every `name: "<value>"` / `name: '<value>'` in `body`. A `name:` whose value is not a
    /// quoted literal — `name: { type: 'string' }`, `name: f.name` — is not a reference to
    /// anything and is skipped.
    fn name_refs(body: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut from = 0;
        while let Some(offset) = body[from..].find("name:") {
            let at = from + offset;
            from = at + "name:".len();
            let rest = body[from..].trim_start();
            let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                continue;
            };
            if let Some(end) = rest[1..].find(quote) {
                out.insert(rest[1..=end].to_owned());
            }
        }
        out
    }

    /// **A workflow's advertised identity is the name it is installed under.**
    ///
    /// The Workflow tool can be asked for a script by `meta.name` instead of by path, and
    /// `/jkb-task-swarm` offers exactly that. Prefixing only the *filename* left both copies on a
    /// machine that had upgraded — the stale `task-swarm.js` an older jkb wrote and the new
    /// `jkb-task-swarm.js` — declaring the same `name: 'task-swarm'`, so resolving by name was
    /// ambiguous and could run the outdated script. The filename rule is checked by the write
    /// test above; this is the same rule in the one place a filename is not what identifies the
    /// file.
    #[test]
    fn a_bundled_workflow_is_named_as_it_is_installed() {
        let bare = stems(BUNDLED_WORKFLOWS);
        for (stem, body) in BUNDLED_WORKFLOWS {
            let want = format!("{ASSET_PREFIX}{stem}");
            assert!(
                name_refs(body).contains(&want),
                "workflow `{stem}` installs as `{want}.js` but does not declare \
                 `name: '{want}'` — asked for by name it is either unfindable or ambiguous with \
                 whatever else on the machine claims that name"
            );
        }
        for (stem, body) in BUNDLED_COMMANDS.iter().chain(BUNDLED_WORKFLOWS.iter()) {
            for named in name_refs(body) {
                assert!(
                    !bare.contains(named.as_str()),
                    "`{stem}` asks for the workflow by the name `{named}`, which is what a \
                     pre-rename jkb installed — `jkb commands install` writes it as \
                     `{ASSET_PREFIX}{named}`"
                );
            }
        }
    }

    /// Every `.rs` file under this crate's `src/`.
    fn crate_sources() -> Vec<PathBuf> {
        fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            let entries =
                std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
            for entry in entries {
                let path = entry.expect("directory entry").path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut out,
        );
        out
    }

    /// Every string literal in `src`, with comments skipped and `\`-newline continuations joined
    /// as the compiler joins them (so a message split across source lines is searched as the one
    /// sentence the user reads).
    ///
    /// The literal/comment split *is* the rule this check needs: what the binary prints is a
    /// literal, while `/jkb-task-swarm` in a doc comment is prose about the workflow and tells
    /// nobody what to type.
    fn string_literals(src: &str) -> Vec<String> {
        let c: Vec<char> = src.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < c.len() {
            match c[i] {
                '/' if c.get(i + 1) == Some(&'/') => {
                    while i < c.len() && c[i] != '\n' {
                        i += 1;
                    }
                }
                '/' if c.get(i + 1) == Some(&'*') => {
                    let mut depth = 1usize;
                    i += 2;
                    while i < c.len() && depth > 0 {
                        if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                            depth += 1;
                            i += 2;
                        } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                            depth -= 1;
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                }
                // A char literal may hold a quote (`'"'`); a lifetime never closes, so it falls
                // through and is read as ordinary text.
                '\'' => {
                    let mut j = i + 1 + usize::from(c.get(i + 1) == Some(&'\\'));
                    j += 1;
                    i = if c.get(j) == Some(&'\'') {
                        j + 1
                    } else {
                        i + 1
                    };
                }
                '"' => {
                    let mut lit = String::new();
                    i += 1;
                    while i < c.len() && c[i] != '"' {
                        if c[i] == '\\' && c.get(i + 1) == Some(&'\n') {
                            i += 2;
                            while c.get(i).is_some_and(|ch| ch.is_whitespace()) {
                                i += 1;
                            }
                        } else if c[i] == '\\' {
                            lit.push(c[i]);
                            i += 1;
                            if i < c.len() {
                                lit.push(c[i]);
                                i += 1;
                            }
                        } else {
                            lit.push(c[i]);
                            i += 1;
                        }
                    }
                    i += 1;
                    out.push(lit);
                }
                _ => i += 1,
            }
        }
        out
    }

    /// **What the binary prints is held to the same rule as what the bundle ships.**
    ///
    /// `jkb task land`'s refusal said "run `/review-log`", and `jkb guide` — the cheat sheet
    /// shipped to every machine — said it too. After the rename that command exists in exactly
    /// one place in the world: this checkout. The bundled bodies were already checked; the
    /// binary's own messages state the same rule and nothing checked those, so the project
    /// contradicted itself (`README.md` advertises the prefixed names).
    ///
    /// Comments are not searched — see [`string_literals`]. A doc comment naming `/jkb-task-swarm`
    /// is describing the workflow, not telling anyone what to type.
    #[test]
    fn no_message_the_binary_prints_names_a_command_only_this_checkout_has() {
        let ours = repo_stems("commands", "md");
        assert!(
            ours.contains("review-log"),
            "the repo's command directory was not found, so this test would pass vacuously: \
             {ours:?}"
        );
        let sources = crate_sources();
        assert!(
            sources.len() > 1,
            "no crate sources found, so this test would pass vacuously"
        );
        for path in sources {
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            for lit in string_literals(&src) {
                for named in &ours {
                    assert!(
                        !names_command(&lit, named),
                        "{} prints a message telling the reader to run `/{named}`, which exists \
                         only inside this checkout — `jkb commands install` writes it as \
                         `/{ASSET_PREFIX}{named}`:\n  {lit}",
                        path.display()
                    );
                }
            }
        }
    }

    #[test]
    fn bundled_commands_carry_frontmatter() {
        assert!(!BUNDLED_COMMANDS.is_empty());
        for (stem, body) in BUNDLED_COMMANDS {
            assert!(!stem.is_empty());
            assert!(body.contains("description:"), "{stem} missing frontmatter");
        }
    }

    #[test]
    fn bundled_workflows_export_meta() {
        assert!(!BUNDLED_WORKFLOWS.is_empty());
        for (stem, body) in BUNDLED_WORKFLOWS {
            assert!(!stem.is_empty());
            assert!(body.contains("export const meta"), "{stem} missing meta");
        }
    }
}
