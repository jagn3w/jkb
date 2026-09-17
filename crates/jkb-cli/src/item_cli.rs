//! `jkb item`, `stat`, `related`, `blob` and `history` as clients of the ops (tasks S6.4 stage 5): the
//! same answer on the host and through `jkb serve`. The renderings are the ones these commands printed
//! before they were ported.

use std::path::Path;

use anyhow::{Context as _, Result};
use jkb_api::items::{BlobRow, ItemInfo, RelatedRow, Version};
use jkb_api::{Request, Response};

use crate::ops_cli::{unexpected, Ops};
use crate::{first_line, BlobCmd, DirArg, ItemCmd};

fn item(ops: &Ops<'_>, uid: &str, preview: Option<usize>) -> Result<ItemInfo> {
    match ops.call(Request::ItemShow {
        uid: uid.to_owned(),
        preview,
    })? {
        Response::Item { item } => Ok(*item),
        other => unexpected("item.show", &other),
    }
}

/// Human-readable header lines for `item show` and `stat`.
fn print_item_detail(i: &ItemInfo) {
    println!("uid:       {}", i.uid);
    println!("kind:      {}", i.kind);
    if let Some(s) = &i.status {
        println!("status:    {s}");
    }
    if let Some(r) = &i.resolution {
        println!("resolution: {r}");
    }
    if let Some(ns) = &i.namespace {
        println!("namespace: {ns}");
    }
    if let Some(m) = &i.mime {
        println!("mime:      {m}");
    }
    if let Some(b) = &i.binding {
        println!("binding:   {b}");
    }
    if !i.tags.is_empty() {
        let t: Vec<String> = i
            .tags
            .iter()
            .map(|t| format!("{}={}", t.facet, t.value))
            .collect();
        println!("tags:      {}", t.join(", "));
    }
    println!("updated:   {}", i.updated_at);
}

fn tags_json(i: &ItemInfo) -> Vec<serde_json::Value> {
    i.tags
        .iter()
        .map(|t| serde_json::json!({"facet": t.facet, "value": t.value}))
        .collect()
}

/// `jkb stat <uid>`.
///
/// # Errors
/// The op's refusal.
pub(crate) fn stat(ops: &Ops<'_>, uid: &str) -> Result<()> {
    let i = item(ops, uid, Some(0))?;
    if ops.json {
        println!(
            "{}",
            serde_json::json!({
                "uid": i.uid, "kind": i.kind, "status": i.status,
                "resolution": i.resolution,
                "priority": i.priority, "due": i.due, "mime": i.mime,
                "namespace": i.namespace, "binding": i.binding, "content_chars": i.content_chars,
                "tags": tags_json(&i),
                "created_at": i.created_at, "updated_at": i.updated_at,
            })
        );
    } else {
        print_item_detail(&i);
        println!("content:   {} chars", i.content_chars);
    }
    Ok(())
}

/// `jkb item …`.
///
/// # Errors
/// The op's refusal, or unreadable input.
pub(crate) fn run(ops: &Ops<'_>, cmd: ItemCmd) -> Result<()> {
    match cmd {
        ItemCmd::Show { uid, preview } => show(ops, &uid, preview),
        ItemCmd::Rm { uid, force } => rm(ops, &uid, force),
        ItemCmd::Edit {
            uid,
            text,
            stdin,
            append,
        } => edit(ops, &uid, &text, stdin, append),
    }
}

/// `jkb item show <uid>` — kind-aware details and a bounded preview of the content.
fn show(ops: &Ops<'_>, uid: &str, preview: Option<usize>) -> Result<()> {
    let i = item(ops, uid, preview)?;
    if ops.json {
        let v = serde_json::json!({
            "uid": i.uid,
            "kind": i.kind,
            "status": i.status,
            "resolution": i.resolution,
            "priority": i.priority,
            "due": i.due,
            "mime": i.mime,
            "binding": i.binding,
            "namespace": i.namespace,
            "content_chars": i.content_chars,
            "content_hash": i.content_hash,
            "created_at": i.created_at,
            "updated_at": i.updated_at,
            "tags": tags_json(&i),
            "preview": i.preview,
            "preview_truncated": i.preview_truncated,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print_item_detail(&i);
        println!(
            "content:   {} chars{}",
            i.content_chars,
            if i.preview_truncated {
                " (preview truncated)"
            } else {
                ""
            }
        );
        if !i.preview.is_empty() {
            println!("\n{}", i.preview);
        }
    }
    Ok(())
}

fn rm(ops: &Ops<'_>, uid: &str, force: bool) -> Result<()> {
    // No vector sweep: the `vec_items_<dim>_gc` trigger (D42.2) removes the vector with the item.
    let removed = match ops.call(Request::ItemRm {
        uid: uid.to_owned(),
        force,
    })? {
        Response::ItemRemoved { removed } => removed,
        other => return unexpected("item.rm", &other),
    };
    if ops.json {
        println!(
            "{}",
            serde_json::json!({
                "uid": removed.uid,
                "kind": removed.kind,
                "placements": removed.placements,
                "edges": removed.edges,
                "tags": removed.tags,
            })
        );
    } else {
        println!(
            "removed {} [{}] — {} placement(s), {} edge(s), {} tag(s)",
            removed.uid, removed.kind, removed.placements, removed.edges, removed.tags
        );
        println!("`jkb undo` restores it, including its edges.");
    }
    Ok(())
}

/// `jkb item edit` — through `task.edit`, whose edit rule (`item::edit_content`) is the one for any
/// item: an item in a tasks.md appends with one newline and refuses a line that would end its body.
fn edit(ops: &Ops<'_>, uid: &str, text: &[String], stdin: bool, append: bool) -> Result<()> {
    let new_text = if stdin {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("reading item content from stdin")?;
        buf.trim_end().to_owned()
    } else if text.is_empty() {
        anyhow::bail!("provide new content as arguments, or pass --stdin");
    } else {
        text.join(" ")
    };
    anyhow::ensure!(
        uid.contains(':'),
        "no item with uid `{uid}` — `item edit` takes an item's full uid"
    );
    let file_backed = match ops.call(Request::TaskEdit {
        uid: uid.to_owned(),
        text: new_text,
        append,
    })? {
        Response::Edited { file_backed } => file_backed,
        other => return unexpected("task.edit", &other),
    };
    crate::report(ops.json, uid, if append { "appended" } else { "edited" });
    if (file_backed || uid.starts_with("file://")) && !ops.json {
        eprintln!(
            "note: this is a file-backed item; its file follows at the next sync (`jkb sync` on the \
             host, or its watcher)."
        );
    }
    Ok(())
}

/// `jkb related <uid>`.
///
/// # Errors
/// The op's refusal.
pub(crate) fn related(
    ops: &Ops<'_>,
    uid: &str,
    edges: &[String],
    depth: usize,
    direction: DirArg,
) -> Result<()> {
    let rows = match ops.call(Request::KbRelated {
        uid: uid.to_owned(),
        edges: edges.to_vec(),
        depth,
        direction: match direction {
            DirArg::Out => jkb_api::items::Direction::Out,
            DirArg::In => jkb_api::items::Direction::In,
            DirArg::Both => jkb_api::items::Direction::Both,
        },
    })? {
        Response::Related { rows, .. } => rows,
        other => return unexpected("kb.related", &other),
    };
    if ops.json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r: &RelatedRow| {
                serde_json::json!({
                    "uid": r.uid,
                    "kind": r.kind,
                    "status": r.status,
                    "resolution": r.resolution,
                    "depth": r.depth,
                    "via": r.via,
                    "direction": r.direction,
                    "snippet": r.snippet.as_deref().map(first_line),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if rows.is_empty() {
        println!("(no related items)");
    } else {
        for r in &rows {
            let arrow = if r.direction == "in" { "<-" } else { "->" };
            println!(
                "{:>2}  {arrow} {:<26} [{}]{} — {}",
                r.depth,
                format!("{} {}", r.via, r.uid),
                r.kind,
                r.resolution
                    .as_deref()
                    .map(|x| format!(" ({x})"))
                    .unwrap_or_default(),
                r.snippet.as_deref().map(first_line).unwrap_or_default(),
            );
        }
    }
    Ok(())
}

/// `jkb blob …`.
///
/// # Errors
/// The op's refusal.
pub(crate) fn blob(ops: &Ops<'_>, cmd: BlobCmd) -> Result<()> {
    match cmd {
        BlobCmd::Ls { contains, limit } => blob_ls(ops, contains, limit),
        BlobCmd::Cat { hash } => blob_cat(ops, &hash),
    }
}

fn blob_ls(ops: &Ops<'_>, contains: Option<String>, limit: usize) -> Result<()> {
    let blobs = match ops.call(Request::KbBlobs { contains, limit })? {
        Response::Blobs { blobs, .. } => blobs,
        other => return unexpected("kb.blobs", &other),
    };
    if ops.json {
        let arr: Vec<serde_json::Value> = blobs
            .iter()
            .map(|b: &BlobRow| {
                serde_json::json!({
                    "hash": b.hash, "size": b.size, "mime": b.mime, "created_at": b.created_at,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if blobs.is_empty() {
        println!("(no matching blobs)");
    } else {
        for b in &blobs {
            println!(
                "{}  {:>9}  {}",
                &b.hash[..16.min(b.hash.len())],
                b.size,
                b.created_at
            );
        }
    }
    Ok(())
}

/// `jkb blob cat <hash>` — a text blob to stdout, by a unique hash prefix (which can never print the
/// wrong version).
fn blob_cat(ops: &Ops<'_>, prefix: &str) -> Result<()> {
    use std::io::Write as _;
    // The host reads the bytes in-process, whatever they are: the archive's recovery path is
    // `jkb blob cat <hash> > file`, for a PDF as much as for a text file.
    if let Some(db) = ops.db {
        let prefix = prefix.to_owned();
        let (_, bytes) = db
            .read_with(move |c| jkb_api::items::blob_bytes(c, &prefix))
            .map_err(|e| anyhow::anyhow!(e.message))?;
        std::io::stdout().write_all(&bytes)?;
        return Ok(());
    }
    match ops.call(Request::KbBlob {
        prefix: prefix.to_owned(),
    })? {
        Response::Blob { text, .. } => {
            std::io::stdout().write_all(text.as_bytes())?;
            Ok(())
        }
        other => unexpected("kb.blob", &other),
    }
}

/// Resolve a path the way the sync journal's uris were built: canonicalized.
///
/// `canonicalize` needs the file to exist, which is precisely what `jkb history` is often asked
/// about, and plain absolutisation resolves no symlinks — so on macOS a deleted file under `/tmp` or
/// `/var` produced a uri the journal never wrote. Canonicalizing the deepest ancestor that DOES exist
/// (normally the parent) and rejoining the rest gets both.
fn resolve_for_journal(path: &Path) -> std::path::PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut rest = Vec::new();
    let mut cur = abs.as_path();
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            rest.push(name.to_owned());
        }
        if let Ok(real) = std::fs::canonicalize(parent) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        cur = parent;
    }
    abs
}

/// `jkb history <path>` — every synced version of a file, newest first.
///
/// # Errors
/// The op's refusal.
pub(crate) fn history(ops: &Ops<'_>, path: &str) -> Result<()> {
    // A `file://` uri is taken as its path; a bare path is resolved here, where the file is, and the op
    // re-roots it from this process's home to the host's.
    let local = path.strip_prefix("file://").unwrap_or(path);
    let resolved = resolve_for_journal(Path::new(local));
    let resolved = resolved
        .to_str()
        .with_context(|| format!("{} is not UTF-8", resolved.display()))?
        .to_owned();
    let home = std::env::var("HOME").unwrap_or_default();
    let (uri, versions) = match ops.call(Request::KbHistory {
        path: resolved,
        home,
    })? {
        Response::Versions { uri, versions, .. } => (uri, versions),
        other => return unexpected("kb.history", &other),
    };
    if ops.json {
        let arr: Vec<serde_json::Value> = versions
            .iter()
            .map(
                |v: &Version| serde_json::json!({ "ts": v.ts, "blob": v.blob, "status": v.status }),
            )
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if versions.is_empty() {
        println!(
            "(no recorded history for {uri})\n\
             Versions synced before this build did not journal their blob hash — search the \
             archive instead: jkb blob ls --contains \"<a line you remember>\""
        );
    } else {
        for v in &versions {
            println!(
                "{}  {}  [{}]",
                v.ts,
                &v.blob[..16.min(v.blob.len())],
                v.status
            );
        }
        println!("\nRead one with: jkb blob cat <hash>");
    }
    Ok(())
}
