//! Remote mode: `jkb` in a process that must not open a database (design r3.2 H2).
//!
//! Set `JKB_REMOTE=http://<host>:<port>` and every command either reaches the knowledge base through
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

use super::{Cli, Command, CommandsCmd};

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
        Command::Mq { .. } => Support::Ported,
        Command::Notify { .. } | Command::Guide | Command::Commands { .. } => Support::NoDatabase,
        Command::Serve { .. } => {
            Support::Refused("the daemon runs on the host, next to the database")
        }
        Command::Ingest { .. }
        | Command::Mount { .. }
        | Command::Sync { .. }
        | Command::Service { .. } => Support::Refused(HOST_ONLY),
        Command::Query { .. }
        | Command::Search { .. }
        | Command::Ns { .. }
        | Command::Tag { .. }
        | Command::Staging { .. }
        | Command::Task { .. }
        | Command::View { .. }
        | Command::Undo { .. }
        | Command::Index { .. }
        | Command::Doctor { .. }
        | Command::Mcp
        | Command::Ls { .. }
        | Command::Grep { .. }
        | Command::Cat { .. }
        | Command::Tree { .. }
        | Command::Find { .. }
        | Command::Recent { .. }
        | Command::Stat { .. }
        | Command::Item { .. }
        | Command::Related { .. }
        | Command::Inv { .. }
        | Command::Blob { .. }
        | Command::History { .. } => Support::Refused(NOT_YET),
    }
}

/// The daemon address from `JKB_REMOTE`, if remote mode is on.
#[must_use]
pub fn target() -> Option<String> {
    std::env::var("JKB_REMOTE")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Where `jkb serve` is, for a process that talks to it without being in remote mode — the
/// notification hook, which never opens a database in any mode. `JKB_REMOTE` when set; else
/// `JKB_DAEMON_URL`, which the dev container sets to the host (`.container/container.json`,
/// checked against the firewall's opening by `.container/check-config.sh`); else the address
/// `jkb serve` binds by default, which is the host's own loopback.
#[must_use]
pub fn daemon_url() -> String {
    daemon_url_from(target(), std::env::var("JKB_DAEMON_URL").ok())
}

fn daemon_url_from(remote: Option<String>, configured: Option<String>) -> String {
    remote
        .or_else(|| {
            configured
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| format!("http://{}", jkb_daemon::DEFAULT_ADDR))
}

/// The file whose recent modification means the daemon was just unreachable, shared by every
/// client in this home so a burst of short-lived processes pays one connect timeout.
#[must_use]
pub fn down_marker() -> PathBuf {
    home().join(".cache/jkb/remote-unreachable")
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// The token file: `JKB_REMOTE_TOKEN_FILE`, else where the host's `jkb serve` writes it for the
/// default database (`~/.jkb/jkb.db`, seen through the bind) — the same function it uses, not a copy.
#[must_use]
pub fn token_file() -> PathBuf {
    std::env::var_os("JKB_REMOTE_TOKEN_FILE")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| super::service::serve_token_path(&home().join(".jkb/jkb.db")))
}

/// The subcommand as typed, for the refusal message: the first argument that is not a flag. `--db`,
/// the one global flag taking a value, is refused before this is asked.
fn subcommand_name() -> String {
    std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .unwrap_or_else(|| "this command".to_owned())
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
        Support::Refused(why) => bail!(
            "jkb {}: not available with JKB_REMOTE set — {why}",
            subcommand_name()
        ),
        Support::NoDatabase => match cli.command {
            Command::Notify { cmd } => {
                super::notify::run(&cmd);
                Ok(())
            }
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
        Support::Ported => match cli.command {
            Command::Mq { cmd } => {
                let backend = match jkb_daemon::client::RemoteBackend::new(remote, token_file()) {
                    Ok(backend) => backend,
                    Err(e) => {
                        let err = super::mq_cli::refused(e.code, e.message);
                        // The same `--json` rule `mq_cli::run` applies once it has a backend.
                        if cli.json {
                            super::mq_cli::print_failure(&cmd, &err);
                        }
                        return Err(err);
                    }
                }
                .with_down_marker(down_marker());
                super::mq_cli::run(&backend, cmd, cli.json)
            }
            _ => bail!("internal: a Ported command with no remote dispatch"),
        },
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{daemon_url_from, support, Support};
    use crate::Cli;

    #[test]
    fn the_daemon_is_remote_mode_s_then_the_configured_one_then_this_host_s() {
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(
            daemon_url_from(s("http://r:1"), s("http://c:2")),
            "http://r:1"
        );
        assert_eq!(daemon_url_from(None, s(" http://c:2 ")), "http://c:2");
        assert_eq!(
            daemon_url_from(None, s("  ")),
            format!("http://{}", jkb_daemon::DEFAULT_ADDR),
            "an empty setting is no setting"
        );
        assert_eq!(
            daemon_url_from(None, None),
            format!("http://{}", jkb_daemon::DEFAULT_ADDR)
        );
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
            vec!["ingest", "/etc/passwd"],
            vec!["service", "install"],
            vec!["serve"],
        ] {
            assert!(
                matches!(support(&parse(&host_only).command), Support::Refused(_)),
                "{host_only:?} must be refused remotely"
            );
        }
        assert!(matches!(
            support(&parse(&["task", "next"]).command),
            Support::Refused(_)
        ));
    }
}
