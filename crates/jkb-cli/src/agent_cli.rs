//! `jkb workflow agent …` (design D53.7) as a client of the `workflow.agent*` ops: the agent
//! templates the workflow scripts read, and what the Code Factory's Workflows tab calls for the same
//! work.
//!
//! A script reads its prompt with `jkb workflow agent show <name> --var k=v …`: the operator's copy
//! when one overrides the packaged template, filled in, refused when a placeholder is left empty or
//! a value names none. `export` is the one verb that writes a file: the template, as the next
//! version in the checkout's packaged-templates file — the commit a contribution makes.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use clap::{Args, Subcommand};
use jkb_api::workflows::{AgentEdit, AgentView};
use jkb_api::{Backend, Request, Response};
use jkb_core::workflow::agents::{self, AgentPermissions, Isolation, Writes};
use serde_json::json;

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Every template, as the one in effect.
    #[command(alias = "ls")]
    List,
    /// One template: the one in effect, unless `--packaged` or `--version` says otherwise. With
    /// `--var`, only its prompt, filled in — what a workflow script reads.
    Show {
        /// Its name.
        name: String,
        /// The packaged template, whatever copy overrides it.
        #[arg(long, conflicts_with = "version")]
        packaged: bool,
        /// This version of the operator copy.
        #[arg(long)]
        version: Option<i64>,
        /// Fill a placeholder: `--var name=value`, once per placeholder, every one of them.
        #[arg(long = "var", value_name = "NAME=VALUE")]
        vars: Vec<String>,
    },
    /// Copy a template into an operator copy (operator). Under its own name the copy overrides the
    /// packaged template; `--packaged` copies the packaged text back over an existing copy. The
    /// edit flags `set` takes apply to the copied text in the same write: a refused edit leaves no
    /// copy behind, so copying a packaged template to change it is this one command.
    Copy {
        /// The template to copy.
        from: String,
        /// The copy's name.
        #[arg(long = "as")]
        as_name: Option<String>,
        /// Copy the packaged text even when a copy overrides it.
        #[arg(long)]
        packaged: bool,
        #[command(flatten)]
        edit: EditArgs,
    },
    /// Edit an operator copy (operator), appending a version.
    Set {
        /// The copy.
        name: String,
        #[command(flatten)]
        edit: EditArgs,
    },
    /// Write a template into the repository's packaged-templates file as its next version — what a
    /// contribution commits. Run in the checkout the contribution is made from.
    Export {
        /// The template (the one in effect).
        name: String,
        /// The packaged-templates file; by default the one in the current directory's repository.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Export even though the file packages a different version of the template than the one
        /// it is built on — a deliberate revert of whatever changed upstream since. Without it
        /// that is refused, naming both versions.
        #[arg(long)]
        override_base: bool,
    },
}

/// The fields `set` changes, and `copy` changes in the copy it makes.
#[derive(Args)]
pub struct EditArgs {
    /// The new prompt, from a file (`-` reads stdin).
    #[arg(long, value_name = "FILE")]
    template_file: Option<PathBuf>,
    /// operator, coordinator, designer, implementer, reviewer or `systemic_reviewer`.
    #[arg(long)]
    role: Option<String>,
    /// One line on what it does.
    #[arg(long)]
    describe: Option<String>,
    /// Where it runs: none or worktree.
    #[arg(long)]
    isolation: Option<String>,
    /// The model it runs on; `session` for the session's own.
    #[arg(long)]
    model: Option<String>,
    /// The most it may change: nothing, kb, git or code.
    #[arg(long)]
    writes: Option<String>,
    /// The agents it hands off to, comma-separated (empty for none).
    #[arg(long)]
    hands_off_to: Option<String>,
}

impl EditArgs {
    /// The edit these flags name, or `None` for no flags. A permission flag changes that one
    /// permission of `current`'s, which is read only when one is given.
    fn into_edit(
        self,
        current: impl FnOnce() -> Result<AgentPermissions>,
    ) -> Result<Option<AgentEdit>> {
        let Self {
            template_file,
            role,
            describe,
            isolation: iso,
            model,
            writes: w,
            hands_off_to,
        } = self;
        let permissions = if iso.is_some() || model.is_some() || w.is_some() {
            let current = current()?;
            Some(AgentPermissions {
                isolation: iso.as_deref().map_or(Ok(current.isolation), isolation)?,
                model: match model.as_deref() {
                    None => current.model,
                    Some("session") => None,
                    Some(m) => Some(m.to_owned()),
                },
                writes: w.as_deref().map_or(Ok(current.writes), writes)?,
            })
        } else {
            None
        };
        let edit = AgentEdit {
            template: template_file.as_deref().map(read_template).transpose()?,
            role,
            describe,
            permissions,
            hands_off_to: hands_off_to.map(|h| {
                h.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            }),
        };
        Ok((edit != AgentEdit::default()).then_some(edit))
    }
}

fn call(b: &dyn Backend, r: Request) -> Result<Response> {
    b.call(r).map_err(|e| anyhow::anyhow!("{}", e.message))
}

fn agent_of(op: &str, r: Response) -> Result<(AgentView, Option<String>, bool)> {
    match r {
        Response::WorkflowAgent {
            agent,
            rendered,
            wrote,
        } => Ok((*agent, rendered, wrote)),
        other => bail!("{op}: unexpected answer {other:?}"),
    }
}

fn show(b: &dyn Backend, name: &str) -> Result<AgentView> {
    show_from(b, name, false)
}

fn show_from(b: &dyn Backend, name: &str, packaged: bool) -> Result<AgentView> {
    Ok(agent_of(
        "workflow.agent",
        call(
            b,
            Request::WorkflowAgent {
                name: name.to_owned(),
                packaged,
                version: None,
                vars: None,
            },
        )?,
    )?
    .0)
}

/// `--var name=value`s as a map; a name given twice is refused rather than one value winning.
fn parse_vars(vars: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for v in vars {
        let (k, val) = v
            .split_once('=')
            .with_context(|| format!("--var `{v}`: write it as name=value"))?;
        if out.insert(k.to_owned(), val.to_owned()).is_some() {
            bail!("--var `{k}` is given twice");
        }
    }
    Ok(out)
}

fn read_template(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading the template from stdin")?;
        return Ok(buf);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn isolation(s: &str) -> Result<Isolation> {
    match s {
        "none" => Ok(Isolation::None),
        "worktree" => Ok(Isolation::Worktree),
        _ => bail!("--isolation none or worktree (got `{s}`)"),
    }
}

fn writes(s: &str) -> Result<Writes> {
    match s {
        "nothing" => Ok(Writes::Nothing),
        "kb" => Ok(Writes::Kb),
        "git" => Ok(Writes::Git),
        "code" => Ok(Writes::Code),
        _ => bail!("--writes nothing, kb, git or code (got `{s}`)"),
    }
}

fn print_agent(a: &AgentView) {
    let source = match a.source {
        agents::Source::Packaged => "packaged (read-only)".to_owned(),
        agents::Source::Operator if a.overrides_packaged => {
            format!(
                "operator copy, overriding packaged v{}",
                a.packaged_version.unwrap_or_default()
            )
        }
        agents::Source::Operator => "operator's own".to_owned(),
    };
    println!("{} v{} — {source}", a.name, a.version);
    println!("workflow:     {}", a.workflow);
    println!("role:         {} ({})", a.role, a.role_ops.join(", "));
    println!(
        "permissions:  isolation {}, model {}, writes {}",
        json!(a.permissions.isolation).as_str().unwrap_or("?"),
        a.permissions.model.as_deref().unwrap_or("session"),
        json!(a.permissions.writes).as_str().unwrap_or("?")
    );
    if !a.hands_off_to.is_empty() {
        println!("hands off to: {}", a.hands_off_to.join(", "));
    }
    if !a.placeholders.is_empty() {
        println!("placeholders: {}", a.placeholders.join(", "));
    }
    if let Some(b) = &a.based_on {
        println!("copied from:  {b}");
    }
    if a.behind_packaged {
        println!(
            "note:         the packaged template has a newer version than this was copied from"
        );
    }
    println!("{}\n\n{}", a.describe, a.template);
}

/// Run a `jkb workflow agent` verb.
///
/// # Errors
/// A refused op, a malformed argument, or a file that cannot be read or written.
#[allow(clippy::too_many_lines)] // one arm per verb
pub fn run(b: &dyn Backend, cmd: AgentCmd, json_out: bool) -> Result<()> {
    match cmd {
        AgentCmd::List => {
            let (agents, roles) = match call(b, Request::WorkflowAgents {})? {
                Response::WorkflowAgents { agents, roles } => (agents, roles),
                other => bail!("workflow.agents: unexpected answer {other:?}"),
            };
            if json_out {
                println!("{}", json!({ "agents": agents, "roles": roles }));
                return Ok(());
            }
            for a in &agents {
                let mark = match a.source {
                    agents::Source::Packaged => " ",
                    agents::Source::Operator => "*",
                };
                let behind = if a.behind_packaged { " (behind)" } else { "" };
                println!(
                    "{mark} {:<26} v{:<3} {:<12} {:<12} {}{behind}",
                    a.name, a.version, a.workflow, a.role, a.describe
                );
            }
            Ok(())
        }
        AgentCmd::Show {
            name,
            packaged,
            version,
            vars,
        } => {
            let fill = !vars.is_empty();
            let vars = fill.then(|| parse_vars(&vars)).transpose()?;
            let (a, rendered, _) = agent_of(
                "workflow.agent",
                call(
                    b,
                    Request::WorkflowAgent {
                        name,
                        packaged,
                        version,
                        vars,
                    },
                )?,
            )?;
            if json_out {
                println!("{}", json!({ "agent": a, "rendered": rendered }));
            } else if let Some(prompt) = rendered {
                // Exactly the prompt, for a script to hand to its agent.
                print!("{prompt}");
            } else {
                print_agent(&a);
            }
            Ok(())
        }
        AgentCmd::Copy {
            from,
            as_name,
            packaged,
            edit,
        } => {
            let edit = edit.into_edit(|| Ok(show_from(b, &from, packaged)?.permissions))?;
            let (a, _, _) = agent_of(
                "workflow.agent_copy",
                call(
                    b,
                    Request::WorkflowAgentCopy {
                        from,
                        packaged,
                        as_name,
                        edit,
                    },
                )?,
            )?;
            if json_out {
                println!("{}", json!({ "agent": a }));
            } else {
                println!(
                    "copied {} as {} v{}",
                    a.based_on.as_deref().unwrap_or("?"),
                    a.name,
                    a.version
                );
            }
            Ok(())
        }
        AgentCmd::Set { name, edit } => {
            // No flags is an empty edit, which the op refuses by name.
            let edit = edit
                .into_edit(|| Ok(show(b, &name)?.permissions))?
                .unwrap_or_default();
            let (a, _, wrote) = agent_of(
                "workflow.agent_set",
                call(b, Request::WorkflowAgentSet { name, edit })?,
            )?;
            if json_out {
                println!("{}", json!({ "agent": a, "wrote": wrote }));
            } else if wrote {
                println!("{} is now v{}", a.name, a.version);
            } else {
                println!(
                    "{} v{} already says that: nothing written",
                    a.name, a.version
                );
            }
            Ok(())
        }
        AgentCmd::Export {
            name,
            file,
            override_base,
        } => {
            let file = if let Some(f) = file {
                f
            } else {
                let cwd = std::env::current_dir().context("the current directory")?;
                crate::gitrepo::root(&cwd)?
                    .context(
                        "not inside a git checkout: run it in the jkb checkout the \
                         contribution is made from, or name the file with --file",
                    )?
                    .join(agents::PACKAGED_FILE)
            };
            let text = std::fs::read_to_string(&file).with_context(|| {
                format!("reading the packaged-templates file {}", file.display())
            })?;
            let view = show(b, &name)?;
            let agent = view
                .to_agent()
                .map_err(|e| anyhow::anyhow!("{}", e.message))?;
            let base = agents::ExportBase {
                built_on: view.packaged_base,
                installed: view.packaged_version,
            };
            let (out, version) = agents::export(&text, &agent, base, override_base)?;
            crate::atomic::write(&file, out.as_bytes())?;
            if json_out {
                println!(
                    "{}",
                    json!({ "name": name, "version": version, "file": file.display().to_string() })
                );
            } else {
                println!("{name} packaged as v{version} in {}", file.display());
            }
            Ok(())
        }
    }
}
