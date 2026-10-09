//! `jkb design export` and `jkb design source` (design D55.5–6): the file half of `docs/` generated
//! from designs.
//!
//! The ops are pure database work (`jkb_api`'s rule), so everything that touches the checkout is
//! here: resolving paths against the repository root, writing a design's render to its doc target,
//! hashing a source, and the drift check `scripts/check.sh` runs. What a generated file *is* — its
//! header and its text — is `jkb_core::design::export`'s alone; this side writes and compares the
//! bytes the op answers and never renders anything itself.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use jkb_api::designs::{Design, Export, Source};
use jkb_api::{Request, Response};
use jkb_core::design::export::{generated_from, DOCS_DIR};

use crate::ops_cli::{unexpected, Ops};

/// The checkout's root, and the current directory, both canonical so one strips from the other.
fn checkout() -> Result<(PathBuf, PathBuf)> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let root = crate::gitrepo::root(&cwd)?
        .context("not inside a git work tree — `jkb design export` writes into a checkout")?;
    let canon = |p: &Path| {
        p.canonicalize()
            .with_context(|| format!("resolving {}", p.display()))
    };
    Ok((canon(&root)?, canon(&cwd)?))
}

/// `arg` (relative to `cwd`, or absolute) as a `/`-separated path relative to `root`.
fn repo_relative(root: &Path, cwd: &Path, arg: &str) -> Result<String> {
    let mut path = PathBuf::new();
    for c in cwd.join(arg).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                path.pop();
            }
            other => path.push(other),
        }
    }
    let rel = path
        .strip_prefix(root)
        .map_err(|_| anyhow::anyhow!("`{arg}` is outside the repository at {}", root.display()))?;
    let parts = rel
        .components()
        .map(|c| {
            c.as_os_str()
                .to_str()
                .with_context(|| format!("`{arg}` is not UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    if parts.is_empty() {
        bail!("`{arg}` is the repository root, not a file");
    }
    Ok(parts.join("/"))
}

fn exports(ops: &Ops<'_>, uid: Option<String>, repo: Option<String>) -> Result<Vec<Export>> {
    match ops.call(Request::DesignExport { uid, repo })? {
        Response::DesignExports { exports } => Ok(exports),
        other => unexpected("design.export", &other),
    }
}

fn meta_answer(ops: &Ops<'_>, op: &str, request: Request) -> Result<Design> {
    match ops.call(request)? {
        Response::DesignMeta { design } => Ok(design),
        other => unexpected(op, &other),
    }
}

/// Write one export to its target under `root`; `true` when the file changed.
fn write(root: &Path, e: &Export) -> Result<bool> {
    let target = e.doc_target.as_deref().with_context(|| {
        format!(
            "design {} has no doc target — name one with `jkb design export {} --to docs/<file>.md`",
            e.uid, e.uid
        )
    })?;
    let path = root.join(target);
    if std::fs::read(&path).is_ok_and(|now| now == e.text.as_bytes()) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    crate::atomic::write(&path, e.text.as_bytes())?;
    Ok(true)
}

/// `jkb design export <uid> [--to <path>]` and `jkb design export --all`.
fn export(
    ops: &Ops<'_>,
    uid: Option<String>,
    to: Option<&str>,
    repo: Option<String>,
) -> Result<()> {
    let (root, cwd) = checkout()?;
    if let (Some(uid), Some(to)) = (&uid, to) {
        let path = repo_relative(&root, &cwd, to)?;
        meta_answer(
            ops,
            "design.target",
            Request::DesignTarget {
                uid: uid.clone(),
                path,
            },
        )?;
    }
    let repo = match uid {
        Some(_) => None,
        None => Some(crate::design_cli::repo_of(ops, repo)?),
    };
    let all = uid.is_none();
    let list = exports(ops, uid, repo)?;
    let mut done = Vec::new();
    for e in &list {
        let changed = write(&root, e)?;
        done.push(serde_json::json!({
            "uid": e.uid,
            "doc_target": e.doc_target,
            "version": e.version,
            "changed": changed,
        }));
        if !ops.json {
            let how = if changed { "wrote" } else { "unchanged" };
            println!(
                "{how:<9} {}  ← {} (version {})",
                e.doc_target.as_deref().unwrap_or_default(),
                e.uid,
                e.version
            );
        }
    }
    if ops.json {
        println!("{}", serde_json::to_string_pretty(&done)?);
    } else if all && list.is_empty() {
        println!("(no designs with a doc target — give one with `jkb design export <uid> --to`)");
    }
    Ok(())
}

/// Every regular file under `dir`, recursively, sorted. Symlinks are not followed: a link is not a
/// file this checkout generated.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            files_under(&entry.path(), out)?;
        } else if kind.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// What is wrong with one generated file, or `None` when it is its design's render.
fn drift(ops: &Ops<'_>, rel: &str, text: &str, uid: &str) -> Option<String> {
    let rendered = match exports(ops, Some(uid.to_owned()), None) {
        Ok(mut list) if list.len() == 1 => list.remove(0),
        Ok(_) => {
            return Some(format!(
                "{rel}: design.export answered no single design {uid}"
            ))
        }
        Err(e) => {
            return Some(format!(
                "{rel}: generated from design {uid}, which cannot be rendered here: {e:#}"
            ))
        }
    };
    if rendered.doc_target.as_deref() != Some(rel) {
        return Some(format!(
            "{rel}: carries design {uid}'s header, but that design exports to {} — delete the \
             stale file, or point the design here with `jkb design export {uid} --to {rel}`",
            rendered
                .doc_target
                .as_deref()
                .map_or_else(|| "no file".to_owned(), |t| format!("`{t}`"))
        ));
    }
    (rendered.text != text).then(|| {
        format!(
            "{rel}: differs from design {uid} at version {} — it was edited by hand, or the design \
             moved on since it was exported. Make the change in the design (`jkb design edit`), \
             then `jkb design export {uid}`",
            rendered.version
        )
    })
}

/// `jkb design export --check`: every `docs/` file whose first line is the generated header must be
/// exactly its design's render now. Files without the header are hand-written and not checked.
fn check(ops: &Ops<'_>) -> Result<()> {
    let (root, _) = checkout()?;
    let docs = root.join(DOCS_DIR);
    let mut files = Vec::new();
    if docs.is_dir() {
        files_under(&docs, &mut files)?;
    }
    files.sort();
    let mut checked = Vec::new();
    let mut problems = Vec::new();
    for path in files {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let rel = repo_relative(&root, &root, &path.to_string_lossy())?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue; // not text, so not a generated file
        };
        let Some(uid) = generated_from(text) else {
            continue;
        };
        checked.push(rel.clone());
        if let Some(problem) = drift(ops, &rel, text, uid) {
            problems.push(problem);
        }
    }
    if ops.json {
        let report = serde_json::json!({ "checked": checked, "drift": problems });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for p in &problems {
            eprintln!("drift: {p}");
        }
    }
    if !problems.is_empty() {
        bail!(
            "{} of {} generated docs/ file(s) differ from their designs",
            problems.len(),
            checked.len()
        );
    }
    if !ops.json {
        println!(
            "{} generated docs/ file(s) match their designs",
            checked.len()
        );
    }
    Ok(())
}

/// `jkb design export …`.
///
/// # Errors
/// The op's refusal, a path outside the checkout, an unwritable target, or drift under `--check`.
pub(crate) fn run(
    ops: &Ops<'_>,
    uid: Option<String>,
    to: Option<&str>,
    repo: Option<String>,
    check_only: bool,
) -> Result<()> {
    if check_only {
        check(ops)
    } else {
        export(ops, uid, to, repo)
    }
}

/// `jkb design source <uid> <path>…`: record each file with the blake3 of its content now.
///
/// # Errors
/// An unreadable file, one outside the checkout, or the op's refusal.
pub(crate) fn source(ops: &Ops<'_>, uid: String, paths: &[String]) -> Result<()> {
    let (root, cwd) = checkout()?;
    let sources = paths
        .iter()
        .map(|p| {
            let path = repo_relative(&root, &cwd, p)?;
            let bytes =
                std::fs::read(root.join(&path)).with_context(|| format!("reading `{p}`"))?;
            Ok(Source {
                path,
                blake3: jkb_core::blob::hash_bytes(&bytes),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let design = meta_answer(
        ops,
        "design.sources",
        Request::DesignSources { uid, sources },
    )?;
    if ops.json {
        println!("{}", serde_json::to_string_pretty(&design)?);
    } else {
        println!("{}  {}", design.uid, design.title);
        for s in &design.sources {
            println!("  source {}  blake3={}", s.path, s.blake3);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::repo_relative;
    use std::path::Path;

    #[test]
    fn a_path_is_resolved_against_the_cwd_and_made_relative_to_the_root() {
        let root = Path::new("/r");
        assert_eq!(
            repo_relative(root, Path::new("/r"), "docs/a.md").unwrap(),
            "docs/a.md"
        );
        assert_eq!(
            repo_relative(root, Path::new("/r/crates"), "../docs/./a.md").unwrap(),
            "docs/a.md"
        );
        assert_eq!(
            repo_relative(root, Path::new("/elsewhere"), "/r/docs/a.md").unwrap(),
            "docs/a.md"
        );
        assert!(repo_relative(root, Path::new("/r"), "../x.md").is_err());
        assert!(repo_relative(root, Path::new("/r"), ".").is_err());
    }
}
