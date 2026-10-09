//! `jkb design export` and `jkb design source` (design D55.5–6): the file half of `docs/` generated
//! from designs.
//!
//! The ops are pure database work (`jkb_api`'s rule), so everything that touches the checkout is
//! here: resolving paths against the repository root, writing a design's render to its doc target,
//! hashing a source, and the drift checks. What a generated file *is* — its header, the body hash
//! it records, its text — is `jkb_core::design::export`'s alone; this side writes and compares the
//! bytes and never renders anything itself.
//!
//! Two checks, deliberately apart (D55.6, amended). `--check` opens no database: each generated
//! file's header records the blake3 of its body, so a hand edit is seen from the file alone, the
//! same in `scripts/check.sh`, CI and a fresh clone. `--check --against-db` asks the live designs
//! too — has one moved on since its export, does a design's doc target have no file — which only a
//! machine holding the designs can answer.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use jkb_api::designs::{Design, Export, Source};
use jkb_api::{Request, Response};
use jkb_core::design::export::{self as gen, Generated, DOCS_DIR};

use crate::ops_cli::{unexpected, Ops};

/// The checkout `dir` is in: the nearest directory at or above it holding a `.git` entry — a
/// directory in a main checkout, a `gitdir:` file in a linked worktree.
///
/// Found by looking, never by running git. `gitrepo::root` spawns jkb's audited git, which refuses
/// a repository whose own config carries keys outside its allowlist — and `actions/checkout`
/// writes `http.https://github.com/.extraheader` into every CI checkout, as a developer's
/// `credential.helper`, `http.*` or `includeIf` would locally. The drift check failed every CI push
/// that way (review round 2). Nothing here needs git: only where the checkout's files are.
fn root_of(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|d| std::fs::symlink_metadata(d.join(".git")).is_ok())
        .map(Path::to_path_buf)
}

/// The checkout's root, and the current directory, both canonical so one strips from the other.
fn checkout() -> Result<(PathBuf, PathBuf)> {
    let cwd = std::env::current_dir()
        .context("reading the current directory")?
        .canonicalize()
        .context("resolving the current directory")?;
    let root = root_of(&cwd)
        .context("not inside a git checkout — `jkb design export` reads and writes one")?;
    Ok((root, cwd))
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

/// Refuse a design that is not of `here`, the repo this checkout is: its doc target is a path in
/// its own repo's checkout, and written into another one it would be a stray file that names it.
fn same_repo(e: &Export, here: &str) -> Result<()> {
    if e.repo.as_deref() == Some(here) {
        return Ok(());
    }
    bail!(
        "design {} is of repo {}, but this checkout is repo {here} — export it from its own \
         repo's checkout",
        e.uid,
        e.repo.as_deref().unwrap_or("(none)")
    )
}

/// The repo the checkout at `root` is: the mount covering `root` itself, since `root` is where the
/// files are written. `--repo` and the current directory's mount may only agree with it — neither
/// can point an export at another repo's design.
fn checkout_repo(ops: &Ops<'_>, root: &Path, cwd: &Path, asked: Option<String>) -> Result<String> {
    let here = ops.ambient_repo_at(root)?.with_context(|| {
        format!(
            "the checkout at {} is in no mounted repo, so which repo's designs belong in it is not \
             known — mount it (`jkb mount create repos/<repo> {}`)",
            root.display(),
            root.display()
        )
    })?;
    if let Some(there) = ops.ambient_repo_at(cwd)?.filter(|r| *r != here) {
        bail!("the current directory is in repo {there}, but its checkout is repo {here}");
    }
    if let Some(asked) = asked.filter(|r| *r != here) {
        bail!("--repo {asked}, but this checkout is repo {here}");
    }
    Ok(here)
}

/// `jkb design export <uid> [--to <path>]` and `jkb design export --all`. Either way only into the
/// checkout of the design's own repo ([`checkout_repo`]).
fn export(
    ops: &Ops<'_>,
    uid: Option<String>,
    to: Option<&str>,
    repo: Option<String>,
) -> Result<()> {
    let (root, cwd) = checkout()?;
    let here = checkout_repo(ops, &root, &cwd, repo)?;
    if let Some(uid) = &uid {
        for e in exports(ops, Some(uid.clone()), None)? {
            same_repo(&e, &here)?;
        }
        if let Some(to) = to {
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
    }
    let all = uid.is_none();
    let scope = all.then(|| here.clone());
    let list = exports(ops, uid, scope)?;
    for e in &list {
        same_repo(e, &here)?;
    }
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

/// Every generated `docs/` file under `root`: its repo-relative path and what its first line says.
/// Files that are not UTF-8 are not text, so not generated; hand-written ones are left out.
fn generated_files(root: &Path) -> Result<Vec<(String, Option<String>)>> {
    let docs = root.join(DOCS_DIR);
    let mut files = Vec::new();
    if docs.is_dir() {
        files_under(&docs, &mut files)?;
    }
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let rel = repo_relative(root, root, &path.to_string_lossy())?;
        // Classified by the first line's bytes, so a generated file re-saved in another encoding is
        // still a generated file — and reported, not skipped as "not text".
        let first = bytes.split(|&b| b == b'\n').next().unwrap_or_default();
        if matches!(gen::parse(&String::from_utf8_lossy(first)), Generated::Hand) {
            continue;
        }
        out.push((rel, String::from_utf8(bytes).ok()));
    }
    Ok(out)
}

/// What is wrong with one generated file on its own, with no database: a header that does not
/// read, or a body that is no longer the one its header hashed.
fn tampered(rel: &str, text: Option<&str>) -> Option<String> {
    let Some(text) = text else {
        return Some(format!(
            "{rel}: carries the generated header but is not UTF-8 any more (re-saved in another \
             encoding?) — re-export it from its design (`jkb design export <design>`)"
        ));
    };
    match gen::parse(text) {
        Generated::Hand => None,
        Generated::Malformed(why) => Some(format!(
            "{rel}: {why} — re-export it from its design (`jkb design export <design>`), or delete \
             the first line if the file is meant to be hand-written"
        )),
        g @ Generated::File { uid, .. } => (!g.intact()).then(|| {
            format!(
                "{rel}: edited by hand since it was exported from design {uid} (its body no longer \
                 matches the blake3 in its header). Make the change in the design (`jkb design \
                 edit`), then `jkb design export {uid}`"
            )
        }),
    }
}

/// Report a check's problems and fail when there are any.
fn report(json: bool, what: &str, checked: &[String], problems: &[String]) -> Result<()> {
    if json {
        let report = serde_json::json!({ "checked": checked, "drift": problems });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for p in problems {
            eprintln!("drift: {p}");
        }
    }
    if !problems.is_empty() {
        bail!(
            "{} problem(s) among {} generated docs/ file(s)",
            problems.len(),
            checked.len()
        );
    }
    if !json {
        println!("{} generated docs/ file(s) {what}", checked.len());
    }
    Ok(())
}

/// `jkb design export --check`: every `docs/` file carrying the generated header must still be the
/// body that header hashed. Opens no database and reaches no daemon (dispatched in `main` before
/// either), so it is the same gate in `scripts/check.sh`, CI and a fresh clone.
///
/// What it cannot see, by construction: whether a design moved on since its export, and a design
/// whose doc target has no file at all — both are facts about the database. `--against-db` asks
/// them.
///
/// # Errors
/// Not inside a git checkout, an unreadable file, `--repo` (which names designs, and so a
/// database), or a tampered generated file.
pub(crate) fn check_files(repo: Option<&str>, json: bool) -> Result<()> {
    if repo.is_some() {
        bail!(
            "`--repo` names designs, which `--check` alone never reads — add `--against-db` to \
             compare with them"
        );
    }
    let (root, _) = checkout()?;
    let files = generated_files(&root)?;
    let problems: Vec<String> = files
        .iter()
        .filter_map(|(rel, text)| tampered(rel, text.as_deref()))
        .collect();
    let checked: Vec<String> = files.into_iter().map(|(rel, _)| rel).collect();
    report(
        json,
        "match the bodies their headers hash",
        &checked,
        &problems,
    )
}

/// What is wrong with one generated file against its design now, or `None` when its body is the
/// design's render. Only the body is compared: the header's version token moves with every edit,
/// a PROPOSED one included, and is information for the reader.
fn drift(ops: &Ops<'_>, here: &str, rel: &str, text: &str, uid: &str) -> Option<String> {
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
    if rendered.repo.as_deref() != Some(here) {
        return Some(format!(
            "{rel}: generated from design {uid} of repo {}, but this checkout is repo {here} — a \
             stray export; delete it",
            rendered.repo.as_deref().unwrap_or("(none)")
        ));
    }
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
    let body = |t: &str| match gen::parse(t) {
        Generated::File { body, .. } => Some(body.to_owned()),
        Generated::Hand | Generated::Malformed(_) => None,
    };
    (body(&rendered.text) != body(text)).then(|| {
        format!(
            "{rel}: differs from design {uid} at version {} — the design moved on since it was \
             exported. Re-export it with `jkb design export {uid}`",
            rendered.version
        )
    })
}

/// `jkb design export --check --against-db`: everything `--check` asks, then each generated file
/// against its design's render now, and every design of the repo whose doc target has no file.
fn check_against_db(ops: &Ops<'_>, repo: Option<String>) -> Result<()> {
    let (root, cwd) = checkout()?;
    let repo = checkout_repo(ops, &root, &cwd, repo)?;
    let files = generated_files(&root)?;
    let mut problems = Vec::new();
    for (rel, text) in &files {
        if let Some(p) = tampered(rel, text.as_deref()) {
            problems.push(p);
        } else if let Some(text) = text {
            let Some(uid) = gen::generated_from(text) else {
                continue;
            };
            problems.extend(drift(ops, &repo, rel, text, uid));
        }
    }
    for e in exports(ops, None, Some(repo))? {
        let Some(target) = e.doc_target.as_deref() else {
            continue;
        };
        if !files.iter().any(|(rel, _)| rel == target) {
            let what = if root.join(target).exists() {
                "is there but does not carry the generated header"
            } else {
                "does not exist"
            };
            problems.push(format!(
                "{target}: design {} exports here, but the file {what} — `jkb design export {}`",
                e.uid, e.uid
            ));
        }
    }
    let checked: Vec<String> = files.into_iter().map(|(rel, _)| rel).collect();
    report(ops.json, "match their designs", &checked, &problems)
}

/// The arguments of `jkb design export`.
pub(crate) struct ExportArgs {
    pub uid: Option<String>,
    pub to: Option<String>,
    pub repo: Option<String>,
    pub check: bool,
    pub against_db: bool,
}

/// `jkb design export …`. A bare `--check` never gets here: `main` runs it, database-free, before
/// anything opens one ([`check_files`]); it is answered the same way here, for a caller that did.
///
/// # Errors
/// The op's refusal, a path outside the checkout, an unwritable target, or drift under `--check`.
pub(crate) fn run(ops: &Ops<'_>, args: ExportArgs) -> Result<()> {
    let ExportArgs {
        uid,
        to,
        repo,
        check,
        against_db,
    } = args;
    match (check, against_db) {
        (true, true) => check_against_db(ops, repo),
        (true, false) => check_files(repo.as_deref(), ops.json),
        // `--all` is the absence of a uid, which clap enforces.
        (false, _) => export(ops, uid, to.as_deref(), repo),
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
    use super::{repo_relative, root_of};
    use std::path::Path;

    /// The checkout is the nearest directory with a `.git` entry, a directory or a linked
    /// worktree's `gitdir:` file, found without running git.
    #[test]
    fn the_checkout_is_the_nearest_directory_with_a_git_entry() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        std::fs::create_dir_all(main.join("docs/deep")).unwrap();
        assert_eq!(root_of(&main.join("docs/deep")), Some(main.clone()));
        let linked = main.join("work/linked");
        std::fs::create_dir_all(linked.join("docs")).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: ../../.git/worktrees/linked\n").unwrap();
        assert_eq!(root_of(&linked.join("docs")), Some(linked));
        let bare = dir.path().join("none");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(root_of(&bare).is_none_or(|r| !r.starts_with(dir.path())));
    }

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
