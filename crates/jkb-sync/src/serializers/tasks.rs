//! The `tasks` serializer: one `tasks.md` ⇄ many task items (design D24).
//!
//! A tasks file mixes three kinds of line:
//! - **section headers** (`## Backend`) → a [`SyncSection`] that the engine maps to a
//!   namespace, so a section is queryable structure;
//! - **task lines** (`- [ ] title !p1 @2026-07-15 #size=small +repos/app needs:^dep ^id`)
//!   → a `task` [`SyncItem`], with a checkbox status, quick-add-style modifiers, a
//!   `needs:^id` dependency, indentation-driven `parent_of` hierarchy, and a stable
//!   trailing `^id` (minted deterministically when absent);
//! - **everything else** (prose, the legend comment, blanks) → a `text` [`SyncItem`],
//!   preserved verbatim so the file round-trips.
//!
//! Identity is the visible `^id`, carried in each item's binding uri as
//! `file://<path>#<id>` by the engine. [`render`](TasksSerializer::render) normalizes:
//! it always writes the `^id` back (the "renumber" pass) and is **idempotent**
//! (`render(parse(render(doc))) == render(doc)`), which the engine relies on to store
//! rendered bytes as the three-way base without triggering a false "KB changed".

use std::collections::{HashMap, HashSet};

use jkb_types::{EdgeType, Error as TypeError};

use super::{SyncDoc, SyncEdge, SyncItem, SyncSection, SyncSerializer};
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
        } else {
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
    pos: i64,
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
}

impl ParseState {
    /// Emit any accumulated prose/blank lines as one verbatim `text` item.
    fn flush_text(&mut self) {
        if self.text_run.is_empty() {
            return;
        }
        let content = self.text_run.join("\n");
        self.text_run.clear();
        let local_id = uniquify(
            format!("text-{}", &hash_hex(content.as_bytes())[..8]),
            &mut self.used_ids,
        );
        let mut item = SyncItem::new(local_id, "text", content);
        item.section.clone_from(&self.current_section);
        item.position = self.pos;
        self.doc.items.push(item);
        self.pos += 1;
    }

    /// Handle a `##` header: open a namespace-mapped section, resetting task nesting.
    fn on_header(&mut self, line: &str, level: usize, header_text: &str) {
        self.flush_text();
        self.task_stack.clear();
        while self.sec_stack.last().is_some_and(|(lvl, _)| *lvl >= level) {
            self.sec_stack.pop();
        }
        let parent = self.sec_stack.last().map(|(_, p)| p.clone());
        let base = match &parent {
            Some(p) => format!("{p}/{}", slug(header_text)),
            None => slug(header_text),
        };
        let path = uniquify(base, &mut self.used_sections);
        self.sec_stack.push((level, path.clone()));
        self.current_section = Some(path.clone());
        self.doc.sections.push(SyncSection {
            path,
            header_line: line.to_owned(),
            position: self.pos,
        });
        self.pos += 1;
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
        item.position = self.pos;
        item.status = Some(status.to_owned());
        item.priority = parsed.priority;
        item.due = parsed.due;
        item.tags = parsed.tags;
        item.mirrors = parsed.mirrors;
        item.parent.clone_from(&parent);
        self.doc.items.push(item);
        self.pos += 1;

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

/// A section header or an item, interleaved by `position` when rendering.
enum Block<'a> {
    Section(&'a SyncSection),
    Item(&'a SyncItem),
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

    // Interleave sections and items in file order.
    let mut blocks: Vec<(i64, Block)> = Vec::new();
    for s in &doc.sections {
        blocks.push((s.position, Block::Section(s)));
    }
    for i in &doc.items {
        blocks.push((i.position, Block::Item(i)));
    }
    blocks.sort_by_key(|(p, _)| *p);

    let mut lines: Vec<String> = Vec::new();
    for (_, block) in blocks {
        match block {
            Block::Section(s) => lines.push(s.header_line.clone()),
            Block::Item(i) if i.kind == "task" => {
                lines.push(render_task(i, &parent_of, &deps));
            }
            Block::Item(i) => lines.push(i.content.clone()),
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Render one task line: indentation from its `parent_of` depth, a checkbox from its
/// status, then the modifiers in a canonical order, then the trailing `^id`.
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
    let mut parts: Vec<String> = vec![item.content.clone()];
    if let Some(p) = item.priority {
        parts.push(format!("!p{p}"));
    }
    if let Some(d) = &item.due {
        parts.push(format!("@{d}"));
    }
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
    format!("{indent}- [{checkbox}] {}", parts.join(" "))
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
    out
}

/// Whether `s` is a uri-safe local id: non-empty lowercase letters, digits, and dashes.
fn is_uri_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Split on whitespace, keeping `"…"`-quoted spans together (mirrors `jkb_core::task`).
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for c in input.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                current.push(c);
            }
            c if c.is_whitespace() && !in_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Strip a single pair of surrounding double quotes, if present.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
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

/// Parse a task line, returning `(indent_spaces, status, rest_after_checkbox)`.
fn task_line(line: &str) -> Option<(usize, &'static str, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let trimmed = &line[indent..];
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
    let rest = after.get(1..)?.strip_prefix("] ")?;
    Some((indent, status, rest))
}

/// A lowercase `[a-z0-9-]` slug of `text` (runs of other characters collapse to `-`).
fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "section".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Mint a deterministic uri-safe id for a caret-less task, disambiguating collisions
/// within the file with a numeric suffix. Pure — no RNG or clock.
fn mint_id(title: &str, used: &mut HashSet<String>) -> String {
    let mut base: String = slug(title).chars().take(24).collect();
    let trimmed = base.trim_matches('-');
    base = if trimmed.is_empty() {
        "task".to_owned()
    } else {
        trimmed.to_owned()
    };
    let short = &hash_hex(title.as_bytes())[..6];
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

/// blake3 of `bytes` as lowercase hex.
fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// A validation error for the `tasks` format (routes to quarantine in the engine).
fn bad(msg: &str) -> Error {
    Error::Types(TypeError::Validation(format!("tasks file: {msg}")))
}

#[cfg(test)]
mod tests {
    use super::{SyncSerializer, TasksSerializer};

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

        // Prose preserved as text items.
        assert!(doc
            .items
            .iter()
            .any(|i| i.kind == "text" && i.content.contains("Legend")));
        assert!(doc
            .items
            .iter()
            .any(|i| i.kind == "text" && i.content == "Some description."));
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
}
