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
/// everything else the user runs, and both writers here are unconditional — `try_ensure` fires on
/// any `jkb` invocation whose stamp does not match and `fs::write`s the set with no existence
/// check, no prompt and no backup, while `uninstall` `remove_file`s it and prints "removed"
/// having never checked it wrote that file. Under a bare stem that destroys a user's own
/// `code-review.js`, a name far likelier to be taken than `task-swarm`. A prefixed name is just
/// as predictable for a `scriptPath` — which is the only reason the bare one was kept — and it
/// cannot collide, so there is nothing left to guard against per call site.
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

/// Write `set` into `dir`, naming each file `{prefix}{stem}.{ext}`. Prints each write when
/// `verbose` (an explicit `install`); stays silent for the auto-install path.
fn write_set(
    dir: &Path,
    prefix: &str,
    ext: &str,
    set: &[(&str, &str)],
    verbose: bool,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (stem, body) in set {
        let path = dir.join(format!("{prefix}{stem}.{ext}"));
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        if verbose {
            println!("wrote {}", path.display());
        }
    }
    Ok(())
}

/// Path of the stamp recording which bundled-asset version has been reconciled into the
/// config dir, so auto-install only acts when the shipped bundle actually changes.
fn stamp_path(base: &Path) -> PathBuf {
    base.join(".jkb-assets")
}

/// A stable fingerprint of the bundled asset set (names + contents); changes whenever the
/// binary ships different commands or workflows.
fn fingerprint() -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for entry in BUNDLED_COMMANDS.iter().chain(BUNDLED_WORKFLOWS.iter()) {
        entry.0.hash(&mut h);
        entry.1.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// Remove `set` from `dir` (files named `{prefix}{stem}.{ext}`), reporting each.
fn remove_set(dir: &Path, prefix: &str, ext: &str, set: &[(&str, &str)]) -> Result<()> {
    for (stem, _) in set {
        let path = dir.join(format!("{prefix}{stem}.{ext}"));
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            println!("removed {}", path.display());
        } else {
            println!("not installed: {}", path.display());
        }
    }
    Ok(())
}

/// Write every bundled command and workflow into the Claude Code config directory.
///
/// # Errors
/// Returns an error if `HOME` is unset or an asset can't be written.
pub fn install() -> Result<()> {
    let base = base_dir()?;
    write_set(
        &base.join("commands"),
        ASSET_PREFIX,
        "md",
        BUNDLED_COMMANDS,
        true,
    )?;
    write_set(
        &base.join("workflows"),
        ASSET_PREFIX,
        "js",
        BUNDLED_WORKFLOWS,
        true,
    )?;
    std::fs::write(stamp_path(&base), fingerprint()).context("writing asset stamp")?;
    println!(
        "installed {} command(s) and {} workflow(s).",
        BUNDLED_COMMANDS.len(),
        BUNDLED_WORKFLOWS.len(),
    );
    println!(
        "commands: {}",
        BUNDLED_COMMANDS
            .iter()
            .map(|(s, _)| format!("/{ASSET_PREFIX}{s}"))
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
    let base = base_dir()?;
    remove_set(&base.join("commands"), ASSET_PREFIX, "md", BUNDLED_COMMANDS)?;
    remove_set(
        &base.join("workflows"),
        ASSET_PREFIX,
        "js",
        BUNDLED_WORKFLOWS,
    )?;
    if base.exists() {
        // Mark the current bundle reconciled so auto-install won't re-add these until the
        // binary ships a different set.
        std::fs::write(stamp_path(&base), fingerprint()).context("writing asset stamp")?;
        println!(
            "removed. Auto-install won't re-add them until the next `jkb` upgrade; set \
             JKB_NO_AUTO_COMMANDS=1 to disable auto-install entirely."
        );
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
    let _ = try_ensure();
}

fn try_ensure() -> Result<()> {
    let base = base_dir()?;
    if !base.exists() {
        return Ok(());
    }
    let stamp = stamp_path(&base);
    let want = fingerprint();
    if std::fs::read_to_string(&stamp).ok().as_deref() == Some(want.as_str()) {
        return Ok(());
    }
    write_set(
        &base.join("commands"),
        ASSET_PREFIX,
        "md",
        BUNDLED_COMMANDS,
        false,
    )?;
    write_set(
        &base.join("workflows"),
        ASSET_PREFIX,
        "js",
        BUNDLED_WORKFLOWS,
        false,
    )?;
    std::fs::write(&stamp, want).context("writing asset stamp")?;
    Ok(())
}

/// List the bundled assets and where `install` would write them (a dry run).
///
/// # Errors
/// Returns an error if `HOME` is unset (so the target directory can't be resolved).
pub fn list() -> Result<()> {
    let base = base_dir()?;
    let commands = base.join("commands");
    let workflows = base.join("workflows");
    println!("config directory: {}", base.display());
    println!("commands ({}):", commands.display());
    for (stem, _) in BUNDLED_COMMANDS {
        let mark = mark(&commands.join(format!("{ASSET_PREFIX}{stem}.md")));
        println!("  /{ASSET_PREFIX}{stem}  ({mark})");
    }
    println!("workflows ({}):", workflows.display());
    for (stem, _) in BUNDLED_WORKFLOWS {
        let mark = mark(&workflows.join(format!("{ASSET_PREFIX}{stem}.js")));
        println!("  {ASSET_PREFIX}{stem}  ({mark})");
    }
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
