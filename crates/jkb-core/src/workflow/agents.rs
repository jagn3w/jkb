//! Workflow agent templates: the prompts a workflow script gives its agents, as data (design D53.7,
//! `docs/code-factory.md`).
//!
//! A template is a name, the workflow that runs it, the D52 role its calls act as, the prompt with
//! `{{placeholders}}` the script fills, what the agent is allowed to do ([`AgentPermissions`]),
//! and the agents it hands its result to (the Workflows tab's graph edges).
//!
//! **Two sources, one listing**, the way strategy presets sit beside strategy definitions:
//!
//! * **packaged** templates are compiled in from [`PACKAGED_FILE`] (`agents.json` beside this
//!   module). Nothing can edit one in place — there is no row to update — and a newer jkb's text
//!   reaches every database without a migration. They were copied out of the workflow scripts'
//!   prompt functions; changing one is a contribution to that file ([`export`]).
//! * **operator copies** are rows of `workflow_agents` (V025), versioned and append-only. A copy
//!   under a packaged template's name **overrides** it: [`resolve`] answers the copy, so the
//!   script that asks for the template by name runs the operator's text.
//!
//! The template file stores each prompt as an array of lines, so a contribution's pull request
//! diffs line by line rather than as one escaped string.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::roles::Role;
use crate::store::WriteMeta;
use crate::{Error, Result};

/// The packaged-templates file, relative to the repository root: what a contribution edits.
pub const PACKAGED_FILE: &str = "crates/jkb-core/src/workflow/agents.json";

const PACKAGED_JSON: &str = include_str!("agents.json");

/// The longest template accepted, in bytes.
pub const MAX_TEMPLATE_BYTES: usize = 64 * 1024;

/// The most agents one template may hand off to.
pub const MAX_HANDOFFS: usize = 64;

/// The longest one-line description accepted, in characters.
pub const MAX_DESCRIBE_CHARS: usize = 500;

fn invalid(msg: impl Into<String>) -> Error {
    Error::Types(jkb_types::Error::Validation(msg.into()))
}

/// Where an agent runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// In the caller's checkout.
    None,
    /// In a git worktree of its own.
    Worktree,
}

/// The most an agent may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Writes {
    /// Nothing: it reads and reports.
    Nothing,
    /// Knowledge-base bookkeeping through `jkb` (claims, statuses, branch records).
    Kb,
    /// Git refs, through a script it runs (the merge queue), but no code of its own.
    Git,
    /// Code: it edits files and commits.
    Code,
}

/// What an agent is allowed to do, beyond what its role's op classes allow in jkb.
///
/// `deny_unknown_fields`, like a strategy spec: a permission a newer jkb added is refused by an
/// older one rather than ignored, since ignoring it could only mean a laxer agent than was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPermissions {
    /// Where it runs.
    pub isolation: Isolation,
    /// The model it runs on; `None` is the session's own.
    #[serde(default)]
    pub model: Option<String>,
    /// The most it may change.
    pub writes: Writes,
}

/// What one version of a template says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    /// The workflow script that runs it (`task-swarm`, `code-review`).
    pub workflow: String,
    /// The role its calls act as.
    pub role: Role,
    /// A piece other templates include, not an agent of its own: the graph does not draw it.
    pub fragment: bool,
    /// One line on what it does.
    pub describe: String,
    /// The prompt, with `{{placeholders}}`.
    pub template: String,
    /// What it may do.
    pub permissions: AgentPermissions,
    /// The agents it hands its result to.
    pub hands_off_to: Vec<String>,
}

/// Where a template comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Compiled in from [`PACKAGED_FILE`]; read-only.
    Packaged,
    /// An operator copy, stored; editable.
    Operator,
}

/// One version of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    /// Its name.
    pub name: String,
    /// Its version: the packaged file's for a packaged template, the row's for a copy.
    pub version: i64,
    /// Packaged or an operator copy.
    pub source: Source,
    /// What it says.
    pub def: AgentDef,
    /// What a copy was made from: `packaged:<name>@<v>` or `<name>@<v>`. `None` for a packaged
    /// template and for an edit, which follows the version before it.
    pub based_on: Option<String>,
    /// When a copy's version was written; `None` for a packaged template.
    pub defined_at: Option<String>,
}

/// A template as the listing shows it: the one in effect, and how it stands against the packaged
/// template of its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The template in effect: the newest operator copy, else the packaged one.
    pub agent: Agent,
    /// The packaged template's version, when there is one of this name.
    pub packaged_version: Option<i64>,
    /// An operator copy is overriding a packaged template.
    pub overrides_packaged: bool,
    /// The copy was taken from an older packaged version than the one compiled in now.
    pub behind_packaged: bool,
}

// -------------------------------------------------------------------------------------------
// Validation
// -------------------------------------------------------------------------------------------

/// Refuse a name that is not 1 to 64 of `a-z`, `0-9` and `-`, starting with a letter.
///
/// # Errors
/// [`Error::Types`] naming the rule.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "an agent name of 1 to 64 lowercase letters, digits and `-`, starting with a letter \
             (got `{name}`)"
        )))
    }
}

fn is_placeholder_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// One piece of a template: text, or a placeholder.
enum Piece<'a> {
    Text(&'a str),
    Slot(&'a str),
}

/// Split a template into text and `{{placeholders}}`. Every `{{` opens one, which must close with
/// `}}` around a name of `a-z`, `0-9` and `_`: a template that cannot be filled unambiguously is
/// refused when it is saved, not when a script first fills it.
fn pieces(template: &str) -> Result<Vec<Piece<'_>>> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        if open > 0 {
            out.push(Piece::Text(&rest[..open]));
        }
        let after = &rest[open + 2..];
        let close = after.find("}}").ok_or_else(|| {
            invalid("a `{{` with no closing `}}` — a placeholder is `{{name}}`".to_owned())
        })?;
        let name = &after[..close];
        if !is_placeholder_name(name) {
            return Err(invalid(format!(
                "`{{{{{name}}}}}` is not a placeholder: a name of lowercase letters, digits and `_`"
            )));
        }
        out.push(Piece::Slot(name));
        rest = &after[close + 2..];
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest));
    }
    Ok(out)
}

/// The placeholders a template names, sorted and without repeats.
///
/// # Errors
/// [`Error::Types`] for a malformed placeholder.
pub fn placeholders(template: &str) -> Result<Vec<String>> {
    let names: BTreeSet<&str> = pieces(template)?
        .into_iter()
        .filter_map(|p| match p {
            Piece::Slot(n) => Some(n),
            Piece::Text(_) => None,
        })
        .collect();
    Ok(names.into_iter().map(str::to_owned).collect())
}

/// Fill a template's placeholders from `vars`. Every placeholder needs a value and every value a
/// placeholder: a script passing a name the template no longer has, or missing one it gained, is
/// told so rather than running a prompt with a hole or a stray value in it.
///
/// # Errors
/// [`Error::Types`] naming the missing or unknown placeholders, or a malformed template.
pub fn render(template: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let parts = pieces(template)?;
    let named: BTreeSet<&str> = parts
        .iter()
        .filter_map(|p| match p {
            Piece::Slot(n) => Some(*n),
            Piece::Text(_) => None,
        })
        .collect();
    let missing: Vec<&str> = named
        .iter()
        .copied()
        .filter(|n| !vars.contains_key(*n))
        .collect();
    let unknown: Vec<&str> = vars
        .keys()
        .map(String::as_str)
        .filter(|k| !named.contains(k))
        .collect();
    if !missing.is_empty() || !unknown.is_empty() {
        let mut why = Vec::new();
        if !missing.is_empty() {
            why.push(format!("no value for {}", missing.join(", ")));
        }
        if !unknown.is_empty() {
            why.push(format!("no placeholder named {}", unknown.join(", ")));
        }
        return Err(invalid(format!(
            "cannot fill the template: {}",
            why.join("; ")
        )));
    }
    let mut out = String::with_capacity(template.len());
    for p in parts {
        match p {
            Piece::Text(t) => out.push_str(t),
            Piece::Slot(n) => out.push_str(&vars[n]),
        }
    }
    Ok(out)
}

impl AgentDef {
    /// Refuse a definition that could not mean what it says.
    ///
    /// # Errors
    /// [`Error::Types`] naming the first problem.
    pub fn validate(&self) -> Result<()> {
        let workflow_ok = !self.workflow.is_empty()
            && self.workflow.len() <= 64
            && self
                .workflow
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !workflow_ok {
            return Err(invalid(format!(
                "a workflow name of 1 to 64 lowercase letters, digits and `-` (got `{}`)",
                self.workflow
            )));
        }
        if self.describe.trim().is_empty() || self.describe.chars().count() > MAX_DESCRIBE_CHARS {
            return Err(invalid(format!(
                "a description of 1 to {MAX_DESCRIBE_CHARS} characters"
            )));
        }
        if self.template.trim().is_empty() {
            return Err(invalid("an empty template"));
        }
        if self.template.len() > MAX_TEMPLATE_BYTES {
            return Err(invalid(format!(
                "a template of at most {MAX_TEMPLATE_BYTES} bytes (got {})",
                self.template.len()
            )));
        }
        placeholders(&self.template)?;
        if let Some(model) = &self.permissions.model {
            let ok = !model.is_empty()
                && model.len() <= 64
                && model
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
            if !ok {
                return Err(invalid(format!(
                    "a model name of 1 to 64 letters, digits, `-`, `_` and `.` (got `{model}`)"
                )));
            }
        }
        if self.hands_off_to.len() > MAX_HANDOFFS {
            return Err(invalid(format!(
                "at most {MAX_HANDOFFS} hand-offs (got {})",
                self.hands_off_to.len()
            )));
        }
        let mut seen = BTreeSet::new();
        for to in &self.hands_off_to {
            validate_name(to)?;
            if !seen.insert(to) {
                return Err(invalid(format!("`{to}` is named twice in hands_off_to")));
            }
        }
        if self.fragment && !self.hands_off_to.is_empty() {
            return Err(invalid(
                "a fragment is part of other templates and hands off to nothing",
            ));
        }
        Ok(())
    }
}

// -------------------------------------------------------------------------------------------
// The packaged file
// -------------------------------------------------------------------------------------------

/// One entry of [`PACKAGED_FILE`], in its stored form. The field order is the file's.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagedEntry {
    name: String,
    version: i64,
    workflow: String,
    role: Role,
    #[serde(default)]
    fragment: bool,
    describe: String,
    permissions: AgentPermissions,
    #[serde(default)]
    hands_off_to: Vec<String>,
    /// The prompt, one element per line.
    template: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagedFile {
    agents: Vec<PackagedEntry>,
}

impl PackagedEntry {
    fn into_agent(self) -> Agent {
        Agent {
            name: self.name,
            version: self.version,
            source: Source::Packaged,
            def: AgentDef {
                workflow: self.workflow,
                role: self.role,
                fragment: self.fragment,
                describe: self.describe,
                template: self.template.join("\n"),
                permissions: self.permissions,
                hands_off_to: self.hands_off_to,
            },
            based_on: None,
            defined_at: None,
        }
    }

    fn from_agent(name: &str, version: i64, def: &AgentDef) -> Self {
        Self {
            name: name.to_owned(),
            version,
            workflow: def.workflow.clone(),
            role: def.role,
            fragment: def.fragment,
            describe: def.describe.clone(),
            permissions: def.permissions.clone(),
            hands_off_to: def.hands_off_to.clone(),
            template: def.template.split('\n').map(str::to_owned).collect(),
        }
    }
}

fn parse_file(json: &str) -> Result<PackagedFile> {
    let file: PackagedFile = serde_json::from_str(json)
        .map_err(|e| invalid(format!("the packaged-templates file does not parse: {e}")))?;
    let mut names = BTreeSet::new();
    for e in &file.agents {
        validate_name(&e.name).map_err(|err| invalid(format!("packaged `{}`: {err}", e.name)))?;
        if !names.insert(e.name.as_str()) {
            return Err(invalid(format!(
                "the packaged-templates file names `{}` twice",
                e.name
            )));
        }
        if e.version < 1 {
            return Err(invalid(format!(
                "packaged `{}` has version {}, not 1 or more",
                e.name, e.version
            )));
        }
        e.clone()
            .into_agent()
            .def
            .validate()
            .map_err(|err| invalid(format!("packaged `{}`: {err}", e.name)))?;
    }
    Ok(file)
}

/// The file in its canonical form: two-space JSON and a final newline, what [`export`] writes.
fn write_file(file: &PackagedFile) -> Result<String> {
    let mut s = serde_json::to_string_pretty(file).map_err(|e| invalid(e.to_string()))?;
    s.push('\n');
    Ok(s)
}

/// Every packaged template, in the file's order.
///
/// # Errors
/// [`Error::Types`] if the compiled-in file is malformed — which a test refuses before it can
/// ship, so in practice never.
pub fn packaged() -> Result<&'static [Agent]> {
    static PACKAGED: OnceLock<std::result::Result<Vec<Agent>, String>> = OnceLock::new();
    PACKAGED
        .get_or_init(|| {
            parse_file(PACKAGED_JSON)
                .map(|f| {
                    f.agents
                        .into_iter()
                        .map(PackagedEntry::into_agent)
                        .collect()
                })
                .map_err(|e| e.to_string())
        })
        .as_deref()
        .map_err(|e| invalid(e.clone()))
}

fn packaged_named(name: &str) -> Result<Option<&'static Agent>> {
    Ok(packaged()?.iter().find(|a| a.name == name))
}

/// Write `agent` into the packaged-templates file `file` (its current text), returning the new
/// text and the version it was packaged as: the entry of that name is replaced with the next
/// version, or appended as version 1. What *Contribute to jkb* commits.
///
/// # Errors
/// [`Error::Types`] if `file` is malformed, or already packages exactly this template — a pull
/// request that changes nothing.
pub fn export(file: &str, agent: &Agent) -> Result<(String, i64)> {
    let mut parsed = parse_file(file)?;
    agent.def.validate()?;
    let Some(entry) = parsed.agents.iter_mut().find(|e| e.name == agent.name) else {
        parsed
            .agents
            .push(PackagedEntry::from_agent(&agent.name, 1, &agent.def));
        return Ok((write_file(&parsed)?, 1));
    };
    if entry.clone().into_agent().def == agent.def {
        return Err(invalid(format!(
            "`{}` is already packaged exactly as it is — nothing to contribute",
            agent.name
        )));
    }
    let version = entry.version + 1;
    *entry = PackagedEntry::from_agent(&agent.name, version, &agent.def);
    Ok((write_file(&parsed)?, version))
}

// -------------------------------------------------------------------------------------------
// Operator copies
// -------------------------------------------------------------------------------------------

const COLUMNS: &str = "name, version, workflow, role, fragment, describe, template, permissions, \
                       hands_off_to, based_on, defined_at";

fn row_to_agent(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        name: r.get(0)?,
        version: r.get(1)?,
        workflow: r.get(2)?,
        role: r.get(3)?,
        fragment: r.get(4)?,
        describe: r.get(5)?,
        template: r.get(6)?,
        permissions: r.get(7)?,
        hands_off_to: r.get(8)?,
        based_on: r.get(9)?,
        defined_at: r.get(10)?,
    })
}

struct RawRow {
    name: String,
    version: i64,
    workflow: String,
    role: String,
    fragment: bool,
    describe: String,
    template: String,
    permissions: String,
    hands_off_to: String,
    based_on: Option<String>,
    defined_at: String,
}

impl RawRow {
    /// Read a stored row. A permission or field this jkb does not know is refused, not dropped.
    fn into_agent(self) -> Result<Agent> {
        let unreadable = |what: &str, e: &dyn std::fmt::Display| {
            invalid(format!(
                "agent `{}@{}`: its {what} cannot be read by this jkb ({e}) — a newer jkb may have \
                 written it",
                self.name, self.version
            ))
        };
        let permissions: AgentPermissions =
            serde_json::from_str(&self.permissions).map_err(|e| unreadable("permissions", &e))?;
        let hands_off_to: Vec<String> =
            serde_json::from_str(&self.hands_off_to).map_err(|e| unreadable("hand-offs", &e))?;
        let role = Role::parse(&self.role).map_err(|e| unreadable("role", &e))?;
        Ok(Agent {
            name: self.name,
            version: self.version,
            source: Source::Operator,
            def: AgentDef {
                workflow: self.workflow,
                role,
                fragment: self.fragment,
                describe: self.describe,
                template: self.template,
                permissions,
                hands_off_to,
            },
            based_on: self.based_on,
            defined_at: Some(self.defined_at),
        })
    }
}

fn newest_copy(conn: &Connection, name: &str) -> Result<Option<Agent>> {
    conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM workflow_agents WHERE name = ?1 ORDER BY version DESC LIMIT 1"
    ))?
    .query_row([name], row_to_agent)
    .optional()?
    .map(RawRow::into_agent)
    .transpose()
}

/// Every version of an operator copy, oldest first.
///
/// # Errors
/// A database error, or a row this jkb cannot read.
pub fn versions(conn: &Connection, name: &str) -> Result<Vec<Agent>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM workflow_agents WHERE name = ?1 ORDER BY version"
    ))?;
    let rows = stmt.query_map([name], row_to_agent)?;
    rows.map(|r| r.map_err(Error::from).and_then(RawRow::into_agent))
        .collect()
}

/// Which version of a template to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// The one in effect: the newest operator copy, else the packaged template.
    Effective,
    /// The packaged template, whatever overrides it.
    Packaged,
    /// This version of the operator copy.
    Version(i64),
}

fn unknown(name: &str) -> Error {
    invalid(format!(
        "no agent template `{name}` (`jkb workflow agent list` shows them)"
    ))
}

/// The template `name`, as `pick` says.
///
/// # Errors
/// [`Error::Types`] for an unknown name or version, or a stored row this jkb cannot read.
pub fn resolve(conn: &Connection, name: &str, pick: Pick) -> Result<Agent> {
    match pick {
        Pick::Effective => match newest_copy(conn, name)? {
            Some(a) => Ok(a),
            None => packaged_named(name)?.cloned().ok_or_else(|| unknown(name)),
        },
        Pick::Packaged => packaged_named(name)?.cloned().ok_or_else(|| {
            invalid(format!(
                "no packaged agent template `{name}` (an operator's own template has no packaged \
                 one)"
            ))
        }),
        Pick::Version(v) => conn
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM workflow_agents WHERE name = ?1 AND version = ?2"
            ))?
            .query_row(params![name, v], row_to_agent)
            .optional()?
            .map(RawRow::into_agent)
            .transpose()?
            .ok_or_else(|| invalid(format!("no version {v} of the operator's `{name}`"))),
    }
}

/// The packaged version a copy was last taken from, if it was taken from the packaged template of
/// its own name: what says whether the packaged text has moved on since.
fn copied_from_packaged(conn: &Connection, name: &str) -> Result<Option<i64>> {
    let based: Option<String> = conn
        .prepare_cached(
            "SELECT based_on FROM workflow_agents WHERE name = ?1 AND based_on IS NOT NULL
             ORDER BY version DESC LIMIT 1",
        )?
        .query_row([name], |r| r.get(0))
        .optional()?;
    Ok(based
        .as_deref()
        .and_then(|b| b.strip_prefix("packaged:"))
        .and_then(|b| b.strip_prefix(name))
        .and_then(|b| b.strip_prefix('@'))
        .and_then(|v| v.parse().ok()))
}

/// Every template, packaged ones in the file's order and then the operator's own by name — each as
/// the one in effect.
///
/// # Errors
/// A database error, or a stored row this jkb cannot read.
pub fn list(conn: &Connection) -> Result<Vec<Listed>> {
    let copies: Vec<String> = conn
        .prepare_cached("SELECT DISTINCT name FROM workflow_agents ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let packaged = packaged()?;
    let mut out = Vec::new();
    for p in packaged {
        out.push(listed(conn, &p.name, Some(p))?);
    }
    for name in copies
        .iter()
        .filter(|n| !packaged.iter().any(|p| p.name == **n))
    {
        out.push(listed(conn, name, None)?);
    }
    Ok(out)
}

/// The template `name` as [`list`] shows it, read alone: another template's unreadable row does not
/// stop it.
///
/// # Errors
/// [`Error::Types`] for an unknown name or a stored row of it this jkb cannot read.
pub fn standing(conn: &Connection, name: &str) -> Result<Listed> {
    listed(conn, name, packaged_named(name)?)
}

fn listed(conn: &Connection, name: &str, packaged: Option<&Agent>) -> Result<Listed> {
    let copy = newest_copy(conn, name)?;
    let behind = match (packaged, &copy) {
        (Some(p), Some(_)) => copied_from_packaged(conn, name)?.is_some_and(|v| v < p.version),
        _ => false,
    };
    let overrides = packaged.is_some() && copy.is_some();
    let agent = match copy {
        Some(c) => c,
        None => packaged.cloned().ok_or_else(|| unknown(name))?,
    };
    Ok(Listed {
        agent,
        packaged_version: packaged.map(|p| p.version),
        overrides_packaged: overrides,
        behind_packaged: behind,
    })
}

fn append(conn: &Connection, name: &str, def: &AgentDef, based_on: Option<&str>) -> Result<Agent> {
    def.validate()?;
    let permissions =
        serde_json::to_string(&def.permissions).map_err(|e| invalid(e.to_string()))?;
    let hands = serde_json::to_string(&def.hands_off_to).map_err(|e| invalid(e.to_string()))?;
    let raw = conn
        .prepare_cached(&format!(
            "INSERT INTO workflow_agents ({COLUMNS})
             VALUES (?1, (SELECT coalesce(max(version), 0) + 1 FROM workflow_agents WHERE name = ?1),
                     ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             RETURNING {COLUMNS}"
        ))?
        .query_row(
            params![
                name,
                def.workflow,
                def.role.as_str(),
                def.fragment,
                def.describe,
                def.template,
                permissions,
                hands,
                based_on
            ],
            row_to_agent,
        )?;
    raw.into_agent()
}

/// Copy the template `from` into an operator copy named `as_name` (default: `from` itself, which
/// then overrides the packaged template). `packaged` copies the packaged text even when a copy
/// already overrides it — how an operator goes back to it. A new version of the target is
/// appended; nothing is replaced.
///
/// A copy under another *packaged* template's name is refused: it would silently replace that
/// agent's prompt with an unrelated one.
///
/// # Errors
/// [`Error::Types`] for an unknown source, a malformed name, or a copy onto another packaged name;
/// a database error.
pub fn copy(
    conn: &Connection,
    _meta: &WriteMeta,
    from: &str,
    packaged: bool,
    as_name: Option<&str>,
) -> Result<Agent> {
    let source = resolve(
        conn,
        from,
        if packaged {
            Pick::Packaged
        } else {
            Pick::Effective
        },
    )?;
    let target = as_name.unwrap_or(from);
    validate_name(target)?;
    if target != from && packaged_named(target)?.is_some() {
        return Err(invalid(format!(
            "`{target}` is a packaged template, and a copy under its name overrides it: copy \
             `{target}` itself to change it"
        )));
    }
    let based_on = match source.source {
        Source::Packaged => format!("packaged:{}@{}", source.name, source.version),
        Source::Operator => format!("{}@{}", source.name, source.version),
    };
    append(conn, target, &source.def, Some(&based_on))
}

/// What [`set`] changes; `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    /// The prompt.
    pub template: Option<String>,
    /// The role.
    pub role: Option<Role>,
    /// The description.
    pub describe: Option<String>,
    /// The permissions.
    pub permissions: Option<AgentPermissions>,
    /// The hand-offs.
    pub hands_off_to: Option<Vec<String>>,
}

/// Edit the operator copy `name`, appending a version — or nothing, when the edit changes nothing
/// (`false` beside the copy as it stands). A packaged template is never edited: copy it first.
///
/// # Errors
/// [`Error::Types`] for a name with no operator copy, an empty edit, or a definition that does not
/// validate; a database error.
pub fn set(conn: &Connection, _meta: &WriteMeta, name: &str, edit: Edit) -> Result<(Agent, bool)> {
    if edit == Edit::default() {
        return Err(invalid("nothing to change: name a field to set"));
    }
    let Some(current) = newest_copy(conn, name)? else {
        return Err(if packaged_named(name)?.is_some() {
            invalid(format!(
                "`{name}` is a packaged template and read-only: `jkb workflow agent copy {name}` \
                 makes the operator copy you edit"
            ))
        } else {
            unknown(name)
        });
    };
    let mut def = current.def.clone();
    if let Some(t) = edit.template {
        def.template = t;
    }
    if let Some(r) = edit.role {
        def.role = r;
    }
    if let Some(d) = edit.describe {
        def.describe = d;
    }
    if let Some(p) = edit.permissions {
        def.permissions = p;
    }
    if let Some(h) = edit.hands_off_to {
        def.hands_off_to = h;
    }
    if def == current.def {
        return Ok((current, false));
    }
    Ok((append(conn, name, &def, None)?, true))
}

#[cfg(test)]
mod tests;
