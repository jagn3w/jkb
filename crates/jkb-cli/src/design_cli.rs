//! `jkb design …` (design D53.4–5) as a client of the `design.*` ops: the same answer on the host and
//! through `jkb serve`, and what the Code Factory app calls for the same work.
//!
//! The edit loop an agent runs is `jkb design cat <uid>` — the text with span markers and a version
//! token — then `jkb design edit <uid> --base <token> --find <quote> --replace <text>`, the quote
//! copied from what `cat` printed. The edit is resolved in the version the token names and merged
//! over anything written since.

use anyhow::{bail, Context as _, Result};
use jkb_api::designs::plans::{Plan, PlanTask};
use jkb_api::designs::{DesignDoc, EditAsk, PromptAsk, Span, Written};
use jkb_api::{Request, Response};

use crate::ops_cli::{unexpected, Ops};
use crate::{DesignCmd, DesignPlanCmd, DesignPromptCmd};

/// The text `value` names: itself, or stdin for `-`.
fn text_arg(value: String) -> Result<String> {
    if value != "-" {
        return Ok(value);
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).context("reading stdin")?;
    Ok(buf)
}

/// The repo a design lives under: as given, or the ambient one ([`Ops::ambient_repo`]).
pub(crate) fn repo_of(ops: &Ops<'_>, repo: Option<String>) -> Result<String> {
    if let Some(repo) = repo {
        return Ok(repo);
    }
    ops.ambient_repo()?
        .context("not inside a mounted repo — name one with --repo")
}

fn written(ops: &Ops<'_>, op: &str, request: Request) -> Result<Written> {
    match ops.call(request)? {
        Response::DesignWritten { written } => Ok(written),
        other => unexpected(op, &other),
    }
}

fn span_answer(ops: &Ops<'_>, op: &str, request: Request) -> Result<Span> {
    match ops.call(request)? {
        Response::DesignSpan { span } => Ok(*span),
        other => unexpected(op, &other),
    }
}

fn print_written(ops: &Ops<'_>, w: &Written) -> Result<()> {
    if ops.json {
        println!("{}", serde_json::to_string_pretty(w)?);
        return Ok(());
    }
    match w.seq {
        Some(seq) => println!("{}: update {seq}", w.uid),
        None => println!("{}: nothing changed", w.uid),
    }
    if let Some(span) = &w.span {
        println!("span:    {span}");
    }
    println!("version: {}", w.version);
    for span in &w.demoted {
        println!(
            "demoted: {span} — its approved words changed, so it is PROPOSED again until re-approved"
        );
    }
    Ok(())
}

fn print_span(ops: &Ops<'_>, s: &Span) -> Result<()> {
    if ops.json {
        println!("{}", serde_json::to_string_pretty(s)?);
        return Ok(());
    }
    let anchored = if s.anchored { "" } else { " (unanchored)" };
    println!(
        "{}  {}{anchored}  reviewer={}{}",
        s.uid,
        s.state,
        s.reviewer,
        s.approved_by
            .as_deref()
            .map(|b| format!("  approved_by={b}"))
            .unwrap_or_default()
    );
    for step in &s.steps {
        println!("  staged into {step}");
    }
    if s.demoted {
        for p in &s.pieces {
            // Every piece of a demoted span is PROPOSED; its provenance says which words changed.
            let how = if p.removed {
                "removed"
            } else if p.added {
                "added"
            } else {
                "unchanged"
            };
            println!("  {how:<11} {:?}", p.text);
        }
    } else if !s.text.is_empty() {
        println!("  {:?}", s.text);
    }
    Ok(())
}

/// `<start>..<end>`, UTF-16 offsets.
fn parse_range(range: &str) -> Result<(u32, u32)> {
    let (start, end) = range
        .split_once("..")
        .with_context(|| format!("--range {range:?}: expected <start>..<end>"))?;
    let num = |s: &str| {
        s.trim()
            .parse::<u32>()
            .with_context(|| format!("--range {range:?}: {s:?} is not an offset"))
    };
    Ok((num(start)?, num(end)?))
}

fn print_doc(ops: &Ops<'_>, d: &DesignDoc, plain: bool) -> Result<()> {
    if ops.json {
        println!("{}", serde_json::to_string_pretty(d)?);
    } else if plain {
        print!("{}", d.text);
    } else {
        println!("design:  {} — {}", d.uid, d.title);
        println!("version: {}", d.version);
        println!();
        println!("{}", d.marked);
    }
    Ok(())
}

fn plan_answer(ops: &Ops<'_>, op: &str, request: Request) -> Result<Plan> {
    match ops.call(request)? {
        Response::DesignPlan { plan } => Ok(*plan),
        other => unexpected(op, &other),
    }
}

fn print_task(t: &PlanTask, indent: &str) {
    let depth = "  ".repeat(usize::try_from(t.depth).unwrap_or(0));
    let claim = t
        .claimed_by
        .as_deref()
        .map(|c| format!("  claimed_by={c}"))
        .unwrap_or_default();
    println!(
        "{indent}{depth}[{}] {}  {}  strategy={}{claim}",
        t.status.as_deref().unwrap_or("?"),
        t.uid,
        t.title,
        t.strategy
    );
}

fn print_plan(p: &Plan) {
    let archived = if p.archived { "  (archived)" } else { "" };
    println!("{}  {}{archived}", p.uid, p.title);
    for (n, step) in p.steps.iter().enumerate() {
        println!("  {}. {}  {}", n + 1, step.text, step.uid);
        for s in &step.spans {
            println!("       stages {} {}", s.uid, s.state);
        }
        for t in &step.tasks {
            print_task(t, "       ");
        }
    }
}

/// `jkb design plan …` (D53.6).
fn plan_cmd(ops: &Ops<'_>, what: DesignPlanCmd) -> Result<()> {
    let plan = match what {
        DesignPlanCmd::Ls { uid, all } => {
            let list = match ops.call(Request::DesignPlans { uid, all })? {
                Response::DesignPlans { list } => list,
                other => return unexpected("design.plans", &other),
            };
            if ops.json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            if list.plans.is_empty() {
                println!("(no plans)");
            }
            for p in &list.plans {
                print_plan(p);
            }
            if list.hidden > 0 {
                println!(
                    "({} archived plan(s) not shown — every task done or cancelled; --all lists them)",
                    list.hidden
                );
            }
            if !list.tasks.is_empty() {
                println!("one-off tasks:");
                for t in &list.tasks {
                    print_task(t, "  ");
                }
            }
            return Ok(());
        }
        DesignPlanCmd::Create { uid, title, steps } => plan_answer(
            ops,
            "design.plan_create",
            Request::DesignPlanCreate {
                uid,
                title: title.join(" "),
                steps,
            },
        )?,
        DesignPlanCmd::Step { plan, text } => plan_answer(
            ops,
            "design.plan_step",
            Request::DesignPlanStep {
                plan,
                text: text.join(" "),
            },
        )?,
        DesignPlanCmd::Show { plan } => {
            plan_answer(ops, "design.plan", Request::DesignPlan { plan })?
        }
    };
    if ops.json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print_plan(&plan);
    }
    Ok(())
}

/// `s` as one shell word: itself when nothing in it is special, else single-quoted.
fn shell_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ':'));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Print a prompt's text, or the whole answer under `--json`.
fn print_prompt<T: serde::Serialize>(ops: &Ops<'_>, answer: &T, text: &str) -> Result<()> {
    if ops.json {
        println!("{}", serde_json::to_string_pretty(answer)?);
    } else {
        print!("{text}");
    }
    Ok(())
}

/// `jkb design prompt record` (D53.6): the launch's own record of the session it is about to
/// start, its cwd the directory it runs in unless named.
fn record_prompt(ops: &Ops<'_>, request: Request) -> Result<()> {
    match ops.call(request)? {
        Response::DesignPromptRecorded { prompt, .. } => {
            if ops.json {
                println!("{}", serde_json::to_string_pretty(&prompt)?);
            } else {
                println!("{}  {}  {}", prompt.uid, prompt.launch, prompt.cwd);
            }
            Ok(())
        }
        other => unexpected("design.prompt_record", &other),
    }
}

/// `jkb design prompt ls` (D53.6): a design's prompts, newest first, each with how to resume it.
fn list_prompts(ops: &Ops<'_>, uid: String) -> Result<()> {
    let (uid, prompts) = match ops.call(Request::DesignPrompts { uid })? {
        Response::DesignPrompts { uid, prompts } => (uid, prompts),
        other => return unexpected("design.prompts", &other),
    };
    if ops.json {
        let listing = serde_json::json!({ "uid": uid, "prompts": prompts });
        println!("{}", serde_json::to_string_pretty(&listing)?);
        return Ok(());
    }
    if prompts.is_empty() {
        println!("(no prompts)");
    }
    for p in &prompts {
        let subject = p
            .subject
            .as_deref()
            .map(|s| format!("  on {s}"))
            .unwrap_or_default();
        println!(
            "{}  [{}]  {}{subject}  {}",
            p.created_at, p.launch, p.title, p.uid
        );
        println!(
            "    resume: cd {} && claude --resume {}",
            shell_word(&p.cwd),
            p.session
        );
    }
    Ok(())
}

/// `jkb design prompt of` (D53.9): the prompt a session was recorded with, which names its design.
fn prompt_of(ops: &Ops<'_>, session: String) -> Result<()> {
    let (session, prompt) = match ops.call(Request::DesignPromptOf { session })? {
        Response::DesignPromptOf { session, prompt } => (session, prompt),
        other => return unexpected("design.prompt_of", &other),
    };
    if ops.json {
        let answer = serde_json::json!({ "session": session, "prompt": prompt });
        println!("{}", serde_json::to_string_pretty(&answer)?);
        return Ok(());
    }
    match prompt {
        None => println!("(none)"),
        Some(p) => {
            println!("{}  [{}]  {}  {}", p.design, p.launch, p.title, p.uid);
            println!(
                "    resume: cd {} && claude --resume {}",
                shell_word(&p.cwd),
                p.session
            );
        }
    }
    Ok(())
}

/// `jkb design prompt …`: build a prompt a session starts with, record a session, or list them.
fn prompt_cmd(ops: &Ops<'_>, what: DesignPromptCmd) -> Result<()> {
    let ask = match what {
        DesignPromptCmd::Record {
            uid,
            session,
            cwd,
            launch,
            subject,
            title,
        } => {
            let cwd = match cwd {
                Some(c) => c,
                None => std::env::current_dir()
                    .context("reading the current directory")?
                    .to_str()
                    .context("the current directory is not UTF-8")?
                    .to_owned(),
            };
            return record_prompt(
                ops,
                Request::DesignPromptRecord {
                    uid,
                    session,
                    cwd,
                    launch,
                    // A launch script passes an empty subject for a session on the design itself.
                    subject: subject.filter(|s| !s.trim().is_empty()),
                    title,
                },
            );
        }
        DesignPromptCmd::Ls { uid } => return list_prompts(ops, uid),
        DesignPromptCmd::Of { session } => return prompt_of(ops, session),
        DesignPromptCmd::Discuss { uid, range, base } => {
            let (start, end) = parse_range(&range)?;
            PromptAsk::Discuss {
                uid,
                base,
                start,
                end,
            }
        }
        DesignPromptCmd::Play { plan, strategy } => PromptAsk::Play { plan, strategy },
        DesignPromptCmd::Task { uid } => PromptAsk::Task { uid },
        DesignPromptCmd::New { uid, text } => {
            let text = if text.len() == 1 && text[0] == "-" {
                text_arg("-".to_owned())?
            } else {
                text.join(" ")
            };
            PromptAsk::New { uid, text }
        }
    };
    match ops.call(Request::DesignPrompt { ask })? {
        Response::DesignPrompt { prompt } => print_prompt(ops, &prompt, &prompt.prompt),
        Response::DesignWorkPrompt { prompt } => print_prompt(ops, &prompt, &prompt.prompt),
        Response::DesignNewPrompt { prompt } => print_prompt(ops, &prompt, &prompt.prompt),
        other => unexpected("design.prompt", &other),
    }
}

/// `jkb design …`.
///
/// # Errors
/// The op's refusal, or unreadable stdin.
#[allow(clippy::too_many_lines)] // one arm per subcommand
pub(crate) fn run(ops: &Ops<'_>, cmd: DesignCmd, global: bool) -> Result<()> {
    match cmd {
        DesignCmd::Ls { repo, all } => {
            let repo = if all || global {
                None
            } else {
                // Outside a repo every design is listed; an ambient lookup that fails is an error,
                // never a reason to list everything.
                match repo {
                    Some(r) => Some(r),
                    None => ops.ambient_repo()?,
                }
            };
            let designs = match ops.call(Request::DesignList { repo })? {
                Response::Designs { designs } => designs,
                other => return unexpected("design.list", &other),
            };
            if ops.json {
                println!("{}", serde_json::to_string_pretty(&designs)?);
            } else if designs.is_empty() {
                println!("(no designs)");
            } else {
                for d in designs {
                    println!(
                        "{}  {}  [{}] seq={}{}",
                        d.uid,
                        d.title,
                        d.namespace.unwrap_or_default(),
                        d.seq,
                        d.doc_target
                            .map(|t| format!("  exports to {t}"))
                            .unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
        DesignCmd::Create {
            title,
            repo,
            body,
            stdin,
        } => {
            let repo = repo_of(ops, repo)?;
            let body = if stdin {
                text_arg("-".to_owned())?
            } else {
                body.unwrap_or_default()
            };
            let design = match ops.call(Request::DesignCreate {
                repo,
                title: title.join(" "),
                body,
            })? {
                Response::DesignCreated { design } => design,
                other => return unexpected("design.create", &other),
            };
            if ops.json {
                println!("{}", serde_json::to_string_pretty(&design)?);
            } else {
                println!("{}", design.uid);
            }
            Ok(())
        }
        DesignCmd::Cat { uid, plain } => match ops.call(Request::DesignCat { uid })? {
            Response::DesignText { design } => print_doc(ops, &design, plain),
            other => unexpected("design.cat", &other),
        },
        DesignCmd::Edit {
            uid,
            base,
            find,
            insert_after,
            span,
            occurrence,
            replace,
            text,
        } => {
            let edit = match (find, insert_after, span) {
                (Some(find), None, None) => EditAsk::Replace {
                    find,
                    occurrence,
                    with: text_arg(replace.context("--find needs --replace")?)?,
                },
                (None, Some(find), None) => EditAsk::InsertAfter {
                    find,
                    occurrence,
                    text: text_arg(text.context("--insert-after needs --text")?)?,
                },
                (None, None, Some(span)) => {
                    if occurrence.is_some() {
                        bail!("--occurrence picks a quote's match; --span names its text exactly");
                    }
                    EditAsk::Span {
                        span,
                        with: text_arg(replace.context("--span needs --replace")?)?,
                    }
                }
                _ => bail!("give exactly one of --find, --insert-after or --span"),
            };
            let w = written(ops, "design.edit", Request::DesignEdit { uid, base, edit })?;
            print_written(ops, &w)
        }
        DesignCmd::Span {
            uid,
            base,
            find,
            occurrence,
            reviewer,
        } => {
            let w = written(
                ops,
                "design.span",
                Request::DesignSpan {
                    uid,
                    base,
                    find,
                    occurrence,
                    reviewer: Some(reviewer),
                },
            )?;
            print_written(ops, &w)
        }
        DesignCmd::Spans { uid } => {
            let spans = match ops.call(Request::DesignSpans { uid })? {
                Response::DesignSpans { spans } => spans,
                other => return unexpected("design.spans", &other),
            };
            if ops.json {
                println!("{}", serde_json::to_string_pretty(&spans)?);
            } else if spans.is_empty() {
                println!("(no spans — all of the text is PROPOSED)");
            } else {
                for s in &spans {
                    print_span(ops, s)?;
                }
            }
            Ok(())
        }
        DesignCmd::Approve { span, base } => {
            let s = span_answer(ops, "design.approve", Request::DesignApprove { span, base })?;
            print_span(ops, &s)
        }
        DesignCmd::Stage { span, step } => {
            let s = span_answer(ops, "design.stage", Request::DesignStage { span, step })?;
            print_span(ops, &s)
        }
        DesignCmd::State { uid, since } => match ops.call(Request::DesignState { uid, since })? {
            Response::DesignUpdate {
                update,
                version,
                seq,
            } => {
                if ops.json {
                    println!(
                        "{}",
                        serde_json::json!({ "update": update, "version": version, "seq": seq })
                    );
                } else {
                    println!("version: {version}");
                    println!("{update}");
                }
                Ok(())
            }
            other => unexpected("design.state", &other),
        },
        DesignCmd::Apply { uid, update } => {
            let update = text_arg(update)?.trim().to_owned();
            let w = written(ops, "design.apply", Request::DesignApply { uid, update })?;
            print_written(ops, &w)
        }
        DesignCmd::Export {
            uid,
            to,
            all: _,
            repo,
            check,
        } => crate::design_export::run(ops, uid, to.as_deref(), repo, check),
        DesignCmd::Source { uid, paths } => crate::design_export::source(ops, uid, &paths),
        DesignCmd::Prompt { what } => prompt_cmd(ops, what),
        DesignCmd::Plan { what } => plan_cmd(ops, what),
        DesignCmd::Compact { uid } => {
            match ops.call(Request::DesignCompact { uid: uid.clone() })? {
                Response::DesignCompacted { through, removed } => {
                    if ops.json {
                        println!(
                            "{}",
                            serde_json::json!({ "uid": uid, "through": through, "removed": removed })
                        );
                    } else {
                        println!("{uid}: compacted through update {through} ({removed} folded)");
                    }
                    Ok(())
                }
                other => unexpected("design.compact", &other),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn a_range_is_two_utf16_offsets() {
        assert_eq!(parse_range("3..8").unwrap(), (3, 8));
        assert_eq!(parse_range(" 0 .. 12 ").unwrap(), (0, 12));
        for bad in ["3", "3..", "..8", "a..b", "-1..2", "3-8"] {
            assert!(parse_range(bad).is_err(), "{bad}");
        }
    }
}
