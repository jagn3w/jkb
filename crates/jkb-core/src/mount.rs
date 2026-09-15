//! Mount repository: bind a namespace subtree to an external backing root and a
//! serializer (design D3, D24).

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use jkb_types::{ConflictPolicy, NamespaceId, SyncMode};

use crate::changelog::Entity;
use crate::store::WriteMeta;
use crate::{changelog, Result};

/// A mount's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// External backing root (`file:///…`).
    pub backing_uri: String,
    /// Sync direction for the subtree.
    pub sync_mode: String,
    /// Default serializer for files under the mount.
    pub serializer: String,
    /// Glob of files to include.
    pub include_glob: Option<String>,
    /// Glob of files to exclude.
    pub exclude_glob: Option<String>,
    /// Conflict resolution policy.
    pub conflict_policy: String,
}

/// Create (or replace) a mount on `namespace`, marking the namespace `kind='mount'`.
///
/// # Errors
/// Returns an error if a statement or the changelog append fails.
#[allow(clippy::too_many_arguments)]
pub fn create(
    conn: &Connection,
    meta: &WriteMeta,
    namespace: NamespaceId,
    backing_uri: &str,
    sync_mode: SyncMode,
    serializer: &str,
    include_glob: Option<&str>,
    exclude_glob: Option<&str>,
    conflict_policy: ConflictPolicy,
) -> Result<()> {
    // This is create-or-replace, and `jkb mount create` is also the update command — so
    // whether this is an insert has to be established before writing, not assumed. Logging an
    // update as an `insert` made `undo` take the generic insert inverse and
    // `DELETE FROM mounts`, destroying a mount that existed before the transaction and
    // leaving its `file://` bindings with nothing to sync them.
    let before = get(conn, namespace)?;
    conn.prepare_cached(
        "INSERT INTO mounts
             (namespace_id, backing_uri, sync_mode, serializer, include_glob, exclude_glob, conflict_policy)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(namespace_id) DO UPDATE SET
             backing_uri = excluded.backing_uri, sync_mode = excluded.sync_mode,
             serializer = excluded.serializer, include_glob = excluded.include_glob,
             exclude_glob = excluded.exclude_glob, conflict_policy = excluded.conflict_policy",
    )?
    .execute(params![
        namespace.get(),
        backing_uri,
        sync_mode.as_str(),
        serializer,
        include_glob,
        exclude_glob,
        conflict_policy.as_str()
    ])?;
    conn.prepare_cached("UPDATE namespaces SET kind = 'mount' WHERE id = ?1")?
        .execute([namespace.get()])?;
    let after = json!({
        "namespace_id": namespace.get(),
        "backing_uri": backing_uri,
        "sync_mode": sync_mode.as_str(),
        "serializer": serializer,
        "include_glob": include_glob,
        "exclude_glob": exclude_glob,
        "conflict_policy": conflict_policy.as_str(),
    });
    let before_json = before.as_ref().map(|m| {
        json!({
            "namespace_id": namespace.get(),
            "backing_uri": m.backing_uri,
            "sync_mode": m.sync_mode,
            "serializer": m.serializer,
            "include_glob": m.include_glob,
            "exclude_glob": m.exclude_glob,
            "conflict_policy": m.conflict_policy,
        })
    });
    changelog::upsert(
        conn,
        meta,
        Entity::Mounts,
        &namespace.get().to_string(),
        before_json.as_ref(),
        Some(&after),
    )?;
    Ok(())
}

/// Fetch the mount configured on `namespace`, if any.
///
/// # Errors
/// Returns an error if the query fails.
pub fn get(conn: &Connection, namespace: NamespaceId) -> Result<Option<Mount>> {
    let mount = conn
        .prepare_cached(
            "SELECT backing_uri, sync_mode, serializer, include_glob, exclude_glob, conflict_policy
             FROM mounts WHERE namespace_id = ?1",
        )?
        .query_row([namespace.get()], |row| {
            Ok(Mount {
                backing_uri: row.get(0)?,
                sync_mode: row.get(1)?,
                serializer: row.get(2)?,
                include_glob: row.get(3)?,
                exclude_glob: row.get(4)?,
                conflict_policy: row.get(5)?,
            })
        })
        .optional()?;
    Ok(mount)
}

/// The `file://` mount whose directory covers `path` most closely, with its namespace path. `None` when
/// no mount covers it. The one copy of this lookup: [`ambient_namespace`], `binding::serializer_for` and
/// `jkb_sync::filed_task_problem` all find a path's mount by it.
///
/// Two mounts may cover one directory (`jkb mount create` allows it, and a repo is commonly mounted as
/// documents and as tasks). Between those, a mount whose serializer is `prefer` wins, then the lowest
/// namespace path — the order `jkb mount ls` lists them in. It was the order rows came back from an
/// unordered query, so a `tasks` line could be judged by the `document` mount beside it and skip the
/// tasks-file rules.
///
/// # Errors
/// Returns an error if the query fails.
pub fn covering(
    conn: &Connection,
    path: &Path,
    prefer: Option<&str>,
) -> Result<Option<(String, Mount)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT n.path, m.backing_uri, m.sync_mode, m.serializer, m.include_glob, m.exclude_glob,
                m.conflict_policy
         FROM mounts m JOIN namespaces n ON n.id = m.namespace_id
         WHERE m.backing_uri LIKE 'file://%'
         ORDER BY n.path",
    )?;
    let mounts = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Mount {
                    backing_uri: row.get(1)?,
                    sync_mode: row.get(2)?,
                    serializer: row.get(3)?,
                    include_glob: row.get(4)?,
                    exclude_glob: row.get(5)?,
                    conflict_policy: row.get(6)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Ranked by (directory length, preferred serializer); ties keep the first by path.
    let mut best: Option<((usize, bool), String, Mount)> = None;
    for (ns, mount) in mounts {
        let Some(dir) = mount.backing_uri.strip_prefix("file://") else {
            continue;
        };
        let dir = dir.trim_end_matches('/');
        let rank = (dir.len(), prefer == Some(mount.serializer.as_str()));
        if path.starts_with(dir) && best.as_ref().is_none_or(|(best, _, _)| rank > *best) {
            best = Some((rank, ns, mount));
        }
    }
    Ok(best.map(|(_, ns, mount)| (ns, mount)))
}

/// The namespace paths of every configured mount, ordered by path. Backs the
/// all-mounts watcher (`jkb sync --watch` with no namespace).
///
/// # Errors
/// Returns an error if the query fails.
pub fn all_paths(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT n.path FROM mounts m JOIN namespaces n ON n.id = m.namespace_id ORDER BY n.path",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Every mount as `(namespace path, config)`, ordered by path — for `jkb mount ls`.
///
/// # Errors
/// Returns an error if the query fails.
pub fn all(conn: &Connection) -> Result<Vec<(String, Mount)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT n.path, m.backing_uri, m.sync_mode, m.serializer, m.include_glob,
                m.exclude_glob, m.conflict_policy
         FROM mounts m JOIN namespaces n ON n.id = m.namespace_id
         ORDER BY n.path",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Mount {
                backing_uri: r.get(1)?,
                sync_mode: r.get(2)?,
                serializer: r.get(3)?,
                include_glob: r.get(4)?,
                exclude_glob: r.get(5)?,
                conflict_policy: r.get(6)?,
            },
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The `tasks.md` a task homed at `home_ns` is written to: the nearest mount at or above that
/// namespace, when it is a `file://` mount with the `tasks` serializer. `None` when the nearest mount
/// is anything else, or there is none. The one copy — `jkb-sync`'s `tasks_mount_file` and `jkb-api`'s
/// `task.add` both ask it.
///
/// # Errors
/// Returns an error if a read fails.
pub fn tasks_file_for(conn: &Connection, home_ns: &str) -> Result<Option<String>> {
    // Normalized once and walked by slicing: a refusal for a malformed or oversized home comes from
    // here rather than from each ancestor's lookup.
    let normalized = crate::ns::normalize(home_ns)?;
    let mut cur = Some(normalized);
    while let Some(path) = cur {
        if let Some(ns_id) = crate::ns::get(conn, &path)? {
            if let Some(m) = get(conn, ns_id)? {
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
}

/// Resolve the ambient namespace for a filesystem path: the mirror namespace of the
/// most specific `file://` mount whose backing directory contains `fs_path` (design
/// D-shared / task 8.5). Returns `None` if `fs_path` is under no mount.
///
/// The CLI passes the current directory here to default a query/task scope to the
/// repo the user is standing in (overridable with `--global`).
///
/// # Errors
/// Returns an error if the query fails.
pub fn ambient_namespace(conn: &Connection, fs_path: &Path) -> Result<Option<String>> {
    Ok(covering(conn, fs_path, None)?.map(|(ns_path, _)| ns_path))
}

#[cfg(test)]
mod tests {
    use super::{ambient_namespace, create, get};
    use crate::{ns, Db};
    use jkb_types::{ConflictPolicy, SyncMode};
    use std::path::Path;

    /// Two mounts over one directory resolve the same way every time: the preferred serializer, then
    /// the lowest namespace path, whatever order the rows were created in.
    #[test]
    fn two_mounts_over_one_directory_resolve_by_a_stated_rule() {
        for order in [["tasks/jkb", "repos/jkb"], ["repos/jkb", "tasks/jkb"]] {
            let db = Db::open_in_memory().unwrap();
            db.write_txn("t", move |c, m| {
                for path in order {
                    let serializer = if path.starts_with("tasks") {
                        "tasks"
                    } else {
                        "document"
                    };
                    let id = ns::ensure(c, path)?;
                    create(
                        c,
                        m,
                        id,
                        "file:///r/jkb",
                        SyncMode::Bidirectional,
                        serializer,
                        None,
                        None,
                        ConflictPolicy::Manual,
                    )?;
                }
                let file = Path::new("/r/jkb/tasks.md");
                assert_eq!(
                    super::covering(c, file, Some("tasks"))?
                        .map(|(ns, _)| ns)
                        .as_deref(),
                    Some("tasks/jkb")
                );
                assert_eq!(
                    super::covering(c, file, None)?.map(|(ns, _)| ns).as_deref(),
                    Some("repos/jkb")
                );
                assert_eq!(ambient_namespace(c, file)?.as_deref(), Some("repos/jkb"));
                Ok(())
            })
            .unwrap();
        }
    }

    #[test]
    fn creating_a_mount_marks_the_namespace_and_roundtrips() {
        let db = Db::open_in_memory().unwrap();
        let ns_id = db
            .write_txn("t", |conn, meta| {
                let ns_id = ns::ensure(conn, "docs/monorepo")?;
                create(
                    conn,
                    meta,
                    ns_id,
                    "file:///Users/jagnew/repos/monorepo",
                    SyncMode::Bidirectional,
                    "document",
                    Some("**/*.md"),
                    None,
                    ConflictPolicy::Manual,
                )?;
                Ok(ns_id)
            })
            .unwrap();

        let mount = db.read(move |conn| get(conn, ns_id)).unwrap().unwrap();
        assert_eq!(mount.backing_uri, "file:///Users/jagnew/repos/monorepo");
        assert_eq!(mount.serializer, "document");
        assert_eq!(mount.sync_mode, "bidirectional");

        let kind: String = db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT kind FROM namespaces WHERE id = ?1",
                    [ns_id.get()],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(kind, "mount");
    }

    #[test]
    fn ambient_namespace_picks_the_most_specific_mount() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            for (path, dir) in [
                ("repos/mono", "file:///Users/j/repos/mono"),
                ("repos/mono/backend", "file:///Users/j/repos/mono/backend"),
            ] {
                let ns_id = ns::ensure(conn, path)?;
                create(
                    conn,
                    meta,
                    ns_id,
                    dir,
                    SyncMode::Bidirectional,
                    "document",
                    None,
                    None,
                    ConflictPolicy::Manual,
                )?;
            }
            Ok(())
        })
        .unwrap();

        // A path inside the backend subdir resolves to the more specific mount.
        let scope = db
            .read(|conn| {
                ambient_namespace(conn, Path::new("/Users/j/repos/mono/backend/src/lib.rs"))
            })
            .unwrap();
        assert_eq!(scope.as_deref(), Some("repos/mono/backend"));

        // A path in the repo root but outside backend resolves to the outer mount.
        let scope = db
            .read(|conn| ambient_namespace(conn, Path::new("/Users/j/repos/mono/README.md")))
            .unwrap();
        assert_eq!(scope.as_deref(), Some("repos/mono"));

        // A path under no mount resolves to nothing (→ global scope).
        let scope = db
            .read(|conn| ambient_namespace(conn, Path::new("/tmp/elsewhere")))
            .unwrap();
        assert_eq!(scope, None);
    }
}
