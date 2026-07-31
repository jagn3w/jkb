//! The reconciliation engine: one-shot `sync` over a `file://` mount.
//!
//! Each bound file is reconciled independently inside one `write_txn` (atomic +
//! audited). Direction is decided from the **journal** (`sync_state`, one row per
//! file uri) using a **three-way** comparison against the last-synced **base** bytes:
//! `disk` vs `base` and `kb` (the current KB rendered through the serializer) vs
//! `base`, never `disk` vs `kb`. This is what lets a multi-item file distinguish "the
//! KB changed task A" from "the disk changed task B" and auto-merge disjoint edits
//! instead of declaring a whole-file conflict (design D25).
//!
//! A file's items are gathered by the bindings `file://<path>` and
//! `file://<path>#<local_id>` (design D24); the serializer maps those bytes to a
//! [`SyncDoc`] and back. On a `tasks` parse failure the file is **quarantined** — its
//! last-good items are left intact, the failing bytes are stashed, and the journal is
//! flagged `needs_attention` — rather than overwritten.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobMatcher};
use serde_json::json;
use walkdir::WalkDir;

use jkb_core::item::NewItem;
use jkb_core::{
    binding, blob, edge, item, mount, ns, placement, sync_state, tag, task, Db, WriteMeta,
};
use jkb_types::{
    EdgeType, Error as TypeError, ItemId, NamespaceId, PlacementRole, SyncMode, TaskStatus,
};
use rusqlite::{Connection, OptionalExtension};

use crate::serializers::{resolve, SyncBlock, SyncDoc, SyncItem, SyncSection, SyncSerializer};
use crate::{Error, Result};

/// What happened when reconciling one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A new file on disk was imported for the first time (items created).
    Created,
    /// Changed disk content was imported into the KB (disk → KB).
    Imported,
    /// A changed item was rendered back to its file (KB → disk).
    Exported,
    /// Disjoint disk and KB edits were merged three-way (both sides reconciled).
    Merged,
    /// Both sides changed the same unit and the policy is `manual`: nothing modified.
    Conflict,
    /// The file failed to parse and was quarantined (last-good items kept, bytes stashed).
    Quarantined,
    /// Neither side changed since the last sync.
    UpToDate,
    /// The mount's `sync_mode` does not permit the needed direction.
    Skipped,
}

/// The result of reconciling one file.
#[derive(Debug, Clone)]
pub struct FileResult {
    /// The absolute file path.
    pub path: PathBuf,
    /// What happened.
    pub outcome: Outcome,
}

/// The outcome of a one-shot [`sync`] over a mount.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    /// One entry per reconciled file.
    pub results: Vec<FileResult>,
}

impl SyncReport {
    /// The paths that reported a [`Outcome::Conflict`].
    #[must_use]
    pub fn conflicts(&self) -> Vec<&Path> {
        self.paths_with(Outcome::Conflict)
    }

    /// The paths that were [`Outcome::Quarantined`].
    #[must_use]
    pub fn quarantined(&self) -> Vec<&Path> {
        self.paths_with(Outcome::Quarantined)
    }

    /// The paths that were three-way [`Outcome::Merged`].
    #[must_use]
    pub fn merged(&self) -> Vec<&Path> {
        self.paths_with(Outcome::Merged)
    }

    fn paths_with(&self, outcome: Outcome) -> Vec<&Path> {
        self.results
            .iter()
            .filter(|r| r.outcome == outcome)
            .map(|r| r.path.as_path())
            .collect()
    }

    /// How many files reported `outcome`.
    #[must_use]
    pub fn count(&self, outcome: Outcome) -> usize {
        self.results.iter().filter(|r| r.outcome == outcome).count()
    }
}

/// The mount configuration needed to reconcile a file, owned so it can move into the
/// writer-thread closure.
#[derive(Debug, Clone)]
struct Ctx {
    mount_ns: String,
    dir: PathBuf,
    sync_mode: String,
    conflict_policy: String,
    serializer: String,
}

impl Ctx {
    fn imports(&self) -> bool {
        self.sync_mode == "import" || self.sync_mode == "bidirectional"
    }
    fn exports(&self) -> bool {
        self.sync_mode == "export" || self.sync_mode == "bidirectional"
    }
}

/// Run a one-shot sync over the mount at `mount_ns`.
///
/// # Errors
/// Returns an error if there is no mount at `mount_ns`, its backing uri is not a
/// `file://` path, its serializer is unknown, or a filesystem/database operation
/// fails. Conflicts and quarantines are reported in the [`SyncReport`], not as errors.
pub fn sync(db: &Db, mount_ns: &str) -> Result<SyncReport> {
    let ctx = load_ctx(db, mount_ns)?;
    let _ = resolve(&ctx.serializer)?; // fail fast on an unknown mount serializer

    let filter = Filter::build(&read_globs(db, mount_ns)?)?;
    let paths = discover(db, &ctx, &filter)?;
    reconcile_all(db, &ctx, paths)
}

/// Reconcile only the given `paths` (e.g. the paths named by filesystem-watch events),
/// deduplicated and scoped to the mount's directory and include/exclude globs.
///
/// # Errors
/// Same as [`sync`].
pub fn sync_paths(db: &Db, mount_ns: &str, paths: &[PathBuf]) -> Result<SyncReport> {
    let ctx = load_ctx(db, mount_ns)?;
    let _ = resolve(&ctx.serializer)?;

    let filter = Filter::build(&read_globs(db, mount_ns)?)?;
    let mut relevant: Vec<PathBuf> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for path in paths {
        if filter.accepts(&ctx.dir, path) && seen.insert(path.clone()) {
            relevant.push(path.clone());
        }
    }
    reconcile_all(db, &ctx, relevant)
}

/// Reconcile each path in its own audited transaction, collecting the outcomes.
fn reconcile_all(db: &Db, ctx: &Ctx, paths: Vec<PathBuf>) -> Result<SyncReport> {
    let mut results = Vec::with_capacity(paths.len());
    for path in paths {
        let ctx = ctx.clone();
        let p = path.clone();
        let outcome = db.write_txn_with::<Outcome, Error, _>("sync", move |conn, meta| {
            reconcile(conn, meta, &ctx, &p)
        })?;
        results.push(FileResult { path, outcome });
    }
    // Any tasks just imported from `repos/<repo>/…/tasks.md` get a `tasks/…` mirror so
    // `tasks/**` stays the complete task index. Only run when a file actually changed —
    // a pure-no-op reconcile must not open a write txn, or the watcher would re-fire on
    // its own commit and spin (the file-watch feedback loop).
    let imported = results.iter().any(|r| {
        matches!(
            r.outcome,
            Outcome::Created | Outcome::Imported | Outcome::Merged
        )
    });
    if imported {
        db.write_txn_with::<usize, Error, _>("sync", |conn, meta| {
            Ok(task::ensure_all_mirrors(conn, meta)?)
        })?;
    }
    Ok(SyncReport { results })
}

/// The absolute backing directory of the mount at `mount_ns` (for the watcher).
///
/// # Errors
/// Returns an error if there is no mount there or its backing uri is not `file://`.
pub fn backing_dir(db: &Db, mount_ns: &str) -> Result<PathBuf> {
    Ok(load_ctx(db, mount_ns)?.dir)
}

/// If `home_ns` — or an ancestor of it — is a `tasks`-serializer `file://` mount, return the
/// bare binding uri of that mount's root tasks file (`file://<backing_dir>/tasks.md`). A task
/// homed under such a mount can bind to `<that>#<local_id>` and round-trip via [`sync`]
/// (design D26.5). Returns `None` when no `tasks` mount covers the home namespace, so the
/// caller keeps the task `managed:`. The first mount encountered while walking up stops the
/// search: a non-`tasks` mount covering the home yields `None` rather than crossing it.
///
/// # Errors
/// Returns an error if a database read fails.
pub fn tasks_mount_file(db: &Db, home_ns: &str) -> Result<Option<String>> {
    let home = home_ns.to_owned();
    let uri = db.read(move |conn| {
        let mut cur = Some(home);
        while let Some(path) = cur {
            if let Some(ns_id) = ns::get(conn, &path)? {
                if let Some(m) = mount::get(conn, ns_id)? {
                    if m.serializer == "tasks" {
                        if let Some(dir) = m.backing_uri.strip_prefix("file://") {
                            let dir = dir.trim_end_matches('/');
                            return Ok(Some(format!("file://{dir}/tasks.md")));
                        }
                    }
                    return Ok(None);
                }
            }
            cur = path.rsplit_once('/').map(|(parent, _)| parent.to_owned());
        }
        Ok(None)
    })?;
    Ok(uri)
}

/// Load the mount configuration into an owned [`Ctx`].
fn load_ctx(db: &Db, mount_ns: &str) -> Result<Ctx> {
    let path = mount_ns.to_owned();
    db.read_with::<Ctx, Error, _>(move |conn| {
        let ns_id = ns::get(conn, &path)?
            .ok_or_else(|| Error::Types(TypeError::NotFound(format!("namespace `{path}`"))))?;
        let m = mount::get(conn, ns_id)?
            .ok_or_else(|| Error::Types(TypeError::NotFound(format!("mount at `{path}`"))))?;
        let dir = m.backing_uri.strip_prefix("file://").ok_or_else(|| {
            Error::Types(TypeError::Validation(format!(
                "mount `{path}` backing uri `{}` is not a file:// path",
                m.backing_uri
            )))
        })?;
        Ok(Ctx {
            mount_ns: path.clone(),
            dir: PathBuf::from(dir),
            sync_mode: m.sync_mode,
            conflict_policy: m.conflict_policy,
            serializer: m.serializer,
        })
    })
}

/// The mount's `(include_glob, exclude_glob)`.
fn read_globs(db: &Db, mount_ns: &str) -> Result<(Option<String>, Option<String>)> {
    let path = mount_ns.to_owned();
    let m = db.read(move |conn| {
        let ns_id = ns::get(conn, &path)?;
        match ns_id {
            Some(id) => mount::get(conn, id),
            None => Ok(None),
        }
    })?;
    Ok(m.map_or((None, None), |m| (m.include_glob, m.exclude_glob)))
}

/// The mount's compiled include/exclude globs.
struct Filter {
    include: Option<GlobMatcher>,
    exclude: Option<GlobMatcher>,
}

impl Filter {
    fn build(globs: &(Option<String>, Option<String>)) -> Result<Self> {
        let compile = |g: &Option<String>| -> Result<Option<GlobMatcher>> {
            Ok(g.as_ref()
                .map(|g| Glob::new(g))
                .transpose()?
                .map(|g| g.compile_matcher()))
        };
        Ok(Self {
            include: compile(&globs.0)?,
            exclude: compile(&globs.1)?,
        })
    }

    /// Whether an absolute `path` under `dir` is in scope.
    fn accepts(&self, dir: &Path, path: &Path) -> bool {
        if !path.starts_with(dir) {
            return false;
        }
        let rel = rel_str(dir, path);
        self.include.as_ref().is_none_or(|m| m.is_match(&rel))
            && !self.exclude.as_ref().is_some_and(|m| m.is_match(&rel))
    }

    /// Like [`Self::accepts`] but ignoring `include` — for already-bound files.
    fn accepts_bound(&self, dir: &Path, path: &Path) -> bool {
        path.starts_with(dir)
            && !self
                .exclude
                .as_ref()
                .is_some_and(|m| m.is_match(rel_str(dir, path)))
    }
}

/// The set of files to reconcile: those on disk matching the globs, unioned with the
/// backing files of items already bound under the mount. Binding uris may carry a
/// `#<local_id>` fragment (multi-item files), so they are stripped back to the file
/// path and deduplicated (design D24 — one file, many item bindings).
fn discover(db: &Db, ctx: &Ctx, filter: &Filter) -> Result<Vec<PathBuf>> {
    let mut set: BTreeSet<PathBuf> = BTreeSet::new();

    for entry in WalkDir::new(&ctx.dir)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if entry.file_type().is_file() && filter.accepts(&ctx.dir, entry.path()) {
            set.insert(entry.path().to_path_buf());
        }
    }

    let mount_ns = ctx.mount_ns.clone();
    let bound = db.read(move |conn| binding::synced_uris_under(conn, &mount_ns))?;
    for uri in bound {
        let Some(raw) = uri.strip_prefix("file://") else {
            continue;
        };
        let bare = raw.split_once('#').map_or(raw, |(p, _)| p);
        let path = PathBuf::from(bare);
        if filter.accepts_bound(&ctx.dir, &path) {
            set.insert(path);
        }
    }

    Ok(set.into_iter().collect())
}

/// Reconcile a single file within the current transaction.
// A linear pipeline — read disk, assemble KB, decide direction, act — ending in one arm per
// direction. Splitting it further would mean threading the same seven parameters through more
// helpers than it saves; the stages that carry real logic (`decide_direction`, `missing_file`)
// are already extracted and separately testable.
#[allow(clippy::too_many_lines)]
fn reconcile(conn: &Connection, meta: &WriteMeta, ctx: &Ctx, path: &Path) -> Result<Outcome> {
    let bare_uri = file_uri(path);
    let (ser_name, serializer) = resolve_serializer(conn, ctx, &bare_uri)?;
    let journal = sync_state::get(conn, &bare_uri)?;
    let base_hash = journal.as_ref().and_then(|j| j.last_synced_hash.clone());

    // Parse the disk side (if the file exists). A quarantining serializer turns a parse
    // failure into a journal flag instead of a hard error, protecting last-good items.
    let disk = if path.exists() {
        let bytes = std::fs::read(path)?;
        match serializer.parse(&bytes) {
            Ok(doc) => Some((bytes, doc)),
            Err(e) => {
                if serializer.quarantine_on_parse_error() {
                    return quarantine(
                        conn,
                        meta,
                        &bare_uri,
                        &ser_name,
                        &bytes,
                        &e,
                        journal.as_ref(),
                    );
                }
                return Err(e);
            }
        }
    } else {
        None
    };

    // Assemble the KB side and render it, so we can hash it against the base.
    let kb_doc = assemble_kb_doc(conn, ctx, path, &bare_uri)?;
    let kb_bytes = serializer.render(&kb_doc)?;
    let kb_hash = hash(&kb_bytes);
    let kb_has_items = !kb_doc.items.is_empty();

    let Some((disk_bytes, disk_doc)) = disk else {
        return missing_file(
            conn,
            meta,
            ctx,
            path,
            &bare_uri,
            &ser_name,
            &kb_bytes,
            kb_has_items,
        );
    };

    let disk_hash = hash(&disk_bytes);

    // First sight of this file (no journal row yet).
    if journal.is_none() {
        if ctx.imports() {
            return finish_import(
                conn,
                meta,
                ctx,
                path,
                &bare_uri,
                &ser_name,
                serializer.as_ref(),
                &disk_doc,
                &disk_bytes,
                Outcome::Created,
            );
        }
        return missing_file(
            conn,
            meta,
            ctx,
            path,
            &bare_uri,
            &ser_name,
            &kb_bytes,
            kb_has_items,
        );
    }

    let (disk_changed, kb_changed, base_doc) = decide_direction(
        conn,
        serializer.as_ref(),
        journal.as_ref(),
        base_hash.as_deref(),
        Sides {
            disk: (&disk_doc, &disk_hash),
            kb: (&kb_bytes, &kb_hash),
        },
    )?;
    let was_flagged = journal.as_ref().is_some_and(|j| j.status != "ok");

    match (disk_changed, kb_changed) {
        (false, false) => {
            if was_flagged {
                // A previously quarantined/conflicted file is now clean again.
                mark_ok(conn, meta, &bare_uri, &ser_name, &kb_hash, &kb_bytes)?;
            }
            Ok(Outcome::UpToDate)
        }
        (true, false) => finish_import(
            conn,
            meta,
            ctx,
            path,
            &bare_uri,
            &ser_name,
            serializer.as_ref(),
            &disk_doc,
            &disk_bytes,
            Outcome::Imported,
        ),
        (false, true) => finish_export(conn, meta, ctx, path, &bare_uri, &ser_name, &kb_bytes),
        (true, true) => three_way_resolve(
            conn,
            meta,
            ctx,
            path,
            &bare_uri,
            &ser_name,
            serializer.as_ref(),
            journal.as_ref(),
            &disk_doc,
            &disk_bytes,
            &kb_doc,
            &kb_bytes,
            &base_doc.unwrap_or_default(),
        ),
    }
}

/// Nothing can be imported (the file is gone, or the mount is export-only). If the KB still
/// has items bound to this path, export to write it — that covers a previously-synced file
/// deleted on disk and a KB-created binding not yet written (`task add --sync`). Otherwise
/// there is nothing to reconcile.
#[allow(clippy::too_many_arguments)]
fn missing_file(
    conn: &Connection,
    meta: &WriteMeta,
    ctx: &Ctx,
    path: &Path,
    bare_uri: &str,
    ser_name: &str,
    kb_bytes: &[u8],
    kb_has_items: bool,
) -> Result<Outcome> {
    if ctx.exports() && kb_has_items {
        return finish_export(conn, meta, ctx, path, bare_uri, ser_name, kb_bytes);
    }
    Ok(Outcome::Skipped)
}

/// The two sides a direction decision compares against the base.
#[derive(Clone, Copy)]
struct Sides<'a> {
    /// The parsed disk document and its raw byte hash.
    disk: (&'a SyncDoc, &'a str),
    /// The rendered KB bytes and their hash.
    kb: (&'a [u8], &'a str),
}

/// Decide which side(s) changed, and return the parsed base alongside so a three-way merge
/// does not have to load it twice.
///
/// The comparison is between **documents, not bytes**. `kb` bytes are what *today's*
/// serializer renders; the stored base is bytes some earlier version of it wrote. Comparing
/// those directly conflates "the content changed" with "the renderer changed" — and the
/// second manufactures a phantom edit on every file in a mount at once, which has silently
/// exported over real work and, where the disk had also moved, produced a wall of conflicts
/// that were not conflicts. Re-rendering the base through today's serializer puts every side
/// in the same vocabulary, so only genuine differences survive.
///
/// Byte equality stays the fast path: identical bytes cannot be a changed document, so a
/// settled file still costs two hashes and no parse.
fn decide_direction(
    conn: &Connection,
    serializer: &dyn SyncSerializer,
    journal: Option<&sync_state::SyncState>,
    base_hash: Option<&str>,
    sides: Sides<'_>,
) -> Result<(bool, bool, Option<SyncDoc>)> {
    let (disk_doc, disk_hash) = sides.disk;
    let (kb_bytes, kb_hash) = sides.kb;
    let disk_bytes_differ = base_hash != Some(disk_hash);
    let kb_bytes_differ = base_hash != Some(kb_hash);
    if !disk_bytes_differ && !kb_bytes_differ {
        return Ok((false, false, None));
    }

    let base_doc = load_base_doc(conn, journal, serializer)?;
    match &base_doc {
        Some(doc) => {
            let canonical = serializer.render(doc)?;
            Ok((
                serializer.render(disk_doc)? != canonical,
                kb_bytes != canonical,
                base_doc,
            ))
        }
        // No base document to compare against: the blob is gone (a journal predating the
        // blob store, or one whose blob was pruned). The byte comparison is then all the
        // information there is.
        None => Ok((disk_bytes_differ, kb_bytes_differ, None)),
    }
}

/// Resolve the effective serializer for a file: a per-file `bindings.serializer`
/// override if any item bound to this file carries one, else the mount's (design D24).
fn resolve_serializer(
    conn: &Connection,
    ctx: &Ctx,
    bare_uri: &str,
) -> Result<(String, Box<dyn SyncSerializer>)> {
    let mut name = ctx.serializer.clone();
    let uris = binding::synced_uris_for_file(conn, bare_uri)?;
    if let Some(first) = uris.first() {
        if let Some(id) = binding::item_for_uri(conn, first)? {
            if let Some(over) = binding::get(conn, id)?.and_then(|b| b.serializer) {
                name = over;
            }
        }
    }
    let serializer = resolve(&name)?;
    Ok((name, serializer))
}

/// Import `doc` into the KB, write the canonical rendered bytes back if they differ
/// from what is on disk (persisting minted `^ids`), and record the base + journal.
#[allow(clippy::too_many_arguments)]
fn finish_import(
    conn: &Connection,
    meta: &WriteMeta,
    ctx: &Ctx,
    path: &Path,
    bare_uri: &str,
    ser_name: &str,
    serializer: &dyn SyncSerializer,
    doc: &SyncDoc,
    disk_bytes: &[u8],
    outcome: Outcome,
) -> Result<Outcome> {
    if !ctx.imports() {
        return Ok(Outcome::Skipped);
    }
    let resolved = apply_doc(conn, meta, ctx, path, bare_uri, doc)?;
    let rendered = serializer.render(doc)?;
    // Persist identity / normalization back to disk (the rendered form is authoritative).
    if rendered != disk_bytes {
        write_file(path, &rendered)?;
    }
    settle(conn, meta, bare_uri, ser_name, &rendered, &resolved)?;
    Ok(outcome)
}

/// Export the rendered KB bytes to the file and record the base + journal.
#[allow(clippy::too_many_arguments)]
fn finish_export(
    conn: &Connection,
    meta: &WriteMeta,
    ctx: &Ctx,
    path: &Path,
    bare_uri: &str,
    ser_name: &str,
    kb_bytes: &[u8],
) -> Result<Outcome> {
    if !ctx.exports() {
        return Ok(Outcome::Skipped);
    }
    write_file(path, kb_bytes)?;
    let resolved = current_bindings(conn, bare_uri)?;
    settle(conn, meta, bare_uri, ser_name, kb_bytes, &resolved)?;
    Ok(Outcome::Exported)
}

/// Both sides changed: attempt a three-way merge of disjoint edits, else resolve by the
/// mount's `conflict_policy`.
#[allow(clippy::too_many_arguments)]
fn three_way_resolve(
    conn: &Connection,
    meta: &WriteMeta,
    ctx: &Ctx,
    path: &Path,
    bare_uri: &str,
    ser_name: &str,
    serializer: &dyn SyncSerializer,
    journal: Option<&sync_state::SyncState>,
    disk_doc: &SyncDoc,
    disk_bytes: &[u8],
    kb_doc: &SyncDoc,
    kb_bytes: &[u8],
    base_doc: &SyncDoc,
) -> Result<Outcome> {
    match three_way(base_doc, disk_doc, kb_doc) {
        ThreeWay::Merged(merged) => {
            let resolved = apply_doc(conn, meta, ctx, path, bare_uri, &merged)?;
            let rendered = serializer.render(&merged)?;
            if ctx.exports() {
                write_file(path, &rendered)?;
            }
            settle(conn, meta, bare_uri, ser_name, &rendered, &resolved)?;
            Ok(Outcome::Merged)
        }
        ThreeWay::Conflict => match ctx.conflict_policy.as_str() {
            "disk_wins" => finish_import(
                conn,
                meta,
                ctx,
                path,
                bare_uri,
                ser_name,
                serializer,
                disk_doc,
                disk_bytes,
                Outcome::Imported,
            ),
            "kb_wins" => finish_export(conn, meta, ctx, path, bare_uri, ser_name, kb_bytes),
            _ => {
                // manual: overwrite neither side; flag the file so `doctor` can surface it.
                let base = journal.and_then(|j| j.base_blob_hash.clone());
                let last = journal.and_then(|j| j.last_synced_hash.clone());
                sync_state::upsert(
                    conn,
                    meta,
                    &sync_state::SyncStateWrite {
                        uri: bare_uri,
                        serializer: ser_name,
                        status: "conflict",
                        last_synced_hash: last.as_deref(),
                        base_blob_hash: base.as_deref(),
                        parse_error: None,
                        quarantine_blob_hash: None,
                    },
                )?;
                Ok(Outcome::Conflict)
            }
        },
    }
}

/// Stash the failing bytes and flag the journal `needs_attention`, keeping the KB items
/// and the existing base untouched (design D25 quarantine-don't-destroy).
fn quarantine(
    conn: &Connection,
    meta: &WriteMeta,
    bare_uri: &str,
    ser_name: &str,
    bytes: &[u8],
    err: &Error,
    journal: Option<&sync_state::SyncState>,
) -> Result<Outcome> {
    let qhash = blob::hash_bytes(bytes);
    blob::store(conn, &qhash, bytes, None)?;
    let base = journal.and_then(|j| j.base_blob_hash.clone());
    let last = journal.and_then(|j| j.last_synced_hash.clone());
    sync_state::upsert(
        conn,
        meta,
        &sync_state::SyncStateWrite {
            uri: bare_uri,
            serializer: ser_name,
            status: "needs_attention",
            last_synced_hash: last.as_deref(),
            base_blob_hash: base.as_deref(),
            parse_error: Some(&err.to_string()),
            quarantine_blob_hash: Some(&qhash),
        },
    )?;
    Ok(Outcome::Quarantined)
}

/// Record a clean sync: store the base blob, stamp each item's binding (back-compat),
/// and upsert the journal `ok`.
fn settle(
    conn: &Connection,
    meta: &WriteMeta,
    bare_uri: &str,
    ser_name: &str,
    base_bytes: &[u8],
    items: &[ItemId],
) -> Result<()> {
    let base_hash = blob::hash_bytes(base_bytes);
    blob::store(conn, &base_hash, base_bytes, None)?;
    for id in items {
        binding::mark_synced(conn, meta, *id, &base_hash)?;
    }
    sync_state::upsert(
        conn,
        meta,
        &sync_state::SyncStateWrite {
            uri: bare_uri,
            serializer: ser_name,
            status: "ok",
            last_synced_hash: Some(&base_hash),
            base_blob_hash: Some(&base_hash),
            parse_error: None,
            quarantine_blob_hash: None,
        },
    )?;
    Ok(())
}

/// Re-affirm an `ok` journal for a file whose quarantine/conflict has cleared with no
/// remaining drift.
fn mark_ok(
    conn: &Connection,
    meta: &WriteMeta,
    bare_uri: &str,
    ser_name: &str,
    hash: &str,
    bytes: &[u8],
) -> Result<()> {
    blob::store(conn, hash, bytes, None)?;
    sync_state::upsert(
        conn,
        meta,
        &sync_state::SyncStateWrite {
            uri: bare_uri,
            serializer: ser_name,
            status: "ok",
            last_synced_hash: Some(hash),
            base_blob_hash: Some(hash),
            parse_error: None,
            quarantine_blob_hash: None,
        },
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Multi-item apply (KB write side)
// ---------------------------------------------------------------------------

/// Apply `doc` to the KB (create/update items, place them under their section
/// namespaces, reconcile tags and edges, cancel removed tasks). Two passes so edges
/// resolve after every item exists. Returns the item ids the doc now maps to.
fn apply_doc(
    conn: &Connection,
    meta: &WriteMeta,
    ctx: &Ctx,
    path: &Path,
    bare_uri: &str,
    doc: &SyncDoc,
) -> Result<Vec<ItemId>> {
    let file_ns_path = namespace_for(ctx, path);
    let file_ns = ns::ensure(conn, &file_ns_path)?;

    // Sections → namespaces, carrying only their header text. Their ORDER (and the file's
    // prose) lives in the layout stored on the file namespace below — one sequence, so
    // nothing can drift against anything else.
    let mut section_ns: HashMap<String, NamespaceId> = HashMap::new();
    for s in &doc.sections {
        let full = format!("{file_ns_path}/{}", s.path);
        let id = ns::ensure(conn, &full)?;
        ns::set_metadata(
            conn,
            meta,
            id,
            &json!({ "header_line": s.header_line, "sync_section": true }),
        )?;
        section_ns.insert(s.path.clone(), id);
    }
    set_layout(conn, meta, file_ns, doc)?;
    // A section the file no longer declares must stop being a section. Its namespace can
    // legitimately survive — it may still hold cancelled tasks, which are deliberate history
    // — but leaving `header_line` on it makes `assemble_kb_doc` re-emit a `##` header the
    // file does not have, so the KB render disagrees with the disk forever and every later
    // disk edit is resolved as a conflict (see `memory/sync-export-wins`).
    retire_undeclared_sections(conn, meta, &file_ns_path, doc)?;

    // Existing items bound to this file, by local_id.
    let existing = existing_by_local(conn, bare_uri)?;

    // Pass 1 — items only (no edges yet).
    let mut resolved: HashMap<String, ItemId> = HashMap::new();
    for it in &doc.items {
        let uri = item_uri(bare_uri, &it.local_id);
        let home = it
            .section
            .as_ref()
            .and_then(|s| section_ns.get(s).copied())
            .unwrap_or(file_ns);
        let id = match existing.get(&it.local_id) {
            Some(&id) => {
                update_item(conn, meta, id, it, home)?;
                id
            }
            None => create_item(conn, meta, ctx, it, &uri, home)?,
        };
        for m in &it.mirrors {
            let mns = ns::ensure(conn, m)?;
            placement::place(conn, meta, id, mns, PlacementRole::Reference, 0)?;
        }
        resolved.insert(it.local_id.clone(), id);
    }

    // Items that vanished from the file are detached (rebound to `managed:`) so they are
    // not re-exported, and tasks are additionally marked `cancelled` — non-destructive:
    // the item, its edges, and its history survive (design D25).
    for (lid, &id) in &existing {
        if resolved.contains_key(lid) {
            continue;
        }
        if item_kind(conn, id)?.as_deref() == Some("task") {
            task::set_status(conn, meta, id, TaskStatus::Cancelled)?;
        }
        binding::set(conn, meta, id, "managed:", None, None)?;
    }

    // Pass 2 — reconcile edges now that every local_id resolves to an item.
    reconcile_edges(conn, meta, doc, &resolved)?;

    Ok(resolved.into_values().collect())
}

/// The metadata key holding a file's block order (and its prose, inline).
const LAYOUT_KEY: &str = "layout";

/// Serialize a document's layout for storage on the file's namespace.
fn layout_json(doc: &SyncDoc) -> serde_json::Value {
    let blocks: Vec<serde_json::Value> = doc
        .layout
        .iter()
        .map(|b| match b {
            SyncBlock::Section(path) => json!({ "section": path }),
            SyncBlock::Item(id) => json!({ "item": id }),
            SyncBlock::Prose(text) => json!({ "prose": text }),
        })
        .collect();
    serde_json::Value::Array(blocks)
}

/// Store the document's layout on the file's own namespace, **merging** into whatever
/// metadata it already carries (that namespace may be a mount's, with its own keys).
fn set_layout(
    conn: &Connection,
    meta: &WriteMeta,
    file_ns: NamespaceId,
    doc: &SyncDoc,
) -> Result<()> {
    let mut metadata = ns::get_metadata(conn, file_ns)?
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| json!({}));
    if let Some(map) = metadata.as_object_mut() {
        map.insert(LAYOUT_KEY.to_owned(), layout_json(doc));
    }
    ns::set_metadata(conn, meta, file_ns, &metadata)?;
    Ok(())
}

/// Read a stored layout back out of a namespace's metadata.
fn read_layout(metadata: &serde_json::Value) -> Vec<SyncBlock> {
    let Some(blocks) = metadata
        .get(LAYOUT_KEY)
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|b| {
            let text = |k: &str| {
                b.get(k)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            };
            if let Some(p) = text("section") {
                Some(SyncBlock::Section(p))
            } else if let Some(i) = text("item") {
                Some(SyncBlock::Item(i))
            } else {
                text("prose").map(SyncBlock::Prose)
            }
        })
        .collect()
}

/// Clear the section metadata of every namespace under the file that the document no longer
/// declares, so it stops being rendered as a `##` header. The namespace and everything in it
/// are left alone — this only retires its *section* role.
fn retire_undeclared_sections(
    conn: &Connection,
    meta: &WriteMeta,
    file_ns_path: &str,
    doc: &SyncDoc,
) -> Result<()> {
    let declared: std::collections::HashSet<&str> =
        doc.sections.iter().map(|s| s.path.as_str()).collect();
    for (ns_id, ns_path) in ns::subtree(conn, file_ns_path)? {
        if ns_path == file_ns_path {
            continue;
        }
        if declared.contains(relative(file_ns_path, &ns_path).as_str()) {
            continue;
        }
        let Some(mut metadata) = ns::get_metadata(conn, ns_id)? else {
            continue;
        };
        let Some(map) = metadata.as_object_mut() else {
            continue;
        };
        if map.remove("header_line").is_none() {
            continue; // not a section to begin with
        }
        map.remove("position");
        map.remove("sync_section");
        map.remove("prose");
        ns::set_metadata(conn, meta, ns_id, &metadata)?;
    }
    Ok(())
}

/// Create a new item for a [`SyncItem`], placing it under its home namespace and
/// binding it to `uri`. `content_hash` is left `None` so two identical-title tasks do
/// not dedup-collapse into one item (their `local_id`/uri is the real identity).
fn create_item(
    conn: &Connection,
    meta: &WriteMeta,
    ctx: &Ctx,
    it: &SyncItem,
    uri: &str,
    home: NamespaceId,
) -> Result<ItemId> {
    // A line deleted from the file is detached, not deleted (design D25) — and it keeps its
    // file-derived uid. Re-adding that same line mints the same uid, so a plain insert hits
    // the UNIQUE constraint and the whole sync fails. Re-attaching instead is both the fix
    // and the better semantics: deleting a line and putting it back restores the same item,
    // with its edges, tags and history intact, rather than a stranger wearing its name.
    let id = if let Some(existing) = item::id_for_uid(conn, uri)? {
        update_item(conn, meta, existing, it, home)?;
        existing
    } else {
        let id = item::upsert(
            conn,
            meta,
            &NewItem {
                uid: uri.to_owned(),
                kind: it.kind.clone(),
                content: Some(it.content.clone()),
                content_hash: None,
                mime: None,
            },
        )?;
        set_task_columns(conn, meta, id, it)?;
        placement::place(conn, meta, id, home, PlacementRole::Primary, it.position)?;
        id
    };
    binding::set(
        conn,
        meta,
        id,
        uri,
        Some(sync_mode_of(&ctx.sync_mode)),
        None,
    )?;
    for (facet, value) in &it.tags {
        tag::apply(conn, meta, id, facet, value)?;
    }
    Ok(id)
}

/// Update an existing item to match a [`SyncItem`]: content, task columns, tags, and
/// primary placement — each only when it actually differs, to keep the changelog quiet.
fn update_item(
    conn: &Connection,
    meta: &WriteMeta,
    id: ItemId,
    it: &SyncItem,
    home: NamespaceId,
) -> Result<()> {
    if item::get_content(conn, id)?.as_deref() != Some(it.content.as_str()) {
        item::set_content(conn, meta, id, &it.content, None)?;
    }
    set_task_columns(conn, meta, id, it)?;
    reconcile_tags(conn, meta, id, &it.tags)?;
    task::set_primary_home(conn, meta, id, home, it.position)?;
    Ok(())
}

/// Set the `status`/`priority`/`due` columns for a task item, each only when changed.
fn set_task_columns(conn: &Connection, meta: &WriteMeta, id: ItemId, it: &SyncItem) -> Result<()> {
    if it.kind != "task" {
        return Ok(());
    }
    let (status, priority, due): (Option<String>, Option<i64>, Option<String>) = conn
        .prepare_cached("SELECT status, priority, due FROM items WHERE id = ?1")?
        .query_row([id.get()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    if it.status != status {
        if let Some(s) = &it.status {
            task::set_status_str(conn, meta, id, s)?;
        }
    }
    if it.priority != priority {
        task::set_priority(conn, meta, id, it.priority)?;
    }
    if it.due != due {
        task::set_due(conn, meta, id, it.due.as_deref())?;
    }
    Ok(())
}

/// Reconcile an item's tags to exactly `desired` (add missing, drop stale).
fn reconcile_tags(
    conn: &Connection,
    meta: &WriteMeta,
    id: ItemId,
    desired: &[(String, String)],
) -> Result<()> {
    let current = tag::applications(conn, id)?;
    let want: HashSet<&(String, String)> = desired.iter().collect();
    for (facet, value) in &current {
        if !want.contains(&(facet.clone(), value.clone())) {
            tag::remove(conn, meta, id, facet, value)?;
        }
    }
    let have: HashSet<&(String, String)> = current.iter().collect();
    for (facet, value) in desired {
        if !have.contains(&(facet.clone(), value.clone())) {
            tag::apply(conn, meta, id, facet, value)?;
        }
    }
    Ok(())
}

/// Reconcile `parent_of` and `depends_on` edges to exactly what `doc` declares.
fn reconcile_edges(
    conn: &Connection,
    meta: &WriteMeta,
    doc: &SyncDoc,
    resolved: &HashMap<String, ItemId>,
) -> Result<()> {
    let srcs: Vec<ItemId> = resolved.values().copied().collect();
    for kind in [EdgeType::ParentOf, EdgeType::DependsOn] {
        // desired src -> set(dst) from the doc, mapped to item ids.
        let mut desired: HashMap<ItemId, HashSet<ItemId>> = HashMap::new();
        for e in doc.edges.iter().filter(|e| e.edge_type == kind) {
            if let (Some(&s), Some(&d)) = (resolved.get(&e.src), resolved.get(&e.dst)) {
                if s != d {
                    desired.entry(s).or_default().insert(d);
                }
            }
        }
        // Current edges for all sources in one query, indexed by source.
        let mut current = edge::edges_from_many(conn, &srcs, kind)?;
        for &src in &srcs {
            let want = desired.get(&src).cloned().unwrap_or_default();
            let have: HashSet<ItemId> = current
                .remove(&src)
                .unwrap_or_default()
                .into_iter()
                .collect();
            for &dst in want.difference(&have) {
                edge::link(conn, meta, src, dst, kind, None)?;
            }
            for &dst in have.difference(&want) {
                edge::unlink(conn, meta, src, dst, kind)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// KB read side — assemble a SyncDoc from the items bound to a file
// ---------------------------------------------------------------------------

/// Build a [`SyncDoc`] from the current KB state of a file: its section namespaces and
/// the items bound under it. The inverse of [`apply_doc`], so `render(assemble_kb_doc)`
/// reproduces the last-synced base when the KB is unchanged.
fn assemble_kb_doc(conn: &Connection, ctx: &Ctx, path: &Path, bare_uri: &str) -> Result<SyncDoc> {
    let file_ns_path = namespace_for(ctx, path);
    let mut doc = SyncDoc::default();

    // The file namespace carries the document's layout — its block order and its prose.
    // Sections are the descendant namespaces still carrying `header_line`: one the file
    // stopped declaring was retired by `retire_undeclared_sections` and must not re-emit its
    // header.
    if let Some(file_ns) = ns::get(conn, &file_ns_path)? {
        if let Some(md) = ns::get_metadata(conn, file_ns)? {
            doc.layout = read_layout(&md);
        }
        for (ns_id, ns_path) in ns::subtree(conn, &file_ns_path)? {
            if ns_path == file_ns_path {
                continue;
            }
            let Some(md) = ns::get_metadata(conn, ns_id)? else {
                continue;
            };
            let Some(header) = md.get("header_line").and_then(|v| v.as_str()) else {
                continue;
            };
            doc.sections.push(SyncSection {
                path: relative(&file_ns_path, &ns_path),
                header_line: header.to_owned(),
            });
        }
    }

    // Items: everything bound to this file. Resolve every binding in one query, then
    // build both directions of the id map (uris stay ordered for a stable round-trip).
    let uris = binding::synced_uris_for_file(conn, bare_uri)?;
    let uri_ids = binding::items_for_uris(conn, &uris)?;
    let mut id_to_local: HashMap<i64, String> = HashMap::new();
    let mut resolved: Vec<(String, ItemId)> = Vec::new();
    for uri in &uris {
        let local_id = local_of(bare_uri, uri);
        if let Some(&id) = uri_ids.get(uri) {
            id_to_local.insert(id.get(), local_id.clone());
            resolved.push((local_id, id));
        }
    }

    // Batch every per-item lookup into one query each, keyed by item id, so this is a
    // constant number of round-trips instead of O(N) point queries for N items.
    let ids: Vec<ItemId> = resolved.iter().map(|(_, id)| *id).collect();
    let mut tags = tag::applications_for(conn, &ids)?;
    let mut mirrors = mirror_paths_for(conn, &ids, &file_ns_path)?;
    let parents = edge::edges_from_many(conn, &ids, EdgeType::ParentOf)?;
    let deps = edge::edges_from_many(conn, &ids, EdgeType::DependsOn)?;
    let mut item_rows = item_rows_for(conn, &ids)?;
    let placements = primary_placements_for(conn, &ids)?;

    for (local_id, id) in &resolved {
        // Skip items with no row or no primary placement (mirrors the old `load_item`).
        let (Some(row), Some(placement)) = (item_rows.remove(id), placements.get(id)) else {
            continue;
        };
        let mut item = build_sync_item(local_id, &file_ns_path, row, placement);
        item.tags = tags.remove(id).unwrap_or_default();
        item.mirrors = mirrors.remove(id).unwrap_or_default();
        doc.items.push(item);

        for (edge_type, targets) in [
            (EdgeType::ParentOf, parents.get(id)),
            (EdgeType::DependsOn, deps.get(id)),
        ] {
            for dst_id in targets.into_iter().flatten() {
                if let Some(dst) = id_to_local.get(&dst_id.get()) {
                    doc.edges.push(crate::serializers::SyncEdge {
                        src: local_id.clone(),
                        dst: dst.clone(),
                        edge_type,
                    });
                }
            }
        }
    }

    // A KB written before the layout model has none stored. Rebuild it from the legacy
    // positional data (section `metadata.position`, item `placements.position`, and any
    // `metadata.prose` from the intermediate model) so the assembled document still matches
    // the file. Without this the render comes out empty-ordered, the KB looks changed, and
    // sync exports that garbage over every file it manages — which is exactly what happened.
    // The next import replaces it with a real layout.
    if doc.layout.is_empty() {
        doc.layout = legacy_layout(conn, ctx, path, &doc)?;
    }

    // An item's SECTION comes from the layout — the section header it sits under in the file
    // — not from the namespace it happens to be placed in. The layout is authoritative for
    // document structure, so the two must not be allowed to disagree: when they did, a
    // KB-side re-home left the assembled doc permanently different from the base, and every
    // subsequent disk edit came back as a conflict. Re-homing a file-backed item therefore
    // does not move it between sections in its file; editing the file does.
    apply_layout_sections(&mut doc);
    Ok(doc)
}

/// Reconstruct a layout for a KB that predates the layout model, from the ordinals it does
/// have: each section's `namespaces.metadata.position`, each item's `placements.position`,
/// and any prose stored under the intermediate `metadata.prose` key. Interleaving them by
/// ordinal is precisely the old (fragile) ordering — which is correct *here*, because the
/// goal is to reproduce what that KB last rendered, not to improve on it.
fn legacy_layout(
    conn: &Connection,
    ctx: &Ctx,
    path: &Path,
    doc: &SyncDoc,
) -> Result<Vec<SyncBlock>> {
    let file_ns_path = namespace_for(ctx, path);
    let mut blocks: Vec<(i64, SyncBlock)> = Vec::new();

    if let Some(file_ns) = ns::get(conn, &file_ns_path)? {
        collect_legacy_prose(conn, file_ns, &mut blocks)?;
        for (ns_id, ns_path) in ns::subtree(conn, &file_ns_path)? {
            if ns_path == file_ns_path {
                continue;
            }
            let Some(md) = ns::get_metadata(conn, ns_id)? else {
                continue;
            };
            if md.get("header_line").is_none() {
                continue;
            }
            let position = md
                .get("position")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            blocks.push((
                position,
                SyncBlock::Section(relative(&file_ns_path, &ns_path)),
            ));
            collect_legacy_prose(conn, ns_id, &mut blocks)?;
        }
    }
    for item in &doc.items {
        blocks.push((item.position, SyncBlock::Item(item.local_id.clone())));
    }
    blocks.sort_by_key(|(p, _)| *p);
    Ok(blocks.into_iter().map(|(_, b)| b).collect())
}

/// Pull any intermediate-model `metadata.prose` entries off a namespace into `blocks`.
fn collect_legacy_prose(
    conn: &Connection,
    ns_id: NamespaceId,
    blocks: &mut Vec<(i64, SyncBlock)>,
) -> Result<()> {
    let Some(md) = ns::get_metadata(conn, ns_id)? else {
        return Ok(());
    };
    let Some(entries) = md.get("prose").and_then(serde_json::Value::as_array) else {
        return Ok(());
    };
    for entry in entries {
        let Some(content) = entry.get("content").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let position = entry
            .get("position")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        blocks.push((position, SyncBlock::Prose(content.to_owned())));
    }
    Ok(())
}

/// Set each item's `section` from its position in the layout (the nearest preceding section
/// block). Items absent from the layout keep the section derived from their placement.
fn apply_layout_sections(doc: &mut SyncDoc) {
    let mut current: Option<String> = None;
    let mut by_id: HashMap<&str, Option<String>> = HashMap::new();
    for block in &doc.layout {
        match block {
            SyncBlock::Section(path) => current = Some(path.clone()),
            SyncBlock::Item(id) => {
                by_id.insert(id.as_str(), current.clone());
            }
            SyncBlock::Prose(_) => {}
        }
    }
    for item in &mut doc.items {
        if let Some(section) = by_id.get(item.local_id.as_str()) {
            item.section.clone_from(section);
        }
    }
}

/// An item's `(kind, content, status, priority, due)` columns.
type ItemRow = (
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

/// The `(kind, content, status, priority, due)` column rows of the given items, keyed
/// by id, in one query. Items with no row are absent from the map. The batched form of
/// the per-item `items` select the old `load_item` ran.
fn item_rows_for(conn: &Connection, ids: &[ItemId]) -> Result<HashMap<ItemId, ItemRow>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT id, kind, content, status, priority, due FROM items WHERE id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(ids.iter().map(|id| id.get())),
        |r| {
            Ok((
                ItemId::new(r.get(0)?),
                (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?),
            ))
        },
    )?;
    for row in rows {
        let (id, item_row) = row?;
        out.insert(id, item_row);
    }
    Ok(out)
}

/// The primary-placement `(namespace path, position)` of the given items, keyed by id,
/// in one query. Items with no primary placement are absent from the map. The batched
/// form of the per-item primary-placement select the old `load_item` ran (an item has
/// one primary placement by construction, so first-wins is deterministic).
fn primary_placements_for(
    conn: &Connection,
    ids: &[ItemId],
) -> Result<HashMap<ItemId, (String, i64)>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT p.item_id, n.path, p.position FROM placements p
         JOIN namespaces n ON n.id = p.namespace_id
         WHERE p.role = 'primary' AND p.item_id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(ids.iter().map(|id| id.get())),
        |r| {
            Ok((
                ItemId::new(r.get(0)?),
                (r.get::<_, String>(1)?, r.get::<_, i64>(2)?),
            ))
        },
    )?;
    for row in rows {
        let (id, placement) = row?;
        out.entry(id).or_insert(placement);
    }
    Ok(out)
}

/// Shape a [`SyncItem`] from an item's already-batched row + primary placement
/// (tags/mirrors/edges are filled by the caller). The pure inverse of the per-item
/// mapping the old `load_item` did once the two rows were in hand.
fn build_sync_item(
    local_id: &str,
    file_ns_path: &str,
    row: ItemRow,
    placement: &(String, i64),
) -> SyncItem {
    let (kind, content, status, priority, due) = row;
    let (ns_path, position) = placement;
    let section = if ns_path == file_ns_path {
        None
    } else {
        Some(relative(file_ns_path, ns_path))
    };

    let mut item = SyncItem::new(local_id.to_owned(), &kind, content.unwrap_or_default());
    item.section = section;
    item.position = *position;
    item.status = status;
    item.priority = priority;
    item.due = due;
    item
}

/// The reference-placement namespace paths of the given items outside their file
/// namespace subtree, as `+ns` mirrors, keyed by item id in one query. Each item's
/// paths stay ordered (`ORDER BY item_id, n.path`) for a stable round-trip. Items
/// with no mirrors are absent from the map.
fn mirror_paths_for(
    conn: &Connection,
    ids: &[ItemId],
    file_ns_path: &str,
) -> Result<HashMap<ItemId, Vec<String>>> {
    let mut out: HashMap<ItemId, Vec<String>> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    // Escape LIKE metacharacters in the path (namespace paths contain `_`), so the subtree
    // exclusion is literal and doesn't spuriously match a sibling namespace.
    let subtree_like = format!("{}/%", jkb_core::sql::like_escape(file_ns_path));
    let placeholders = vec!["?"; ids.len()].join(", ");
    // `tasks/**` reference placements are the internal task index (auto-mirrored by
    // `task::ensure_task_mirror`), not user-authored `+ns` mirrors — never serialize
    // them back into the file, or they'd leak in as `+tasks/…` and break byte-stability.
    let sql = format!(
        "SELECT p.item_id, n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
         WHERE p.role = 'reference' AND n.path != ? AND n.path NOT LIKE ? ESCAPE '\\'
           AND n.path != 'tasks' AND n.path NOT LIKE 'tasks/%'
           AND p.item_id IN ({placeholders})
         ORDER BY p.item_id, n.path"
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(ids.len() + 2);
    params.push(rusqlite::types::Value::Text(file_ns_path.to_owned()));
    params.push(rusqlite::types::Value::Text(subtree_like));
    params.extend(
        ids.iter()
            .map(|id| rusqlite::types::Value::Integer(id.get())),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        Ok((ItemId::new(r.get::<_, i64>(0)?), r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, path) = row?;
        out.entry(id).or_default().push(path);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Three-way merge
// ---------------------------------------------------------------------------

/// The outcome of a per-item three-way comparison.
enum ThreeWay {
    /// Disjoint edits — a merged document combining both sides.
    Merged(SyncDoc),
    /// The same item changed incompatibly on both sides.
    Conflict,
}

/// A semantic signature of an item, used to detect per-item changes (ignores position,
/// which is presentation, not meaning).
#[derive(PartialEq, Eq)]
struct Sig {
    content: String,
    status: Option<String>,
    priority: Option<i64>,
    due: Option<String>,
    section: Option<String>,
    /// The item's parent `local_id` (its `parent_of` incoming edge). Part of the
    /// signature so a re-parenting/indentation change on either side is detected —
    /// without it a nesting edit has an identical `Sig` and is silently reverted.
    parent: Option<String>,
    tags: Vec<(String, String)>,
    mirrors: Vec<String>,
    deps: Vec<String>,
}

/// Signatures of every item in `doc`, keyed by `local_id`.
fn sigs(doc: &SyncDoc) -> HashMap<String, Sig> {
    let mut deps: HashMap<&str, Vec<String>> = HashMap::new();
    // `parent_of` edges run parent(src) -> child(dst); key the child's parent by dst so
    // a re-parenting shows up in the child's signature (the edge's authoritative form).
    let mut parent_of: HashMap<&str, String> = HashMap::new();
    for e in &doc.edges {
        match e.edge_type {
            EdgeType::DependsOn => deps.entry(&e.src).or_default().push(e.dst.clone()),
            EdgeType::ParentOf => {
                parent_of.insert(&e.dst, e.src.clone());
            }
            _ => {}
        }
    }
    doc.items
        .iter()
        .map(|i| {
            let mut tags = i.tags.clone();
            tags.sort();
            let mut mirrors = i.mirrors.clone();
            mirrors.sort();
            let mut d = deps.get(i.local_id.as_str()).cloned().unwrap_or_default();
            d.sort();
            (
                i.local_id.clone(),
                Sig {
                    content: i.content.clone(),
                    status: i.status.clone(),
                    priority: i.priority,
                    due: i.due.clone(),
                    section: i.section.clone(),
                    parent: parent_of.get(i.local_id.as_str()).cloned(),
                    tags,
                    mirrors,
                    deps: d,
                },
            )
        })
        .collect()
}

/// Merge disjoint disk and KB edits against a common base. Returns [`ThreeWay::Conflict`]
/// if any single item changed incompatibly on both sides.
fn three_way(base: &SyncDoc, disk: &SyncDoc, kb: &SyncDoc) -> ThreeWay {
    let (bs, ds, ks) = (sigs(base), sigs(disk), sigs(kb));
    let changed = |a: &HashMap<String, Sig>, b: &HashMap<String, Sig>| -> HashSet<String> {
        let mut out = HashSet::new();
        for id in a.keys().chain(b.keys()) {
            if a.get(id) != b.get(id) {
                out.insert(id.clone());
            }
        }
        out
    };
    let changed_disk = changed(&bs, &ds);
    let changed_kb = changed(&bs, &ks);

    for id in changed_disk.intersection(&changed_kb) {
        if ds.get(id) != ks.get(id) {
            return ThreeWay::Conflict;
        }
    }

    // Disjoint: for each id take the side that changed it (else base). `disk` is the
    // structural skeleton; kb-only changes are overlaid.
    let mut merged = SyncDoc::default();
    let mut seen: HashSet<String> = HashSet::new();
    for s in disk.sections.iter().chain(kb.sections.iter()) {
        if seen.insert(s.path.clone()) {
            merged.sections.push(s.clone());
        }
    }

    let mut ids: Vec<String> = bs
        .keys()
        .chain(ds.keys())
        .chain(ks.keys())
        .cloned()
        .collect();
    ids.sort();
    ids.dedup();

    let chosen_side = |id: &str| -> &SyncDoc {
        if changed_disk.contains(id) {
            disk
        } else if changed_kb.contains(id) {
            kb
        } else {
            base
        }
    };

    let mut present: HashSet<String> = HashSet::new();
    for id in &ids {
        let side = chosen_side(id);
        if let Some(item) = side.items.iter().find(|i| &i.local_id == id) {
            merged.items.push(item.clone());
            present.insert(id.clone());
        }
    }

    // Emit each edge from its *owner's* chosen side, so a per-item edit picks up its own
    // edges: `depends_on` is owned by its `src` (the dependent), `parent_of` by its `dst`
    // (the child — its indentation). Taking every edge by `src` alone drops a re-parented
    // child's incoming edge when only the child changed (its parent item stays on `base`).
    for id in &ids {
        let side = chosen_side(id);
        for e in &side.edges {
            let owner = match e.edge_type {
                EdgeType::ParentOf => &e.dst,
                _ => &e.src,
            };
            if owner == id {
                merged.edges.push(e.clone());
            }
        }
    }
    merged
        .edges
        .retain(|e| present.contains(&e.src) && present.contains(&e.dst));

    // Take the LAYOUT (block order + prose) wholesale from the disk side, which is the
    // structural skeleton this merge is built on and which *is* the file's own text. Merging
    // ordinals from three different parses is exactly what used to put a `##` header in the
    // middle of an item; one side's layout is coherent by construction. Blocks naming items
    // that did not survive the merge are dropped, and `render` appends anything the layout
    // does not mention, so a KB-only item is never lost.
    let source = if disk.layout.is_empty() { kb } else { disk };
    merged.layout = source
        .layout
        .iter()
        .filter(|b| match b {
            SyncBlock::Item(id) => present.contains(id),
            _ => true,
        })
        .cloned()
        .collect();
    ThreeWay::Merged(merged)
}

/// Parse the last-synced base bytes back into a document for three-way merge; an empty
/// document if the base blob is missing.
fn load_base_doc(
    conn: &Connection,
    journal: Option<&sync_state::SyncState>,
    serializer: &dyn SyncSerializer,
) -> Result<Option<SyncDoc>> {
    let Some(hash) = journal.and_then(|j| j.base_blob_hash.as_deref()) else {
        return Ok(None);
    };
    match blob::load(conn, hash)? {
        Some(bytes) => Ok(Some(serializer.parse(&bytes)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// The current `local_id -> ItemId` map for a file. Resolves every binding in one query.
fn existing_by_local(conn: &Connection, bare_uri: &str) -> Result<HashMap<String, ItemId>> {
    let uris = binding::synced_uris_for_file(conn, bare_uri)?;
    let uri_ids = binding::items_for_uris(conn, &uris)?;
    let mut out = HashMap::new();
    for uri in &uris {
        if let Some(&id) = uri_ids.get(uri) {
            out.insert(local_of(bare_uri, uri), id);
        }
    }
    Ok(out)
}

/// The item ids currently bound to a file (for stamping on export). Resolves every
/// binding in one query; ids stay in the `synced_uris_for_file` order.
fn current_bindings(conn: &Connection, bare_uri: &str) -> Result<Vec<ItemId>> {
    let uris = binding::synced_uris_for_file(conn, bare_uri)?;
    let uri_ids = binding::items_for_uris(conn, &uris)?;
    let mut out = Vec::new();
    for uri in &uris {
        if let Some(&id) = uri_ids.get(uri) {
            out.push(id);
        }
    }
    Ok(out)
}

/// The `kind` of an item, if it exists.
fn item_kind(conn: &Connection, id: ItemId) -> Result<Option<String>> {
    let kind: Option<String> = conn
        .prepare_cached("SELECT kind FROM items WHERE id = ?1")?
        .query_row([id.get()], |r| r.get(0))
        .optional()?;
    Ok(kind)
}

/// The binding uri for an item: `file://<path>` for the document single item (empty
/// `local_id`), else `file://<path>#<local_id>`.
fn item_uri(bare_uri: &str, local_id: &str) -> String {
    if local_id.is_empty() {
        bare_uri.to_owned()
    } else {
        format!("{bare_uri}#{local_id}")
    }
}

/// The `local_id` encoded in a binding `uri` relative to its file's `bare_uri`.
fn local_of(bare_uri: &str, uri: &str) -> String {
    uri.strip_prefix(bare_uri)
        .and_then(|rest| rest.strip_prefix('#'))
        .unwrap_or("")
        .to_owned()
}

/// The mirror namespace for a file: the mount namespace plus the file's parent
/// directories (the filename is the item/section root, not a namespace segment).
fn namespace_for(ctx: &Ctx, path: &Path) -> String {
    let rel = path.strip_prefix(&ctx.dir).unwrap_or(path);
    let mut parts = vec![ctx.mount_ns.clone()];
    if let Some(parent) = rel.parent() {
        for comp in parent.components() {
            if let Component::Normal(seg) = comp {
                parts.push(seg.to_string_lossy().into_owned());
            }
        }
    }
    parts.join("/")
}

/// `ns_path` relative to `base` (drops the `base/` prefix).
fn relative(base: &str, ns_path: &str) -> String {
    ns_path
        .strip_prefix(base)
        .and_then(|r| r.strip_prefix('/'))
        .unwrap_or(ns_path)
        .to_owned()
}

/// `path` relative to `dir` as a forward-slash string, for glob matching.
fn rel_str(dir: &Path, path: &Path) -> String {
    path.strip_prefix(dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A `file://<absolute path>` uri.
fn file_uri(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

/// blake3 of `bytes` as lowercase hex (the sync hash; same scheme as the blob store).
fn hash(bytes: &[u8]) -> String {
    blob::hash_bytes(bytes)
}

/// Write `bytes` to `path`, creating parent directories as needed.
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Map a mount `sync_mode` string to a [`SyncMode`] (unknown → bidirectional).
fn sync_mode_of(s: &str) -> SyncMode {
    match s {
        "import" => SyncMode::Import,
        "export" => SyncMode::Export,
        _ => SyncMode::Bidirectional,
    }
}
