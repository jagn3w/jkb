//! The `tasks` serializer: one `tasks.md` ⇄ many task items (design D24).
//!
//! A tasks file mixes three kinds of line:
//! - **section headers** (`## Backend`) → a [`SyncSection`] that the engine maps to a
//!   namespace, so a section is queryable structure;
//! - **task lines** (`- [ ] title !p1 @2026-07-15 #size=small +repos/app needs:^dep ^id`)
//!   → a `task` [`SyncItem`], with a checkbox status, quick-add-style modifiers, a
//!   `needs:^id` dependency, indentation-driven `parent_of` hierarchy, and a stable
//!   trailing `^id` (minted deterministically when absent);
//! - **everything else** (prose, the legend comment, blanks) → an inline `SyncBlock::Prose`
//!   in the document layout, preserved verbatim so the file round-trips. Prose is
//!   deliberately **never** an item: it has no identity that survives an edit.
//!
//! Identity is the visible `^id`, carried in each item's binding uri as
//! `file://<path>#<id>` by the engine. [`render`](TasksSerializer::render) normalizes:
//! it always writes the `^id` back (the "renumber" pass) and is **idempotent**
//! (`render(parse(render(doc))) == render(doc)`), which the engine relies on to store
//! rendered bytes as the three-way base without triggering a false "KB changed".

use std::collections::{HashMap, HashSet};

use jkb_core::blob;
use jkb_core::dsl::{slug, tokenize, unquote};
use jkb_types::{EdgeType, Error as TypeError};

use super::{SyncBlock, SyncDoc, SyncEdge, SyncItem, SyncSection, SyncSerializer};
use crate::{Error, Result};

/// The `tasks` serializer (one file ⇄ many task items).
pub struct TasksSerializer;

impl SyncSerializer for TasksSerializer {
    fn name(&self) -> &'static str {
        "tasks"
    }

    fn parse(&self, bytes: &[u8]) -> Result<SyncDoc> {
        let text = std::str::from_utf8(bytes).map_err(|_| {
            bad("file is not valid UTF-8; the `tasks` serializer handles text files")
        })?;
        parse_text(text)
    }

    fn render(&self, doc: &SyncDoc) -> Result<Vec<u8>> {
        Ok(render_doc(doc).into_bytes())
    }

    fn quarantine_on_parse_error(&self) -> bool {
        true
    }
}

/// Parse the whole file into a [`SyncDoc`]. A single trailing newline is normalized
/// away (and re-added on render) so the round-trip is stable.
fn parse_text(text: &str) -> Result<SyncDoc> {
    // A truly empty file yields an empty doc; a lone "\n" is a preserved blank line.
    if text.is_empty() {
        return Ok(SyncDoc::default());
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut st = ParseState::default();
    for line in body.split('\n') {
        if let Some((level, header_text)) = header(line) {
            st.on_header(line, level, header_text);
        } else if let Some((indent, status, rest)) = task_line(line) {
            st.on_task(indent, status, rest)?;
        } else if st.take_continuation(line) {
            // An indented, non-blank line under an open task is that task's own body.
        } else {
            st.open_task = None;
            st.text_run.push(line.to_owned());
        }
    }
    st.flush_text();
    if let Some(missing) = dangling_dependency(&st.doc) {
        return Err(bad(&format!(
            "task dependency `needs:^{missing}` names an id that is not defined in this file"
        )));
    }
    if has_dependency_cycle(&st.doc) {
        return Err(bad("task dependencies (`needs:^…`) form a cycle"));
    }
    Ok(st.doc)
}

/// Mutable state accumulated while parsing a tasks file line by line.
#[derive(Default)]
struct ParseState {
    doc: SyncDoc,
    /// Every minted/claimed `local_id` (tasks and text), so none collide.
    used_ids: HashSet<String>,
    /// Every section slug path, so none collide.
    used_sections: HashSet<String>,
    /// `(header level, slug path)` of the open section ancestry.
    sec_stack: Vec<(usize, String)>,
    /// The current section's slug path (`None` before the first header).
    current_section: Option<String>,
    /// `(indent, local_id)` of the open task ancestry, for `parent_of`.
    task_stack: Vec<(usize, String)>,
    /// Accumulated prose/blank lines awaiting a boundary.
    text_run: Vec<String>,
    /// `(index into doc.items, own indent)` of the task whose body is still open, so an
    /// indented line beneath it is appended to *it* rather than floating in the section.
    open_task: Option<(usize, usize)>,
}

impl ParseState {
    /// If `line` continues the open task's body, append it to that task's content and
    /// report `true`.
    ///
    /// A continuation is a **non-blank line indented deeper than its task** that is not
    /// itself a task line — the shape every multi-line entry in a tasks file already uses
    /// (a finding's failure scenario, a decision's rationale). It belongs to the *item*, not
    /// to the section: stored on the section it drifts away from the task it explains and can
    /// be stranded when the task moves (see `memory/sync-export-wins`).
    ///
    /// A blank line closes the body, so a second paragraph becomes ordinary section prose —
    /// predictable, and it still round-trips byte-for-byte.
    fn take_continuation(&mut self, line: &str) -> bool {
        let Some((index, task_indent)) = self.open_task else {
            return false;
        };
        let indent = line.len() - line.trim_start().len();
        if line.trim().is_empty() || indent <= task_indent {
            self.open_task = None;
            return false;
        }
        // Any pending prose belongs before this task, which was already emitted.
        let item = &mut self.doc.items[index];
        item.content.push('\n');
        item.content.push_str(line.trim_start());
        true
    }

    /// Emit any accumulated prose/blank lines as one verbatim [`SyncProse`] block.
    ///
    /// Prose is **not** an item: it has no identity to give it. Minting one from a content
    /// hash plus an occurrence counter produced ids that broke on the next edit, and the
    /// orphans kept their sections alive forever (see `memory/sync-export-wins`).
    fn flush_text(&mut self) {
        if self.text_run.is_empty() {
            return;
        }
        let content = self.text_run.join("\n");
        self.text_run.clear();
        self.doc.layout.push(SyncBlock::Prose(content));
    }

    /// Handle a `##` header: open a namespace-mapped section, resetting task nesting.
    fn on_header(&mut self, line: &str, level: usize, header_text: &str) {
        self.flush_text();
        self.open_task = None;
        self.task_stack.clear();
        while self.sec_stack.last().is_some_and(|(lvl, _)| *lvl >= level) {
            self.sec_stack.pop();
        }
        let parent = self.sec_stack.last().map(|(_, p)| p.clone());
        let base = match &parent {
            Some(p) => format!("{p}/{}", section_slug(header_text)),
            None => section_slug(header_text),
        };
        let path = uniquify(base, &mut self.used_sections);
        self.sec_stack.push((level, path.clone()));
        self.current_section = Some(path.clone());
        self.doc.layout.push(SyncBlock::Section(path.clone()));
        self.doc.sections.push(SyncSection {
            path,
            header_line: line.to_owned(),
        });
    }

    /// Handle a `- [ ]` task line: create the item, its hierarchy, and its edges.
    fn on_task(&mut self, indent: usize, status: &str, rest: &str) -> Result<()> {
        self.flush_text();
        let parsed = parse_task_tokens(rest);
        let local_id = match parsed.own_id {
            // `classify` already guaranteed uri-safety; only a duplicate id (two lines
            // claiming the same identity) is a genuine error.
            Some(id) => {
                if !self.used_ids.insert(id.clone()) {
                    return Err(bad(&format!("duplicate task id `^{id}`")));
                }
                id
            }
            None => mint_id(&parsed.title, &mut self.used_ids),
        };
        while self
            .task_stack
            .last()
            .is_some_and(|(ind, _)| *ind >= indent)
        {
            self.task_stack.pop();
        }
        let parent = self.task_stack.last().map(|(_, id)| id.clone());
        self.task_stack.push((indent, local_id.clone()));

        let mut item = SyncItem::new(local_id.clone(), "task", parsed.title);
        item.section.clone_from(&self.current_section);
        item.status = Some(status.to_owned());
        item.priority = parsed.priority;
        item.due = parsed.due;
        item.tags = parsed.tags;
        item.mirrors = parsed.mirrors;
        item.parent.clone_from(&parent);
        // `position` is the KB-side ordering hint (`placements.position`), derived from the
        // block index; document order itself lives in `layout`.
        item.position = i64::try_from(self.doc.layout.len()).unwrap_or(i64::MAX);
        self.doc.layout.push(SyncBlock::Item(local_id.clone()));
        self.open_task = Some((self.doc.items.len(), indent));
        self.doc.items.push(item);

        if let Some(p) = parent {
            self.doc.edges.push(SyncEdge {
                src: p,
                dst: local_id.clone(),
                edge_type: EdgeType::ParentOf,
            });
        }
        for dep in parsed.deps {
            self.doc.edges.push(SyncEdge {
                src: local_id.clone(),
                dst: dep,
                edge_type: EdgeType::DependsOn,
            });
        }
        Ok(())
    }
}

/// The `dst` of the first `depends_on` edge whose target is not defined in the file, if
/// any. A `needs:^id` whose `^id` names no task (a typo, or the target line was deleted)
/// would otherwise be silently dropped by the engine's `reconcile_edges` (which skips any
/// edge endpoint that doesn't resolve to an item id), losing the user-declared dependency
/// permanently and invisibly. Surfacing it as a parse error routes the file to quarantine
/// so the mistake is flagged instead of swallowed.
fn dangling_dependency(doc: &SyncDoc) -> Option<String> {
    let defined: HashSet<&str> = doc.items.iter().map(|i| i.local_id.as_str()).collect();
    doc.edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::DependsOn)
        .find(|e| !defined.contains(e.dst.as_str()))
        .map(|e| e.dst.clone())
}

/// Whether the `depends_on` edges among the file's tasks contain a cycle. Detecting it
/// at parse time keeps `edge::link`'s cycle guard from failing mid-apply (which would
/// otherwise abort the whole sync run); a cycle instead routes the file to quarantine.
fn has_dependency_cycle(doc: &SyncDoc) -> bool {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in doc
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::DependsOn)
    {
        adj.entry(&e.src).or_default().push(&e.dst);
    }
    // DFS with a colour map: 0 = unvisited, 1 = on the current path, 2 = fully explored.
    let mut colour: HashMap<&str, u8> = HashMap::new();
    adj.keys().any(|start| {
        colour.get(start).copied().unwrap_or(0) == 0 && visit(start, &adj, &mut colour)
    })
}

/// Recursive cycle-detection visit: returns `true` if a back-edge (cycle) is found.
fn visit<'a>(
    node: &'a str,
    adj: &HashMap<&'a str, Vec<&'a str>>,
    colour: &mut HashMap<&'a str, u8>,
) -> bool {
    colour.insert(node, 1);
    for &next in adj.get(node).map_or(&[][..], Vec::as_slice) {
        match colour.get(next).copied().unwrap_or(0) {
            1 => return true,
            0 if visit(next, adj, colour) => return true,
            _ => {}
        }
    }
    colour.insert(node, 2);
    false
}

/// Render a [`SyncDoc`] back to file bytes: merge sections and items by `position` and
/// emit each as a line (or verbatim block for `text`), with a single trailing newline.
fn render_doc(doc: &SyncDoc) -> String {
    // parent_of (child -> parent) and depends_on (src -> [dst]) from the edges.
    let mut parent_of: HashMap<&str, &str> = HashMap::new();
    let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &doc.edges {
        match e.edge_type {
            EdgeType::ParentOf => {
                parent_of.insert(&e.dst, &e.src);
            }
            EdgeType::DependsOn => deps.entry(&e.src).or_default().push(&e.dst),
            _ => {}
        }
    }

    // Walk the layout — the document's own block order. Nothing is sorted and no ordinal is
    // consulted, so there is no second sequence for this one to drift against.
    let sections: HashMap<&str, &SyncSection> =
        doc.sections.iter().map(|s| (s.path.as_str(), s)).collect();
    let items: HashMap<&str, &SyncItem> =
        doc.items.iter().map(|i| (i.local_id.as_str(), i)).collect();

    let mut lines: Vec<String> = Vec::new();
    for block in &doc.layout {
        match block {
            SyncBlock::Prose(text) => lines.push(text.clone()),
            SyncBlock::Section(path) => {
                // A layout entry naming a section that is gone is simply skipped: the file
                // must never grow a header for a section the document no longer has.
                if let Some(s) = sections.get(path.as_str()) {
                    lines.push(s.header_line.clone());
                }
            }
            SyncBlock::Item(id) => {
                if let Some(i) = items.get(id.as_str()) {
                    if i.kind == "task" {
                        lines.push(render_task(i, &parent_of, &deps));
                    } else {
                        // A non-task item (legacy `text`, from before prose left the item
                        // model) renders verbatim so an un-migrated KB round-trips.
                        lines.push(i.content.clone());
                    }
                }
            }
        }
    }

    // Anything the layout does not mention is appended, so a KB-created item or section can
    // never be silently dropped from its file.
    let listed: HashSet<&str> = doc
        .layout
        .iter()
        .filter_map(|b| match b {
            SyncBlock::Item(id) => Some(id.as_str()),
            _ => None,
        })
        .collect();
    for i in &doc.items {
        if !listed.contains(i.local_id.as_str()) {
            lines.push(if i.kind == "task" {
                render_task(i, &parent_of, &deps)
            } else {
                i.content.clone()
            });
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Render one task: its line (indentation from `parent_of` depth, a checkbox from its
/// status, the modifiers in canonical order, the trailing `^id`), followed by its **body** —
/// every content line after the first, re-indented one level under the task line.
///
/// A task's content is `title\nbody…`: the first line is the title that carries the
/// modifiers, the rest is prose belonging to this task. Re-indenting canonically (rather
/// than preserving whatever the author typed) keeps `render` idempotent, which the engine
/// relies on to use rendered bytes as the three-way base.
fn render_task(
    item: &SyncItem,
    parent_of: &HashMap<&str, &str>,
    deps: &HashMap<&str, Vec<&str>>,
) -> String {
    let indent = "  ".repeat(depth(&item.local_id, parent_of));
    let checkbox = match item.status.as_deref() {
        Some("done") => 'x',
        Some("in_progress") => '~',
        Some("needs_review") => '?',
        Some("cancelled") => '-',
        _ => ' ',
    };
    let (title, body) = match item.content.split_once('\n') {
        Some((title, body)) => (title, Some(body)),
        None => (item.content.as_str(), None),
    };
    let mut parts: Vec<String> = vec![title.to_owned()];
    if let Some(p) = item.priority {
        parts.push(format!("!p{p}"));
    }
    if let Some(d) = &item.due {
        parts.push(format!("@{d}"));
    }
    // Every facet round-trips. There used to be a filter here, dropping the reserved `base=`
    // facet, and it had to be paired with a matching exclusion on the KB side — an asymmetry that
    // was itself a must-fix, because the two universes have to be the same one or a task holding
    // the facet reads as permanently KB-edited and its next disk edit comes back a conflict. The
    // fact that needed protecting is a `branch_records` row now, so both sides draw from one
    // universe and neither has to remember a rule.
    for (facet, value) in &item.tags {
        parts.push(format!("#{facet}={value}"));
    }
    for m in &item.mirrors {
        parts.push(format!("+{m}"));
    }
    if let Some(ds) = deps.get(item.local_id.as_str()) {
        for dep in ds {
            parts.push(format!("needs:^{dep}"));
        }
    }
    parts.push(format!("^{}", item.local_id));
    let mut out = format!("{indent}- [{checkbox}] {}", parts.join(" "));
    if let Some(body) = body {
        for line in body.split('\n') {
            out.push('\n');
            out.push_str(&indent);
            out.push_str("  ");
            out.push_str(line);
        }
    }
    out
}

/// The nesting depth of `id` via `parent_of` (guarded against cycles).
fn depth(id: &str, parent_of: &HashMap<&str, &str>) -> usize {
    let mut d = 0;
    let mut cur = id;
    let mut seen: HashSet<&str> = HashSet::new();
    while let Some(&parent) = parent_of.get(cur) {
        if !seen.insert(cur) {
            break;
        }
        d += 1;
        cur = parent;
    }
    d
}

/// A parsed task line's modifiers.
struct TaskTokens {
    title: String,
    priority: Option<i64>,
    due: Option<String>,
    tags: Vec<(String, String)>,
    mirrors: Vec<String>,
    deps: Vec<String>,
    own_id: Option<String>,
}

/// A well-formed trailing modifier on a task line.
enum Modifier {
    Priority(i64),
    Due(String),
    Tag(String, String),
    Mirror(String),
    Dep(String),
    Own(String),
}

/// Classify one token as a [`Modifier`], or `None` if it is ordinary title text. Only
/// *fully* well-formed tokens count — a bare `+`, a `#tag` without `=`, a non-integer
/// `!p`, or a non-uri-safe `^id` are all just words. This keeps prose that happens to
/// contain `+`/`#`/`@`/`^` (common in real task descriptions) from being misparsed.
fn classify(token: &str) -> Option<Modifier> {
    if let Some(v) = token.strip_prefix("!p") {
        return v.parse::<i64>().ok().map(Modifier::Priority);
    }
    if let Some(v) = token.strip_prefix("needs:^") {
        return (!v.is_empty()).then(|| Modifier::Dep(v.to_owned()));
    }
    if let Some(v) = token.strip_prefix('@') {
        return (!v.is_empty()).then(|| Modifier::Due(v.to_owned()));
    }
    if let Some(v) = token.strip_prefix('#') {
        return v.split_once('=').and_then(|(f, val)| {
            (!f.is_empty() && !val.is_empty()).then(|| Modifier::Tag(f.to_owned(), val.to_owned()))
        });
    }
    if let Some(v) = token.strip_prefix('+') {
        return (!v.is_empty()).then(|| Modifier::Mirror(v.to_owned()));
    }
    if let Some(v) = token.strip_prefix('^') {
        return is_uri_safe(v).then(|| Modifier::Own(v.to_owned()));
    }
    None
}

/// Parse the text after a task checkbox: the maximal run of well-formed [`Modifier`]s
/// at the **end** of the line is metadata; everything before it is the (verbatim)
/// title. Never fails — malformed or mid-line sigils are treated as ordinary words,
/// which is also consistent with [`render_task`] always emitting modifiers trailing.
fn parse_task_tokens(rest: &str) -> TaskTokens {
    // Peel the stable trailing `^id` anchor off the *raw* line first, before the
    // quote-aware `tokenize`. An anchor is a whitespace-delimited `^`-word containing
    // neither spaces nor quotes, so it is always recoverable this way — whereas
    // `tokenize` would let an unterminated `"` earlier in the title (common in real
    // prose) swallow the anchor into one long quoted token, hiding it from `classify`.
    // Missing the anchor makes the parser think the line has none, mint a fresh id, and
    // `render` append a *second* one — double-stamping the line on every sync.
    let (rest, anchor) = split_trailing_anchor(rest);
    let tokens = tokenize(rest);
    let mut start = tokens.len();
    while start > 0 && classify(&tokens[start - 1]).is_some() {
        start -= 1;
    }
    let title = tokens[..start]
        .iter()
        .map(|t| unquote(t))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = TaskTokens {
        title,
        priority: None,
        due: None,
        tags: Vec::new(),
        mirrors: Vec::new(),
        deps: Vec::new(),
        own_id: None,
    };
    for token in &tokens[start..] {
        match classify(token) {
            Some(Modifier::Priority(p)) => out.priority = Some(p),
            Some(Modifier::Due(d)) => out.due = Some(d),
            Some(Modifier::Tag(f, v)) => out.tags.push((f, v)),
            Some(Modifier::Mirror(m)) => out.mirrors.push(m),
            Some(Modifier::Dep(d)) => out.deps.push(d),
            Some(Modifier::Own(id)) => out.own_id = Some(id),
            None => {}
        }
    }
    // A raw trailing anchor is authoritative over anything `classify` recovered.
    if anchor.is_some() {
        out.own_id = anchor;
    }
    out
}

/// Peel every trailing `^<uri-safe>` anchor off the raw line, returning the remaining
/// text and the **left-most** anchor found — the task's stable identity.
///
/// This runs before the quote-aware [`tokenize`] so an unterminated `"` in the title
/// cannot hide the anchor (see [`parse_task_tokens`]). When a line already carries several
/// trailing anchors — the residue of an earlier double-stamp — they collapse to the
/// left-most: minting only ever *appends*, so the original hand-authored id is left-most and
/// the extra minted duplicates are dropped, healing the line back to a single anchor.
fn split_trailing_anchor(rest: &str) -> (&str, Option<String>) {
    let mut head = rest.trim_end();
    let mut anchor: Option<String> = None;
    loop {
        let (before, last) = match head.rsplit_once(char::is_whitespace) {
            Some((b, l)) => (b, l),
            None => ("", head),
        };
        match last.strip_prefix('^').filter(|id| is_uri_safe(id)) {
            Some(id) => {
                anchor = Some(id.to_owned());
                head = before.trim_end();
            }
            None => break,
        }
    }
    match anchor {
        Some(id) => (head, Some(id)),
        None => (rest, None),
    }
}

/// Whether `s` is a uri-safe local id: non-empty lowercase letters, digits, and dashes.
fn is_uri_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Parse a header line `#{1,6} text`, returning `(level, text)`. Only lines starting
/// with `#` at column 0 followed by a space count (so `#facet=x` inside a task or a
/// `#comment` mid-line is never mistaken for a header).
fn header(line: &str) -> Option<(usize, &str)> {
    if !line.starts_with('#') {
        return None;
    }
    let level = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    let text = rest.strip_prefix(' ')?.trim();
    if text.is_empty() {
        None
    } else {
        Some((level, text))
    }
}

/// Tab stop width used to expand a leading `\t` into a visual column count when
/// measuring task indentation (see `task_line`).
const TAB_WIDTH: usize = 4;

/// Visual indentation width of a run of leading whitespace: a space is one
/// column, a tab advances to the next `TAB_WIDTH` stop. Used only for *relative*
/// nesting comparison, so the exact width is immaterial as long as deeper
/// (more-indented) lines yield strictly larger widths.
fn indent_width(ws: &str) -> usize {
    let mut col = 0;
    for c in ws.chars() {
        if c == '\t' {
            col = (col / TAB_WIDTH + 1) * TAB_WIDTH;
        } else {
            col += 1;
        }
    }
    col
}

/// Parse a task line, returning `(indent_width, status, rest_after_checkbox)`.
///
/// `indent_width` is a visual column count (see `indent_width`) used only for the
/// *relative* nesting comparison in `on_task`; tabs count as indentation so a
/// Tab-nested subtask (`"\t- [ ] child"`) still parents under a preceding
/// less-indented task instead of collapsing to depth 0.
fn task_line(line: &str) -> Option<(usize, &'static str, &str)> {
    let ws_bytes = line.len() - line.trim_start_matches([' ', '\t']).len();
    let indent = indent_width(&line[..ws_bytes]);
    let trimmed = &line[ws_bytes..];
    let after = trimmed.strip_prefix("- [")?;
    let mark = after.chars().next()?;
    let status = match mark {
        ' ' => "open",
        'x' | 'X' => "done",
        '~' => "in_progress",
        '?' => "needs_review",
        '-' => "cancelled",
        _ => return None,
    };
    // After the marker we require a literal `]`, then either end-of-line (a titleless
    // placeholder `- [ ]`) or a single space/tab separator before the title. Accepting
    // both an empty remainder and a tab keeps a hand-edited `- [ ]` / `- [ ]\ttitle`
    // parsed as a task instead of silently falling through to verbatim prose (lost from
    // the DAG). `- [x]foo` (no separator) is still not a task line.
    let body = after.get(1..)?.strip_prefix(']')?;
    let rest = match body.chars().next() {
        None => "",
        Some(' ' | '\t') => &body[1..],
        Some(_) => return None,
    };
    Some((indent, status, rest))
}

/// A namespace-path segment from a `##` header: the shared [`slug`] with a `"section"`
/// fallback when the header has no alphanumeric characters.
fn section_slug(text: &str) -> String {
    let s = slug(text);
    if s.is_empty() {
        "section".to_owned()
    } else {
        s
    }
}

/// Mint a deterministic uri-safe id for a caret-less task, disambiguating collisions
/// within the file with a numeric suffix. Pure — no RNG or clock. The base is the shared
/// [`slug`] (so a task synced from a file and one added via the CLI derive the same slug
/// from the same title), capped at 24 characters with a `"task"` fallback.
fn mint_id(title: &str, used: &mut HashSet<String>) -> String {
    let base: String = slug(title).chars().take(24).collect();
    let trimmed = base.trim_matches('-');
    let base = if trimmed.is_empty() {
        "task".to_owned()
    } else {
        trimmed.to_owned()
    };
    let short = &blob::hash_bytes(title.as_bytes())[..6];
    let candidate = format!("{base}-{short}");
    let mut id = candidate.clone();
    let mut n = 2;
    while used.contains(&id) {
        id = format!("{candidate}-{n}");
        n += 1;
    }
    used.insert(id.clone());
    id
}

/// Ensure a section path is unique within the file, suffixing `-2`, `-3`, … on clash.
fn uniquify(base: String, used: &mut HashSet<String>) -> String {
    if used.insert(base.clone()) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

/// A validation error for the `tasks` format (routes to quarantine in the engine).
fn bad(msg: &str) -> Error {
    Error::Types(TypeError::Validation(format!("tasks file: {msg}")))
}

#[cfg(test)]
mod tests {
    use super::{SyncBlock, SyncDoc, SyncSerializer, TasksSerializer};

    /// The document's prose blocks, in order — prose lives inline in the layout.
    fn prose_blocks(doc: &SyncDoc) -> Vec<&str> {
        doc.layout
            .iter()
            .filter_map(|b| match b {
                SyncBlock::Prose(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    const SAMPLE: &str = "\
<!-- Legend: [x] done -->

## 1. Backend & API
Some description.
- [ ] Fix the flaky test !p1 @2026-07-15 #size=small +repos/app needs:^setup ^fix-flaky
  - [x] Write the harness ^write-harness
## 2. Frontend
- [~] Ship the button ^ship
- [ ] Set up CI ^setup
";

    #[test]
    fn parse_maps_sections_tasks_and_prose() {
        let doc = TasksSerializer.parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(doc.sections.len(), 2);
        assert_eq!(doc.sections[0].path, "1-backend-api");
        assert_eq!(doc.sections[0].header_line, "## 1. Backend & API");
        assert_eq!(doc.sections[1].path, "2-frontend");

        let tasks: Vec<_> = doc.items.iter().filter(|i| i.kind == "task").collect();
        assert_eq!(tasks.len(), 4);
        let fix = &tasks[0];
        assert_eq!(fix.content, "Fix the flaky test");
        assert_eq!(fix.local_id, "fix-flaky");
        assert_eq!(fix.status.as_deref(), Some("open"));
        assert_eq!(fix.priority, Some(1));
        assert_eq!(fix.due.as_deref(), Some("2026-07-15"));
        assert_eq!(fix.tags, vec![("size".to_owned(), "small".to_owned())]);
        assert_eq!(fix.mirrors, vec!["repos/app".to_owned()]);
        assert_eq!(fix.section.as_deref(), Some("1-backend-api"));

        // Hierarchy: write-harness is a child of fix-flaky.
        let child = tasks
            .iter()
            .find(|t| t.local_id == "write-harness")
            .unwrap();
        assert_eq!(child.parent.as_deref(), Some("fix-flaky"));
        assert_eq!(child.status.as_deref(), Some("done"));

        // Dependency edge fix-flaky -> setup.
        assert!(doc.edges.iter().any(|e| e.src == "fix-flaky"
            && e.dst == "setup"
            && e.edge_type == jkb_types::EdgeType::DependsOn));
        // Parent_of edge fix-flaky -> write-harness.
        assert!(doc.edges.iter().any(|e| e.src == "fix-flaky"
            && e.dst == "write-harness"
            && e.edge_type == jkb_types::EdgeType::ParentOf));

        // Prose is preserved verbatim as `prose` blocks — NOT as items. Giving it item
        // identity was the bug behind `memory/sync-export-wins`.
        assert!(prose_blocks(&doc).iter().any(|p| p.contains("Legend")));
        assert!(prose_blocks(&doc).contains(&"Some description."));
        assert!(
            !doc.items.iter().any(|i| i.kind == "text"),
            "prose must not materialize as items"
        );
    }

    /// An item's indented continuation lines are its **body**, carried on the item — so they
    /// travel with it instead of being stranded in the section it used to live in.
    #[test]
    fn indented_continuation_lines_are_the_task_body_not_section_prose() {
        let s = TasksSerializer;
        let src = "## Alpha\n\n- [ ] first ^first\n  why this matters\n  and a second line\n- [ ] second ^second\n";
        let doc = s.parse(src.as_bytes()).unwrap();

        let first = doc.items.iter().find(|i| i.local_id == "first").unwrap();
        assert_eq!(
            first.content, "first\nwhy this matters\nand a second line",
            "the continuation lines belong to the task, not the section"
        );
        assert!(
            !prose_blocks(&doc)
                .iter()
                .any(|p| p.contains("why this matters")),
            "a task's own body must not also be loose prose"
        );
        // The next task is unaffected by the one above it.
        let second = doc.items.iter().find(|i| i.local_id == "second").unwrap();
        assert_eq!(second.content, "second");

        // Byte-exact round trip.
        assert_eq!(String::from_utf8(s.render(&doc).unwrap()).unwrap(), src);

        // A BLANK line closes the body: what follows is ordinary section prose. This is the
        // rule `jkb task edit` enforces, so the two cannot disagree.
        let split = s.parse(b"- [ ] t ^t\n  body\n\n  detached\n").unwrap();
        assert_eq!(split.items[0].content, "t\nbody");
        assert!(prose_blocks(&split).iter().any(|p| p.contains("detached")));

        // A deeper `- [ ]` line is still a SUBTASK, never body text.
        let nested = s.parse(b"- [ ] parent ^p\n  - [ ] child ^c\n").unwrap();
        assert_eq!(nested.items.len(), 2);
        assert_eq!(nested.items[0].content, "parent");
        assert_eq!(nested.items[1].parent.as_deref(), Some("p"));
    }

    #[test]
    fn render_is_idempotent_and_preserves_content() {
        let s = TasksSerializer;
        let doc = s.parse(SAMPLE.as_bytes()).unwrap();
        let rendered = s.render(&doc).unwrap();
        // render(parse(render(doc))) == render(doc)
        let doc2 = s.parse(&rendered).unwrap();
        let rendered2 = s.render(&doc2).unwrap();
        assert_eq!(rendered, rendered2);

        let text = String::from_utf8(rendered).unwrap();
        assert!(text.contains("## 1. Backend & API"));
        assert!(text.contains("<!-- Legend: [x] done -->"));
        assert!(text.contains("- [ ] Fix the flaky test !p1 @2026-07-15 #size=small +repos/app needs:^setup ^fix-flaky"));
        assert!(text.contains("  - [x] Write the harness ^write-harness"));
        assert!(text.contains("- [~] Ship the button ^ship"));
    }

    #[test]
    fn caret_less_task_gets_a_minted_id() {
        let doc = TasksSerializer.parse(b"- [ ] no id here\n").unwrap();
        let task = &doc.items[0];
        assert!(task.local_id.starts_with("no-id-here-"));
        assert_eq!(task.status.as_deref(), Some("open"));
    }

    #[test]
    fn minted_id_slug_matches_the_shared_slugger() {
        // The minted id's slug prefix is the shared `jkb_core::dsl::slug`, so a task
        // synced from a file and one added via the CLI derive the same slug from the
        // same title — including unicode, which the old ASCII-only slug dropped.
        let doc = TasksSerializer
            .parse("- [ ] Café résumé cleanup\n".as_bytes())
            .unwrap();
        assert!(
            doc.items[0].local_id.starts_with("café-résumé-cleanup-"),
            "{}",
            doc.items[0].local_id
        );
    }

    #[test]
    fn needs_review_checkbox_round_trips() {
        let s = TasksSerializer;
        let doc = s.parse(b"- [?] awaiting approval ^rev\n").unwrap();
        assert_eq!(doc.items[0].status.as_deref(), Some("needs_review"));
        let text = String::from_utf8(s.render(&doc).unwrap()).unwrap();
        assert!(text.contains("- [?] awaiting approval ^rev"));
    }

    #[test]
    fn parsing_is_lenient_but_real_corruption_errors() {
        // Prose sigils are title text, not errors — real task descriptions contain
        // `+`, `#`, `@` (this is what a raw openspec tasks.md looks like).
        let doc = TasksSerializer
            .parse(b"- [ ] run fmt + clippy and fix #123 @ home\n")
            .unwrap();
        let t = &doc.items[0];
        assert_eq!(t.content, "run fmt + clippy and fix #123 @ home");
        assert!(t.priority.is_none() && t.tags.is_empty() && t.mirrors.is_empty());

        // A malformed *trailing* modifier is also just text, never a hard error.
        assert!(TasksSerializer.parse(b"- [ ] title !pnotanumber\n").is_ok());
        assert!(TasksSerializer.parse(b"- [ ] title ^Bad_Id\n").is_ok());

        // Genuine corruption still errors (→ the engine quarantines):
        // duplicate stable ids and dependency cycles.
        assert!(TasksSerializer
            .parse(b"- [ ] a ^dup\n- [ ] b ^dup\n")
            .is_err());
        assert!(TasksSerializer
            .parse(b"- [ ] a needs:^b ^a\n- [ ] b needs:^a ^b\n")
            .is_err());
    }

    #[test]
    fn dangling_needs_id_errors() {
        // `needs:^missing` names an id no task in the file defines. Rather than silently
        // drop the dependency (the engine's reconcile_edges would skip the unresolved
        // endpoint), parsing errors so the file is quarantined for the user to fix.
        let err = TasksSerializer
            .parse(b"- [ ] a needs:^ghost ^a\n")
            .unwrap_err();
        assert!(
            err.to_string().contains("ghost"),
            "error should name the missing id: {err}"
        );
        // A dependency on a real id still parses cleanly.
        assert!(TasksSerializer
            .parse(b"- [ ] a needs:^b ^a\n- [ ] b ^b\n")
            .is_ok());
    }

    #[test]
    fn prose_never_fails_to_parse() {
        let doc = TasksSerializer
            .parse(b"just some prose\nwith #hashes and !bangs that aren't tasks\n")
            .unwrap();
        assert!(doc.items.iter().all(|i| i.kind == "text"));
        assert!(TasksSerializer.quarantine_on_parse_error());
    }

    #[test]
    fn tab_indented_subtask_nests_under_parent() {
        // A subtask nested with a Tab (not spaces) must still parent under the
        // preceding less-indented task instead of collapsing to depth 0.
        let doc = TasksSerializer
            .parse(b"- [ ] parent ^p\n\t- [ ] child ^c\n")
            .unwrap();
        let child = doc.items.iter().find(|i| i.local_id == "c").unwrap();
        assert_eq!(child.parent.as_deref(), Some("p"));
        assert!(doc
            .edges
            .iter()
            .any(|e| e.src == "p" && e.dst == "c" && e.edge_type == jkb_types::EdgeType::ParentOf));
    }

    #[test]
    fn anchor_survives_an_unterminated_quote_in_the_title() {
        // A title containing an unterminated `"` (here a raw `\"` inside backticks — exactly
        // what a hand-written openspec tasks.md looks like) must NOT hide the trailing
        // anchor. If it did, the parser would mint a fresh id and render would append a
        // *second* anchor, double-stamping the line on every sync.
        let line = "- [x] honour `\\\"` (a literal quote) and ^tokenize-honour-escape\n";
        let s = TasksSerializer;
        let doc = s.parse(line.as_bytes()).unwrap();
        let task = doc.items.iter().find(|i| i.kind == "task").unwrap();
        assert_eq!(task.local_id, "tokenize-honour-escape");
        assert!(
            !task.local_id.contains("honour-escape-"),
            "identity must be the hand-authored anchor, not a freshly minted one"
        );

        // Render emits exactly one anchor, and re-parsing is a fixed point (no growth).
        let rendered = String::from_utf8(s.render(&doc).unwrap()).unwrap();
        assert_eq!(
            rendered.matches('^').count(),
            1,
            "exactly one anchor: {rendered}"
        );
        assert!(rendered.contains("^tokenize-honour-escape"));
        let rendered2 =
            String::from_utf8(s.render(&s.parse(rendered.as_bytes()).unwrap()).unwrap()).unwrap();
        assert_eq!(
            rendered, rendered2,
            "double-stamp would grow the line each sync"
        );
    }

    #[test]
    fn multiple_trailing_anchors_heal_to_the_original() {
        // A line already corrupted by an earlier double-stamp — the hand-authored anchor
        // followed by two minted duplicates — collapses back to the left-most (original)
        // one; minting only ever appends, so the original is left-most.
        let s = TasksSerializer;
        let doc = s
            .parse(b"- [ ] title ^orig-anchor ^minted-5c5e31 ^minted-efd39a\n")
            .unwrap();
        let task = doc.items.iter().find(|i| i.kind == "task").unwrap();
        assert_eq!(task.local_id, "orig-anchor");
        assert_eq!(task.content, "title");
        let rendered = String::from_utf8(s.render(&doc).unwrap()).unwrap();
        assert_eq!(rendered.matches('^').count(), 1);
        assert!(rendered.contains("- [ ] title ^orig-anchor"));
    }

    #[test]
    fn empty_and_tab_separated_checkboxes_parse_as_tasks() {
        // `- [ ]` (no trailing space) and `- [x]\ttitle` (tab after the bracket) must be
        // parsed as tasks, not demoted to verbatim prose (which would drop them from the
        // DAG). `- [x]foo` (no separator) stays prose.
        let doc = TasksSerializer
            .parse(b"- [ ]\n- [x]\tdone one ^d\n- [~]bad\n")
            .unwrap();
        let tasks: Vec<_> = doc.items.iter().filter(|i| i.kind == "task").collect();
        assert_eq!(tasks.len(), 2, "two checkbox lines are tasks");
        assert_eq!(tasks[0].status.as_deref(), Some("open"));
        assert!(tasks[0].content.is_empty());
        assert_eq!(tasks[1].status.as_deref(), Some("done"));
        assert_eq!(tasks[1].content, "done one");
        // `- [~]bad` (no separator) is not a task; it survives verbatim as prose.
        assert!(prose_blocks(&doc).iter().any(|p| p.contains("[~]bad")));
    }
}
