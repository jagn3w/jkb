//! `jkb design …` (design D53.4–5) as a client of the `design.*` ops: the same answer on the host and
//! through `jkb serve`, and what the Code Factory app calls for the same work.
//!
//! The edit loop an agent runs is `jkb design cat <uid>` — the text with span markers and a version
//! token — then `jkb design edit <uid> --base <token> --find <quote> --replace <text>`, the quote
//! copied from what `cat` printed. The edit is resolved in the version the token names and merged
//! over anything written since.

use anyhow::{bail, Context as _, Result};
use jkb_api::designs::{DesignDoc, EditAsk, Span, Written};
use jkb_api::{Request, Response};

use crate::ops_cli::{unexpected, Ops};
use crate::DesignCmd;

/// The text `value` names: itself, or stdin for `-`.
fn text_arg(value: String) -> Result<String> {
    if value != "-" {
        return Ok(value);
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).context("reading stdin")?;
    Ok(buf)
}

/// The repo a design lives under: as given, or the ambient one (the first segment after `repos/`).
fn repo_of(ops: &Ops<'_>, repo: Option<String>) -> Result<String> {
    if let Some(repo) = repo {
        return Ok(repo);
    }
    let ambient = ops.ambient_here()?.and_then(|mount| {
        mount
            .strip_prefix("repos/")
            .unwrap_or(&mount)
            .split('/')
            .next()
            .map(str::to_owned)
    });
    ambient.context("not inside a mounted repo — name one with --repo")
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
            let how = if p.removed {
                "removed"
            } else {
                p.state.as_str()
            };
            println!("  {how:<11} {:?}", p.text);
        }
    } else if !s.text.is_empty() {
        println!("  {:?}", s.text);
    }
    Ok(())
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
                match repo {
                    Some(r) => Some(r),
                    None => repo_of(ops, None).ok(),
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
                        "{}  {}  [{}] seq={}",
                        d.uid,
                        d.title,
                        d.namespace.unwrap_or_default(),
                        d.seq
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
        DesignCmd::Approve { span } => {
            let s = span_answer(ops, "design.approve", Request::DesignApprove { span })?;
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
