//! Remote mode: `jkb` in a process that must not open a database (design r3.2 H2).
//!
//! Set `JKB_REMOTE=http://<host>:<port>` (or bare `<host>:<port>`) and every command either reaches the knowledge base through
//! `jkb serve` or is refused — before it has done anything. It is for the dev container, because a
//! process on the container's kernel opening the host's `jkb.db` corrupts it — but the container
//! does not set it yet: that waits for the cutover (tasks S6), since remote mode refuses `JKB_DB` and
//! every unported command, and the container's agents still use both.
//!
//! **The table is a `match`, not a list.** [`support`] names every [`Command`] with no wildcard arm,
//! so adding a subcommand does not compile until somebody decides whether it may run remotely. A
//! list with a test that every subcommand appears would say the same thing one build later.

use std::path::PathBuf;

use anyhow::{bail, Result};

use super::{Cli, Command, CommandsCmd, NsCmd, TaskCmd};

/// How a command behaves with `JKB_REMOTE` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Served through the daemon.
    Ported,
    /// Needs no database at all, so it runs here as usual.
    NoDatabase,
    /// Refused, with the reason.
    Refused(&'static str),
}

const HOST_ONLY: &str =
    "it acts on the host itself (host files, services or processes), so it runs \
                         on the host, never through the daemon";
const NOT_YET: &str = "not ported to the daemon yet; run it on the host";

/// Whether `command` may run in remote mode. Exhaustive on purpose — see the module doc.
#[must_use]
pub const fn support(command: &Command) -> Support {
    match command {
        Command::Notify { .. } | Command::Guide | Command::Commands { .. } => Support::NoDatabase,
        Command::Serve { .. } => {
            Support::Refused("the daemon runs on the host, next to the database")
        }
        Command::Mount { .. }
        | Command::Sync { .. }
        | Command::Service { .. }
        // It embeds with the host's model (tasks F5).
        | Command::Index { .. }
        // A repair and a copy of the database change the host (design-s6-4.md I); the report does not.
        | Command::Doctor {
            fix: true, ..
        }
        | Command::Doctor {
            backup: Some(_), ..
        }
        | Command::Task {
            // A sweep over every task; and breaking a land lease is the host operator's escape:
            // nothing here can prove its holder gone.
            cmd: TaskCmd::Mirror
                | TaskCmd::Land {
                    break_lock: true,
                    ..
                },
        } => Support::Refused(HOST_ONLY),
        // The queue, and the agent read set (tasks S6.1). `ops_cli::handles` names the same reads for
        // dispatch; `the_ported_reads_are_the_ones_ops_cli_handles` holds the two together.
        Command::Mq { .. }
        | Command::Query { .. }
        | Command::Search { .. }
        | Command::Find { .. }
        | Command::Recent { .. }
        | Command::Ls { .. }
        | Command::Tree { .. }
        | Command::Grep { .. }
        | Command::Cat { .. }
        // Parsed here, stored there (tasks S6.3): the daemon is sent only the extracted text.
        | Command::Ingest { .. }
        | Command::Doctor { .. }
        // git here, the tasks and findings through the ops (stage 5).
        | Command::Staging { .. }
        // Any item, the edge walk and the sync archive (stage 5).
        | Command::Stat { .. }
        | Command::Item { .. }
        | Command::Related { .. }
        | Command::Blob { .. }
        | Command::History { .. }
        | Command::Inv { .. }
        // Every tool an op; a file or URL is read here and only its text sent (design-s6-4.md K).
        | Command::Mcp
        | Command::Ns {
            cmd: NsCmd::Ls { .. } | NsCmd::Mv { .. },
        }
        | Command::Task {
            cmd:
                TaskCmd::Next { .. }
                | TaskCmd::Show { .. }
                | TaskCmd::Subtasks { .. }
                | TaskCmd::Why { .. }
                // The task-mutate set (tasks S6.2); `jkb_api::tasks` refuses the writes that would
                // have this host's sync write a file outside the container's view.
                | TaskCmd::Add { .. }
                | TaskCmd::Set { .. }
                | TaskCmd::Edit { .. }
                | TaskCmd::Tag { .. }
                | TaskCmd::Depend { .. }
                | TaskCmd::Undepend { .. }
                | TaskCmd::Place { .. }
                | TaskCmd::Unplace { .. }
                | TaskCmd::Bind { .. }
                | TaskCmd::Claim { .. }
                | TaskCmd::Release { .. }
                // The session verbs (tasks S6.4): git here, the database through the ops — the
                // worktree-removal records and the sweep lease included (stage 3).
                | TaskCmd::Start { .. }
                | TaskCmd::Work { .. }
                | TaskCmd::Abandon { .. }
                | TaskCmd::Sessions
                // The land lock is a lease, and a gate is run, never stored, from here (stage 4).
                | TaskCmd::Land {
                    break_lock: false,
                    ..
                }
                | TaskCmd::Landed { .. }
                // A review's findings and its record, and crash recovery, probed here (stage 5).
                | TaskCmd::Review { .. }
                | TaskCmd::Reclaim { .. }
                // `gh` and git here, the history through the ops (stage 5).
                | TaskCmd::Pr { .. }
                | TaskCmd::CloseMerged { .. }
                | TaskCmd::Gate {
                    cmd: None,
                    clear: false,
                },
        } => Support::Ported,
        // A stored gate is a command the host runs later (decision A): a client reads it, and runs it
        // where it is, but never stores one.
        Command::Task {
            cmd: TaskCmd::Gate { .. },
        } => Support::Refused(
            "a stored gate is a shell command the host runs, so only the host stores one; run \
             `jkb task gate` with a command on the host",
        ),
        Command::Ns { .. }
        | Command::Tag { .. }
        | Command::Task { .. }
        | Command::View { .. }
        => Support::Refused(NOT_YET),
        // It reverts any transaction, the host's own included (design-s6-4.md J).
        Command::Undo { .. } => Support::Refused(
            "it reverts any transaction, the host's own included, so it runs only on the host",
        ),
    }
}

/// The daemon address from [`REMOTE_VAR`], if remote mode is on, as a URL. `host:port` with no
/// scheme is taken as `http://host:port` — the shape of `jkb serve --addr`, and the one the dev
/// container sets, because `.container/container.json`'s comment stripper (`lib.sh` `dc_strip`)
/// cannot tell a URL's `//` from a comment.
#[must_use]
pub fn target() -> Option<String> {
    target_from(std::env::var(REMOTE_VAR).ok())
}

/// The variable that switches remote mode on. Spelled once: `.container/check-config.sh` reads the
/// name from this line and holds `container.json`'s `containerEnv` to it, because a rename here with
/// the config left behind puts every `jkb` in the container back on a database of its own — or, since
/// the cutover dropped that database, on none — silently. The notification hook's address drifted
/// that way once, as `JKB_DAEMON_URL` in the code and nothing in the config.
pub const REMOTE_VAR: &str = "JKB_REMOTE";

fn target_from(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .map(|v| {
            if v.contains("://") {
                v
            } else {
                format!("http://{v}")
            }
        })
}

/// Where `jkb serve` is, for a process that talks to it without being in remote mode — the
/// notification hook, which never opens a database in any mode: remote mode's daemon when
/// [`REMOTE_VAR`] is set (the dev container's case), else the address `jkb serve` binds by default,
/// the host's own loopback.
#[must_use]
pub fn daemon_url() -> String {
    target().unwrap_or_else(|| format!("http://{}", jkb_daemon::DEFAULT_ADDR))
}

/// The file whose recent modification means the daemon at `url` was just unreachable, shared by
/// every client of that daemon in this home so a burst of short-lived processes pays one connect
/// timeout. Keyed by port like the token: one marker per home let a daemon that was down on one port
/// make every client of another — a permission prompt's hook among them — give up untried.
#[must_use]
pub fn down_marker(url: &str) -> PathBuf {
    home().join(format!(".cache/jkb/remote-unreachable-{}", port_of(url)))
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// The token file for the daemon at `url`: `JKB_REMOTE_TOKEN_FILE`, else where a `jkb serve` on that
/// URL's port writes it (`~/.jkb/daemon/<port>/token`, seen through the bind in the container) — the
/// same function it uses, not a copy.
#[must_use]
pub fn token_file(url: &str) -> PathBuf {
    std::env::var_os("JKB_REMOTE_TOKEN_FILE")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| super::service::serve_token_path(port_of(url)))
}

/// The port a client of `url` connects to (`http://host:port[/…]`) — and so the port its token and
/// down marker are keyed by. A URL naming none gets its **scheme's** default, 80 or 443, because that
/// is where the HTTP client connects: defaulting to serve's port sent the real daemon's token to
/// whatever listened on :80 (stage-5 review). Such a daemon has no token at `daemon/80/token`, so the
/// failure names the path.
fn port_of(url: &str) -> u16 {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let authority = rest.split('/').next().unwrap_or_default();
    // An IPv6 literal's colons are inside its brackets.
    let after_host = authority
        .rsplit_once(']')
        .map_or(authority, |(_, tail)| tail);
    after_host
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(if scheme.eq_ignore_ascii_case("https") {
            443
        } else {
            80
        })
}

/// The command as typed, for the refusal message: its first `words` arguments that are not flags.
/// `--db`, the one global flag taking a value, is refused before this is asked.
fn subcommand_name(args: impl Iterator<Item = String>, words: usize) -> String {
    let name: Vec<String> = args.filter(|a| !a.starts_with('-')).take(words).collect();
    if name.is_empty() {
        "this command".to_owned()
    } else {
        name.join(" ")
    }
}

/// Run `cli` in remote mode against the daemon at `remote`.
///
/// # Errors
/// A refusal (for `--db` or `JKB_DB`, or a command that may not run remotely), or the command's own
/// error.
pub fn run(cli: Cli, remote: &str) -> Result<()> {
    // A refusal here is a `jkb mq` verb's failure too, so it keeps that verb's `--json` rule.
    let refuse = |message: String| {
        let err = super::mq_cli::refused(jkb_api::ErrorCode::BadRequest, message);
        if let (true, Command::Mq { cmd }) = (cli.json, &cli.command) {
            super::mq_cli::print_failure(cmd, &err);
        }
        Err(err)
    };
    if cli.db.is_some() {
        return refuse(format!(
            "--db is refused with JKB_REMOTE set: this process reaches the knowledge base through \
             jkb serve at {remote} and must not open a database itself"
        ));
    }
    // The same refusal for the environment's way of naming one. Nothing below reads it, but a
    // process configured with both is configured two ways at once — the dev container's interim
    // JKB_DB left in place beside JKB_REMOTE, say — and silently obeying one hides that the other
    // is still set for every tool that is not jkb.
    if std::env::var_os("JKB_DB").is_some_and(|v| !v.is_empty()) {
        return refuse(format!(
            "JKB_DB is set alongside JKB_REMOTE: this process reaches the knowledge base through \
             jkb serve at {remote} and must not name a database itself; unset one of them"
        ));
    }
    match support(&cli.command) {
        Support::Refused(why) => {
            // `task` is served in part, so a refusal names the verb: "jkb task: not available" read
            // as if `jkb task next` were refused too.
            let words = if matches!(cli.command, Command::Task { .. }) {
                2
            } else {
                1
            };
            bail!(
                "jkb {}: not available with JKB_REMOTE set — {why}",
                subcommand_name(std::env::args().skip(1), words)
            )
        }
        Support::NoDatabase => match cli.command {
            Command::Guide => {
                super::cmd_guide();
                Ok(())
            }
            Command::Commands { cmd } => match cmd {
                CommandsCmd::Install => super::commands::install(),
                CommandsCmd::Uninstall => super::commands::uninstall(),
                CommandsCmd::List => super::commands::list(),
            },
            _ => bail!("internal: a NoDatabase command with no remote dispatch"),
        },
        Support::Ported => {
            let backend = match jkb_daemon::client::RemoteBackend::new(remote, token_file(remote)) {
                Ok(backend) => backend.with_down_marker(down_marker(remote)),
                Err(e) => {
                    let err = super::mq_cli::refused(e.code, e.message);
                    // The same `--json` rule `mq_cli::run` applies once it has a backend.
                    if let (true, Command::Mq { cmd }) = (cli.json, &cli.command) {
                        super::mq_cli::print_failure(cmd, &err);
                    }
                    return Err(err);
                }
            };
            match cli.command {
                Command::Mq { cmd } => super::mq_cli::run(&backend, cmd, cli.json),
                Command::Mcp => jkb_mcp::run_stdio(jkb_mcp::Tools {
                    backend: std::sync::Arc::new(backend),
                }),
                command if super::ops_cli::handles(&command) => {
                    super::ops_cli::Ops::new(&backend, cli.global, cli.json, true).run(command)
                }
                _ => bail!("internal: a Ported command with no remote dispatch"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{port_of, subcommand_name, support, target_from, Support};
    use crate::Cli;

    #[test]
    fn the_token_is_found_by_the_port_the_url_names() {
        assert_eq!(port_of("http://host.docker.internal:7117"), 7117);
        assert_eq!(port_of("http://127.0.0.1:7200/"), 7200);
        assert_eq!(port_of("127.0.0.1:7300"), 7300);
        assert_eq!(port_of("http://[::1]:7400"), 7400);
        assert_eq!(
            port_of("http://localhost"),
            80,
            "no port: where the client connects, not serve's default"
        );
        assert_eq!(port_of("https://h/"), 443);
        assert_eq!(
            port_of("http://[::1]"),
            80,
            "a v6 literal's colons are not a port"
        );
        assert_ne!(
            super::down_marker("http://127.0.0.1:7117"),
            super::down_marker("http://127.0.0.1:7200"),
            "one daemon being down says nothing about another"
        );
    }

    #[test]
    fn a_remote_address_without_a_scheme_is_http() {
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(target_from(s("http://r:1")).as_deref(), Some("http://r:1"));
        assert_eq!(
            target_from(s("https://r:1")).as_deref(),
            Some("https://r:1")
        );
        assert_eq!(target_from(s(" c:2 ")).as_deref(), Some("http://c:2"));
        assert_eq!(target_from(s("  ")), None, "an empty setting is no setting");
        assert_eq!(target_from(None), None);
    }

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("jkb").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn only_database_free_and_ported_commands_run_remotely() {
        assert_eq!(
            support(&parse(&["mq", "topic", "ls"]).command),
            Support::Ported
        );
        assert_eq!(support(&parse(&["guide"]).command), Support::NoDatabase);
        for host_only in [
            vec!["sync"],
            vec!["mount", "ls"],
            vec!["service", "install"],
            vec!["serve"],
            vec!["doctor", "--fix"],
            vec!["doctor", "--backup", "/tmp/x.db"],
            vec!["undo"],
            vec!["task", "mirror"],
        ] {
            assert!(
                matches!(support(&parse(&host_only).command), Support::Refused(_)),
                "{host_only:?} must be refused remotely"
            );
        }
        for ported in [
            vec!["doctor"],
            vec!["task", "reclaim"],
            vec!["task", "review", "record", "--findings", "r"],
            vec!["task", "review", "file", "--findings", "r", "--from", "-"],
        ] {
            assert_eq!(
                support(&parse(&ported).command),
                Support::Ported,
                "{ported:?}"
            );
        }
    }

    #[test]
    fn a_refusal_names_as_many_words_as_it_is_asked_for() {
        let args = |v: &[&str]| {
            v.iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert_eq!(
            subcommand_name(args(&["--json", "task", "add", "x"]), 2),
            "task add"
        );
        assert_eq!(subcommand_name(args(&["sync", "--watch"]), 1), "sync");
        assert_eq!(subcommand_name(args(&["--json"]), 2), "this command");
    }

    #[test]
    fn the_ported_reads_are_the_ones_ops_cli_handles() {
        for args in [
            vec!["query", "kind:task"],
            vec!["search", "x"],
            vec!["find", "--kind", "task"],
            vec!["recent"],
            vec!["ls"],
            vec!["tree"],
            vec!["grep", "x"],
            vec!["cat", "u"],
            vec!["ingest", "notes.md"],
            vec!["task", "next"],
            vec!["task", "show", "u"],
            vec!["task", "subtasks", "u"],
            vec!["task", "add", "x"],
            vec!["task", "set", "u", "--priority", "1"],
            vec!["task", "why", "u"],
            vec!["task", "claim", "u"],
            vec!["task", "bind", "u", "--managed"],
            vec!["task", "start", "u"],
            vec!["task", "gate"],
            vec!["task", "gate", "make test"],
            vec!["task", "gate", "--clear"],
            vec!["task", "work", "u"],
            vec!["task", "abandon", "u"],
            vec!["task", "sessions"],
            vec!["task", "reclaim"],
            vec!["task", "review", "record", "--findings", "r"],
            vec!["task", "review", "file", "--findings", "r", "--from", "-"],
            vec!["doctor"],
            vec!["doctor", "--fix"],
            vec!["staging", "ls"],
            vec!["related", "u"],
            vec!["blob", "ls"],
            vec!["blob", "cat", "abcd"],
            vec!["history", "x.md"],
            vec!["item", "rm", "u"],
            vec!["inv", "ls"],
            vec!["task", "pr", "u"],
            vec!["task", "close-merged"],
            vec!["ns", "ls"],
            vec!["ns", "mv", "a", "b"],
            vec!["mcp"],
            vec!["inv", "do", "memory/x", "hypothesize", "t"],
            vec!["task", "mirror"],
            vec!["stat", "u"],
            vec!["ns", "ls"],
            vec!["item", "show", "u"],
            vec!["mq", "topic", "ls"],
            vec!["guide"],
        ] {
            let command = parse(&args).command;
            // The queue and the MCP server are dispatched by their own modules, not `ops_cli`.
            let is_mq = matches!(command, crate::Command::Mq { .. } | crate::Command::Mcp);
            assert_eq!(
                support(&command) == Support::Ported,
                is_mq || crate::ops_cli::handles(&command),
                "{args:?}: remote mode serves exactly the commands something dispatches"
            );
        }
    }
}
