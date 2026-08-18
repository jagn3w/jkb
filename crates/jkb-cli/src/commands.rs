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

/// Filename prefix for installed commands, so `/jkb-<name>` is unambiguous and a later
/// `uninstall` knows exactly which files are ours. Workflows are launched by path, so
/// they keep their bare name for a predictable `scriptPath`.
const CMD_PREFIX: &str = "jkb-";

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
        CMD_PREFIX,
        "md",
        BUNDLED_COMMANDS,
        true,
    )?;
    write_set(&base.join("workflows"), "", "js", BUNDLED_WORKFLOWS, true)?;
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
            .map(|(s, _)| format!("/{CMD_PREFIX}{s}"))
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
    remove_set(&base.join("commands"), CMD_PREFIX, "md", BUNDLED_COMMANDS)?;
    remove_set(&base.join("workflows"), "", "js", BUNDLED_WORKFLOWS)?;
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
        CMD_PREFIX,
        "md",
        BUNDLED_COMMANDS,
        false,
    )?;
    write_set(&base.join("workflows"), "", "js", BUNDLED_WORKFLOWS, false)?;
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
        let mark = mark(&commands.join(format!("{CMD_PREFIX}{stem}.md")));
        println!("  /{CMD_PREFIX}{stem}  ({mark})");
    }
    println!("workflows ({}):", workflows.display());
    for (stem, _) in BUNDLED_WORKFLOWS {
        let mark = mark(&workflows.join(format!("{stem}.js")));
        println!("  {stem}  ({mark})");
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
    use super::{BUNDLED_COMMANDS, BUNDLED_WORKFLOWS};
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

    /// Every `{dir}/<stem>.{ext}` path `body` names.
    fn path_refs(body: &str, dir: &str, ext: &str) -> BTreeSet<String> {
        let suffix = format!(".{ext}");
        body.split(&format!("{dir}/"))
            .skip(1)
            .filter_map(|tail| {
                let stem: String = tail
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
                (!stem.is_empty() && tail[stem.len()..].starts_with(&suffix)).then_some(stem)
            })
            .collect()
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

    /// **The bundle is closed under the paths it names.** `jkb commands install` is the whole
    /// point of `.claude/` being embedded — an asset that reaches for a sibling the installer did
    /// not write dead-ends on every machine that is not this repo, and does so silently, because
    /// the command itself installs and runs.
    #[test]
    fn a_bundled_asset_names_no_workflow_or_command_the_bundle_omits() {
        let commands = stems(BUNDLED_COMMANDS);
        let workflows = stems(BUNDLED_WORKFLOWS);
        for (stem, body) in BUNDLED_COMMANDS.iter().chain(BUNDLED_WORKFLOWS.iter()) {
            for named in path_refs(body, "workflows", "js") {
                assert!(
                    workflows.contains(named.as_str()),
                    "`{stem}` reaches for `workflows/{named}.js`, which \
                     `jkb commands install` does not write — add it to BUNDLED_WORKFLOWS"
                );
            }
            for named in path_refs(body, "commands", "md") {
                assert!(
                    commands.contains(named.as_str()),
                    "`{stem}` reaches for `commands/{named}.md`, which \
                     `jkb commands install` does not write — add it to BUNDLED_COMMANDS"
                );
            }
        }
    }

    /// …and under the slash commands it names. `/jkb-review-log` deferred its central step to
    /// `/review`, which was not bundled; the reference is by name rather than by path, so the
    /// check above cannot see it. Which names are *ours* comes from the repo's own
    /// `.claude/commands/`, so a host command like `/security-review` is not claimed.
    #[test]
    fn a_bundled_command_names_no_sibling_slash_command_the_bundle_omits() {
        let bundled = stems(BUNDLED_COMMANDS);
        let ours = repo_stems("commands", "md");
        assert!(
            ours.contains("review"),
            "the repo's command directory was not found, so this test would pass vacuously: {ours:?}"
        );
        for (stem, body) in BUNDLED_COMMANDS.iter().chain(BUNDLED_WORKFLOWS.iter()) {
            for named in &ours {
                assert!(
                    !names_command(body, named) || bundled.contains(named.as_str()),
                    "`{stem}` tells the reader to use `/{named}`, which \
                     `jkb commands install` does not write — add it to BUNDLED_COMMANDS"
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
