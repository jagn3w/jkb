//! The task-mutate set (tasks S6.2): the task writes a container agent needs, as typed operations.
//!
//! As with the read set ([`crate::kb`]), **each function here is the one implementation of its
//! write**: the host CLI runs it through a `LocalBackend` and the dev container through `jkb serve`.
//! Every write is pure database work, but one kind of database write is not contained by the database
//! (design r3.2 H4): a task **bound to a file** is written back to that file by the host's `jkb sync
//! --watch`. So a backend given [`FileRoots`] — what `jkb serve` gives its clients — refuses any write
//! to a task whose binding is a file outside those roots, refuses creating a task bound to one, and
//! refuses binding a task to a file at all. The host CLI's backend has no roots and is refused
//! nothing.
//!
//! Not in the set, on purpose: `task start`/`work`/`land`/`abandon`/`gate`/`sessions` (git, the
//! session record store and the stored gate command, which the container must run itself — a later
//! stage), `task reclaim` (it probes whether owners' processes are alive, which only their host can
//! answer), and `task mirror` (a sweep over every task).

use std::path::{Component, Path, PathBuf};

use jkb_core::lifecycle;
use jkb_core::location::{self, BranchWrite};
use jkb_core::transition;
use jkb_core::{binding, claim, edge, item, mount, ns, placement, tag, task};
use jkb_types::{EdgeType, ItemId, PlacementRole, SyncMode};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::{ApiError, ErrorCode};

/// The directories a backend's clients may cause host files to be written under, through a task's
/// file binding. `jkb serve` gives its clients the host's `~/repos` — the one host directory the dev
/// container binds (`.container/container.json`), so a sync write there is a write the container
/// could already make itself. Anything else is a host file the container cannot see, and a database
/// row must not choose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRoots(Vec<PathBuf>);

impl FileRoots {
    /// These directories, which must be absolute.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self(roots.into_iter().filter(|r| r.is_absolute()).collect())
    }

    /// Whether a binding `uri` writes no host file outside these roots: not a `file://` uri at all,
    /// or one whose path lies under a root. Judged by the path's components, without touching the
    /// filesystem: a path with `.` or `..` in it, or a relative one, is outside every root.
    ///
    /// Symlinks inside a root are not followed. A binding is only ever made from a mount's directory,
    /// which the host's user created and `jkb mount create` canonicalized, and neither a mount nor a
    /// file binding can be made through a backend with roots.
    #[must_use]
    pub fn admits(&self, uri: &str) -> bool {
        let Some(rest) = uri.strip_prefix("file://") else {
            return true;
        };
        // Only a trailing `#<local id>` is a fragment. A `#` anywhere else is in the path itself, and
        // a path judged by what precedes its first `#` could be under a root while the file is not.
        let path = match rest.rsplit_once('#') {
            Some((path, fragment)) if !fragment.contains('/') => path,
            _ => rest,
        };
        let path = Path::new(path);
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return false;
        }
        self.0.iter().any(|root| path.starts_with(root))
    }
}

fn forbidden(what: impl Into<String>) -> ApiError {
    ApiError::with_code(ErrorCode::Forbidden, what)
}

fn no_item(reference: &str) -> ApiError {
    ApiError::with_code(ErrorCode::NotFound, format!("no item with uid {reference}"))
}

/// The task a reference names, refused under `roots` when its binding is a file outside them.
fn writable(
    conn: &Connection,
    reference: &str,
    roots: Option<&FileRoots>,
) -> Result<ItemId, ApiError> {
    let id = task::resolve_ref(conn, reference)?.ok_or_else(|| no_item(reference))?;
    let Some(roots) = roots else {
        return Ok(id);
    };
    let refuse = |file: &str| {
        forbidden(format!(
            "{reference} is filed in {file}, outside the directories this client may cause host \
             files to be written in — the host's sync would write the change there. Run it on the \
             host."
        ))
    };
    if let Some(bound) = binding::get(conn, id)? {
        if !roots.admits(&bound.uri) {
            return Err(refuse(&bound.uri));
        }
    }
    // Its uid too: a task taken out of a file is rebound `managed:` but keeps the `file://` uid it was
    // parsed with, and when the line comes back sync re-attaches it by that uid — carrying whatever
    // was written to it meanwhile back into the file.
    if let Some(meta) = item::get(conn, id)? {
        if !roots.admits(&meta.uid) {
            return Err(refuse(&meta.uid));
        }
    }
    Ok(id)
}

/// The largest body a task write may leave. An append loop otherwise grew one item — and its
/// changelog, which logs the before-state each round — without bound.
pub const MAX_CONTENT_BYTES: usize = 256 * 1024;

/// The most `+ns`, `#facet=value` and `^dep` modifiers one quick-add line may carry. Each is a
/// namespace chain, a tag or an edge written in the one transaction; a megabyte line of them was
/// hundreds of thousands of rows.
pub const MAX_QUICK_ADD_MODIFIERS: usize = 64;

/// The longest tag (`facet=value`) or due date a client may set.
pub const MAX_FIELD_BYTES: usize = 1024;

fn check_len(what: &str, value: &str, max: usize) -> Result<(), ApiError> {
    if value.len() > max {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            format!("{what} of at most {max} bytes"),
        ));
    }
    Ok(())
}

/// The longest claim owner id a client may send. An owner is stored on the task and on every
/// transition it takes; unbounded, a request-sized owner claimed and released in a loop grew each
/// task's history by a megabyte a round.
pub const MAX_OWNER_BYTES: usize = 512;

fn check_owner(owner: &str) -> Result<(), ApiError> {
    if owner.is_empty() || owner.len() > MAX_OWNER_BYTES {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            format!("an owner id of 1 to {MAX_OWNER_BYTES} bytes"),
        ));
    }
    Ok(())
}

/// How `task.tag` treats the facet's other values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagMode {
    /// Add this value beside any others.
    Add,
    /// Make it the facet's only value.
    Set,
    /// Remove it.
    Rm,
}

/// What `task.add` was asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // a request's flags, not state
#[serde(deny_unknown_fields)]
pub struct AddAsk {
    /// The quick-add line: `"text" !p1 @2026-07-15 +ns #facet=value ^dep-uid`.
    pub text: String,
    /// Home the task at exactly this namespace.
    #[serde(default)]
    pub home: Option<String>,
    /// Make it a subtask of this task.
    #[serde(default)]
    pub under: Option<String>,
    /// Home it in the ambient repo's backlog.
    #[serde(default)]
    pub backlog: bool,
    /// Outside any repo, `backlog` may home it in the global backlog — the client asked its user.
    #[serde(default)]
    pub global_backlog: bool,
    /// Require a synced file binding.
    #[serde(default)]
    pub sync: bool,
    /// Force a `managed:` binding.
    #[serde(default)]
    pub managed: bool,
    /// The client's working directory, for the ambient repo ([`crate::kb::ambient`]).
    #[serde(default)]
    pub cwd: String,
    /// The client's `$HOME`.
    #[serde(default)]
    pub client_home: String,
}

/// Why `task.add` created nothing.
#[derive(Debug)]
pub enum AddFailure {
    /// Refused.
    Refused(ApiError),
    /// `backlog` outside any repo needs the user's assent to home the task in the global backlog. The
    /// whole create ran first and is rolled back, so this is answered only when the request would
    /// otherwise succeed — a client asks its user a question whose answer decides the outcome — and
    /// the one rule for what counts as an explicit placement stays here.
    NeedsGlobalBacklogAssent,
}

impl From<ApiError> for AddFailure {
    fn from(e: ApiError) -> Self {
        Self::Refused(e)
    }
}

impl From<jkb_core::Error> for AddFailure {
    fn from(e: jkb_core::Error) -> Self {
        Self::Refused(e.into())
    }
}

/// `task.add`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Added {
    /// The new task's row id.
    pub id: i64,
    /// Its uid.
    pub uid: String,
    /// Its home namespace.
    pub home: String,
    /// The `tasks.md` it is bound to, when it is file-backed.
    pub binding: Option<String>,
}

/// `task.add`: create a task from a quick-add line, homed by the rules the CLI documents — `--home`
/// over a `+ns` in the line, a subtask beside its parent, otherwise the ambient repo's inbox (or its
/// backlog) — and bound to the `tasks.md` of a `tasks` mount covering its home unless `managed`.
///
/// # Errors
/// A malformed line or ref, an `#onto=` tag, a home the rules cannot settle, `--sync` with no tasks
/// mount, [`ErrorCode::Forbidden`] for a binding outside `roots`, or a failed write.
pub fn add(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    ask: &AddAsk,
    server_home: Option<&Path>,
    roots: Option<&FileRoots>,
) -> Result<Added, AddFailure> {
    let invalid = |why: String| ApiError::with_code(ErrorCode::Invalid, why);
    check_len("a task line", &ask.text, MAX_CONTENT_BYTES)?;
    let mut qa = task::parse_quick_add(&ask.text)?;
    let modifiers = qa.placements.len() + qa.tags.len() + qa.depends_on.len();
    if modifiers > MAX_QUICK_ADD_MODIFIERS {
        return Err(invalid(format!(
            "a task line with at most {MAX_QUICK_ADD_MODIFIERS} `+ns`/`#tag`/`^dep` modifiers ({modifiers} given)"
        ))
        .into());
    }
    for (facet, value) in &qa.tags {
        // A land target is a fact about a *branch* and lives in that branch's record, so a facet
        // named `onto` reaches no reader. Refused rather than stored inert.
        if facet == "onto" {
            return Err(invalid(
                "`#onto=` records where a *branch* lands, not where a task is, so it cannot be set \
                 from a task line. Use `jkb task work <uid> --onto <branch>` or `jkb task start \
                 <uid> --branch <b> --onto <branch>`."
                    .to_owned(),
            )
            .into());
        }
        if facet == location::FACET_BRANCH {
            location::valid_ref(value)?;
        }
    }
    // `branch=` goes through `location::record_branch`, the one writer of that facet, after the task
    // exists — not through the quick-add tags.
    let branches: Vec<String> = qa
        .tags
        .iter()
        .filter(|(f, _)| f == location::FACET_BRANCH)
        .map(|(_, v)| v.clone())
        .collect();
    qa.tags.retain(|(f, _)| f != location::FACET_BRANCH);
    let mut explicit = !qa.placements.is_empty();
    let uid = task::mint_uid(&qa.title);
    let mut spec = task::NewTask::from_quick_add(uid.clone(), qa);

    // `--home` wins over a `+<ns>` in the line: it is the unambiguous form, and the only one that
    // can carry a path containing whitespace.
    if let Some(home) = &ask.home {
        spec.home.clone_from(home);
        explicit = true;
    }
    // A subtask defaults to living beside its parent.
    let parent = match &ask.under {
        Some(reference) => {
            let pid = task::resolve_ref(conn, reference)?
                .ok_or_else(|| AddFailure::Refused(no_item(reference)))?;
            if !explicit {
                if let Some(home) = item::primary_namespace(conn, pid)? {
                    spec.home = home;
                    explicit = true;
                }
            }
            Some(pid)
        }
        None => None,
    };

    let assented = settle_home(conn, ask, &mut spec, explicit, server_home)?;
    let synced = file_new_task(conn, ask, &mut spec, &uid, roots)?;

    let id = task::create(conn, meta, &spec)?;
    if let Some(parent) = parent {
        task::add_subtask(conn, meta, parent, id)?;
    }
    for branch in &branches {
        location::record_branch(conn, meta, id, branch, BranchWrite::Add)?;
    }
    // Asked last, after every write this request makes has succeeded, and answered by failing the
    // transaction so they are rolled back: a missing `^dep` or a refused placement is the answer,
    // not a question put to the user first.
    if !assented {
        return Err(AddFailure::NeedsGlobalBacklogAssent);
    }
    Ok(Added {
        id: id.get(),
        uid,
        home: spec.home,
        binding: synced,
    })
}

/// `task.add`'s homing, for a task with no explicit placement: the ambient repo's backlog with
/// `backlog` (the global one only when the client confirmed it), else the ambient repo's inbox mirrored
/// into the global inbox (D26.3), else the default home. `false` when the global backlog is where it
/// goes and the client has not confirmed it — the home is still set, so the rest can be judged.
fn settle_home(
    conn: &Connection,
    ask: &AddAsk,
    spec: &mut task::NewTask,
    explicit: bool,
    server_home: Option<&Path>,
) -> Result<bool, ApiError> {
    let invalid = |why: &str| ApiError::with_code(ErrorCode::Invalid, why);
    let ambient_repo = || crate::kb::ambient(conn, &ask.cwd, &ask.client_home, server_home);
    if explicit {
        if ask.backlog {
            return Err(invalid(
                "--backlog conflicts with an explicit placement (`--home`, or a `+<ns>` in the task \
                 line)",
            ));
        }
    } else if ask.backlog {
        let root = task::DEFAULT_ROOT;
        if let Some(repo) = ambient_repo()? {
            spec.home = format!("{root}/{repo}/.backlog");
        } else {
            spec.home = format!("{root}/.backlog");
            return Ok(ask.global_backlog);
        }
    } else if let Some(repo) = ambient_repo()? {
        spec.home = format!("{}/{repo}/inbox", task::DEFAULT_ROOT);
        spec.mirrors = vec![task::DEFAULT_HOME.to_owned()];
    }
    Ok(true)
}

/// `task.add`'s binding: the `tasks.md` of a `tasks` mount covering the home unless `managed`, refused
/// under `roots` when that file is outside them. The bare file uri, or `None` for `managed:`.
fn file_new_task(
    conn: &Connection,
    ask: &AddAsk,
    spec: &mut task::NewTask,
    uid: &str,
    roots: Option<&FileRoots>,
) -> Result<Option<String>, ApiError> {
    let synced = if ask.managed {
        None
    } else {
        mount::tasks_file_for(conn, &spec.home)?
    };
    match &synced {
        Some(file) => {
            let local = uid.strip_prefix("task:").unwrap_or(uid);
            spec.binding = format!("{file}#{local}");
            if let Some(roots) = roots {
                if !roots.admits(&spec.binding) {
                    return Err(forbidden(format!(
                        "a task homed at `{}` is written to {file}, outside the directories this \
                         client may cause host files to be written in. Add it with --managed, or \
                         run it on the host.",
                        spec.home
                    )));
                }
            }
        }
        None if ask.sync => {
            return Err(ApiError::with_code(
                ErrorCode::Invalid,
                format!(
                    "--sync: no `tasks`-serializer file mount covers the home `{}`",
                    spec.home
                ),
            ))
        }
        None => {}
    }
    Ok(synced)
}

/// `task.set`: any of status, priority and due, in one transaction.
///
/// # Errors
/// Nothing to set, an unknown or derived status, [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`]
/// under `roots`, or a failed write.
pub fn set(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    reference: &str,
    status: Option<&str>,
    priority: Option<i64>,
    due: Option<&str>,
    roots: Option<&FileRoots>,
) -> Result<(), ApiError> {
    if status.is_none() && priority.is_none() && due.is_none() {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            "nothing to set: pass at least one of --status/--priority/--due",
        ));
    }
    if let Some(d) = due {
        check_len("a due date", d, MAX_FIELD_BYTES)?;
    }
    let id = writable(conn, reference, roots)?;
    if let Some(s) = status {
        task::set_status_str(conn, meta, id, s)?;
    }
    if let Some(p) = priority {
        task::set_priority(conn, meta, id, Some(p))?;
    }
    if let Some(d) = due {
        task::set_due(conn, meta, id, Some(d))?;
    }
    Ok(())
}

/// `task.edit`: replace a task's body, or append to it, by `item::edit_content`'s rule — a task in a
/// `tasks.md` refuses a result with a line that would end its body — within [`MAX_CONTENT_BYTES`].
///
/// Answers whether the task is file-backed, so a client can say its file is written by the host's sync.
///
/// # Errors
/// A blank line in a file-backed task's text, [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`]
/// under `roots`, or a failed write.
pub fn edit(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    reference: &str,
    text: &str,
    append: bool,
    roots: Option<&FileRoots>,
) -> Result<bool, ApiError> {
    let id = writable(conn, reference, roots)?;
    Ok(item::edit_content(
        conn,
        meta,
        id,
        text,
        append,
        Some(MAX_CONTENT_BYTES),
    )?)
}

/// `task.tag`: add, set or remove `facet=value`.
///
/// # Errors
/// A tag not of the form `facet=value`, an `onto` facet being set, a `branch` value git would read as
/// an option, [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`] under `roots`, or a failed write.
pub fn tag(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    reference: &str,
    facet_value: &str,
    mode: TagMode,
    roots: Option<&FileRoots>,
) -> Result<(), ApiError> {
    let invalid = |why: &str| ApiError::with_code(ErrorCode::Invalid, why);
    check_len("a tag", facet_value, MAX_FIELD_BYTES)?;
    let (facet, value) = facet_value
        .split_once('=')
        .ok_or_else(|| invalid("tag must be `facet=value`, e.g. `size=small`"))?;
    // `rm` is not refused: removing an inert `onto=` — which a synced `tasks.md` line can still carry —
    // is always safe, and refusing it left no command able to.
    if facet == "onto" && mode != TagMode::Rm {
        return Err(invalid(
            "`onto` records where a *branch* lands, not where a task is, so it is no longer a tag. \
             Use `jkb task work <uid> --onto <branch>`, or `jkb task start <uid> --branch <b> \
             --onto <branch>`.",
        ));
    }
    let id = writable(conn, reference, roots)?;
    if facet == location::FACET_BRANCH && mode != TagMode::Rm {
        let how = if mode == TagMode::Add {
            BranchWrite::Add
        } else {
            BranchWrite::Set
        };
        location::record_branch(conn, meta, id, value, how)?;
        return Ok(());
    }
    match mode {
        // `add` is additive, honest to its name: an open-ended facet legitimately holds several.
        TagMode::Add => tag::apply(conn, meta, id, facet, value)?,
        // `set` replaces the facet's other values — for the facets with one true answer (D36.6).
        TagMode::Set => location::set_facet(conn, meta, id, facet, value)?,
        TagMode::Rm => tag::remove(conn, meta, id, facet, value)?,
    }
    Ok(())
}

/// `task.depend`: `reference` now depends on `dep` (cycle-guarded).
///
/// # Errors
/// [`ErrorCode::NotFound`], a cycle, [`ErrorCode::Forbidden`] under `roots`, or a failed write.
pub fn depend(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    reference: &str,
    dep: &str,
    roots: Option<&FileRoots>,
) -> Result<(), ApiError> {
    let id = writable(conn, reference, roots)?;
    task::add_dependency(conn, meta, id, &task::canonical_uid(dep))?;
    Ok(())
}

/// `task.undepend`: remove `reference`'s `depends_on` edge to `dep`.
///
/// # Errors
/// [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`] under `roots`, or a failed write.
pub fn undepend(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    reference: &str,
    dep: &str,
    roots: Option<&FileRoots>,
) -> Result<(), ApiError> {
    let id = writable(conn, reference, roots)?;
    let dep_id = task::resolve_ref(conn, dep)?.ok_or_else(|| no_item(dep))?;
    edge::unlink(conn, meta, id, dep_id, EdgeType::DependsOn)?;
    Ok(())
}

/// `task.place`: place a task under `ns` as a reference mirror, or as its primary home.
///
/// # Errors
/// A malformed namespace, [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`] under `roots`, or a
/// failed write.
pub fn place(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    reference: &str,
    namespace: &str,
    home: bool,
    roots: Option<&FileRoots>,
) -> Result<(), ApiError> {
    let id = writable(conn, reference, roots)?;
    let ns_id = ns::ensure(conn, namespace)?;
    if home {
        task::set_primary_home(conn, meta, id, ns_id, 0)?;
    } else {
        placement::place(conn, meta, id, ns_id, PlacementRole::Reference, 0)?;
    }
    Ok(())
}

/// `task.unplace`: remove a task's reference placement under `ns`; how many were removed (a missing
/// namespace or mirror removes none).
///
/// # Errors
/// [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`] under `roots`, or a failed write.
pub fn unplace(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    reference: &str,
    namespace: &str,
    roots: Option<&FileRoots>,
) -> Result<usize, ApiError> {
    let id = writable(conn, reference, roots)?;
    Ok(match ns::get(conn, namespace)? {
        Some(ns_id) => placement::unplace(conn, meta, id, ns_id)?,
        None => 0,
    })
}

/// `task.bind`: bind a task to `managed:` storage, or with `sync` to a file.
///
/// # Errors
/// [`ErrorCode::Forbidden`] for any `sync` binding under `roots` — a file binding is exactly how a
/// row would choose a host file for sync to read and write (design H4) — or for rebinding a task bound
/// outside them; [`ErrorCode::NotFound`]; or a failed write.
pub fn bind(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    reference: &str,
    sync: Option<&str>,
    roots: Option<&FileRoots>,
) -> Result<(), ApiError> {
    if sync.is_some() && roots.is_some() {
        return Err(forbidden(
            "binding a task to a file is refused through the daemon: the host's sync would read and \
             write that file. Run `jkb task bind --sync` on the host.",
        ));
    }
    let id = writable(conn, reference, roots)?;
    match sync {
        Some(uri) => binding::set(conn, meta, id, uri, Some(SyncMode::Bidirectional), None)?,
        None => binding::set(conn, meta, id, task::MANAGED_BINDING, None, None)?,
    }
    Ok(())
}

/// `task.claim`'s answer: whether the claim was taken, and the lifecycle's reason when it was not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claimed {
    /// The task is now claimed by the owner.
    pub acquired: bool,
    /// Why not.
    pub refusal: Option<String>,
}

/// `task.claim`: claim a task for `owner` through the lifecycle's `start` — the same transition
/// `task work` and `task start` take, so the history records it and `needs_review` is answered alike.
/// A refusal (a live or unestablished holder, a finished task) is an answer, not an error.
///
/// The machine is told nothing about the current holder's liveness, so a claimed task is refused
/// unless `owner` already holds it: proving an owner gone takes probing its process, which only its
/// own host can do (`jkb task reclaim`, there).
///
/// # Errors
/// [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`] under `roots`, or a failed write.
pub fn claim(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    reference: &str,
    owner: &str,
    roots: Option<&FileRoots>,
) -> Result<Claimed, ApiError> {
    check_owner(owner)?;
    let id = writable(conn, reference, roots)?;
    let facts = lifecycle::TaskFacts {
        actor: Some(jkb_types::AgentId::parse(owner)),
        ..task::observe(conn, id)?
    };
    let outcome = transition::perform(
        conn,
        meta,
        id,
        &facts,
        lifecycle::TaskEvent::Start,
        &transition::Labels::default(),
    )?;
    let refusal = outcome.refusal();
    Ok(Claimed {
        acquired: refusal.is_none(),
        refusal,
    })
}

/// `task.release`: drop `owner`'s claim — a compare-and-swap on the owner, so one agent cannot drop
/// another's, and outside the lifecycle: giving up a claim is not a lifecycle move.
///
/// # Errors
/// [`ErrorCode::NotFound`], [`ErrorCode::Forbidden`] under `roots`, or a failed write.
pub fn release(
    conn: &Connection,
    meta: &jkb_core::WriteMeta,
    reference: &str,
    owner: &str,
    roots: Option<&FileRoots>,
) -> Result<bool, ApiError> {
    check_owner(owner)?;
    let id = writable(conn, reference, roots)?;
    Ok(claim::release(conn, meta, id, owner)?)
}

/// One entry of a task's history, as `task.why` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// When.
    pub at: String,
    /// The transaction it was applied in.
    pub txn: String,
    /// The event.
    pub event: String,
    /// The status it moved from, when recorded.
    pub from: Option<String>,
    /// The status it moved to.
    pub to: String,
    /// Who acted.
    pub agent: Option<String>,
    /// The branch the work is on.
    pub branch: Option<String>,
    /// The branch it lands on.
    pub onto: Option<String>,
    /// The pull request that proved a landing.
    pub pr: Option<i64>,
    /// The facts the guard fired on, as JSON text.
    pub evidence: Option<String>,
}

/// `task.why`: a task's transition history, oldest first, as much of it as fits `budget`.
///
/// # Errors
/// [`ErrorCode::NotFound`], or a failed read.
pub fn why(
    conn: &Connection,
    reference: &str,
    budget: &mut crate::kb::Budget,
) -> Result<Vec<HistoryEntry>, ApiError> {
    let id = task::resolve_ref(conn, reference)?.ok_or_else(|| no_item(reference))?;
    Ok(transition::history(conn, id)?
        .into_iter()
        .map(|r| HistoryEntry {
            at: r.at,
            txn: r.txn_id,
            event: r.event,
            from: r.from_status,
            to: r.to_status,
            agent: r.agent_id.as_ref().map(ToString::to_string),
            branch: r.labels.branch,
            onto: r.labels.onto,
            pr: r.labels.pr_number,
            evidence: r.evidence,
        })
        .take_while(|entry| budget.take(entry))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::FileRoots;

    #[test]
    fn a_file_binding_is_admitted_only_under_a_root_by_its_path_s_components() {
        let roots = FileRoots::new(vec![PathBuf::from("/Users/u/repos")]);
        assert!(roots.admits("managed:"), "no file, nothing to refuse");
        assert!(roots.admits("file:///Users/u/repos/jkb/tasks.md#t1"));
        assert!(!roots.admits("file:///Users/u/Documents/tasks.md#t1"));
        assert!(
            !roots.admits("file:///Users/u/repos-other/tasks.md"),
            "a prefix of the root's name is not under it"
        );
        assert!(
            !roots.admits("file:///Users/u/repos/../.ssh/tasks.md"),
            "`..` is judged outside, not resolved"
        );
        assert!(!roots.admits("file://relative/tasks.md"));
        assert!(
            !roots.admits("file:///Users/u/repos#old/proj/tasks.md#slug"),
            "a `#` in the path is not a fragment boundary"
        );
        assert!(roots.admits("file:///Users/u/repos/proj/tasks.md#slug"));
        assert!(
            roots.admits("file:///Users/u/repos/a#b/tasks.md#slug"),
            "a `#` in a directory under the root is still under it"
        );
        assert!(
            !FileRoots::new(vec![PathBuf::from("relative")]).admits("file:///relative/x"),
            "a relative root admits nothing"
        );
    }
}
