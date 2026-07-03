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
const BUNDLED_COMMANDS: &[(&str, &str)] = &[
    (
        "next-task",
        include_str!("../../../.claude/commands/next-task.md"),
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
const BUNDLED_WORKFLOWS: &[(&str, &str)] = &[(
    "task-swarm",
    include_str!("../../../.claude/workflows/task-swarm.js"),
)];

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

/// Write `set` into `dir`, naming each file `{prefix}{stem}.{ext}`. Returns the installed
/// file names.
fn write_set(dir: &Path, prefix: &str, ext: &str, set: &[(&str, &str)]) -> Result<Vec<String>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut names = Vec::new();
    for (stem, body) in set {
        let name = format!("{prefix}{stem}.{ext}");
        let path = dir.join(&name);
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
        names.push(name);
    }
    Ok(names)
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
    write_set(&base.join("commands"), CMD_PREFIX, "md", BUNDLED_COMMANDS)?;
    write_set(&base.join("workflows"), "", "js", BUNDLED_WORKFLOWS)?;
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
