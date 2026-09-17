//! Install jkb's background jobs as OS services so they run at login (task 11.6, D49).
//!
//! TWO units, because they are two jobs. The **sync watcher** (`jkb sync --watch`) reconciles
//! file mounts; `jkb_sync::watch`/`watch_all` are foreground/blocking, fine to run by hand and not
//! durable. The **worktree reaper** (`jkb task reap --watch`) archives session worktrees a
//! sandboxed `jkb task land` could not move — a session may not unlink its own `.claude` policy
//! files, so something outside it has to finish the disposal — and deletes those archives once
//! they age out. Kept apart deliberately: a wedged file watcher must not also stop every deferred
//! landing on the machine from completing.
//!
//! Each is a launchd agent (macOS) or a systemd **user** unit (Linux), kept alive. The unit
//! generators are pure so they're unit-tested; installation just writes the files and prints the
//! commands to activate them (we don't spawn the services ourselves).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The launchd label / systemd unit stem for the file-sync watcher.
pub const LABEL: &str = "com.jkb.sync";

/// ...and for the worktree reaper. A SECOND unit rather than another job for the watcher: they
/// answer to different failures and run on different rhythms, and folding the sweep into
/// `sync --watch` would mean a wedged file watcher also stops every deferred landing on the
/// machine from ever completing.
///
/// It exists because a session cannot dispose of its own worktree — Claude Code protects a
/// project's `.claude` policy files from the agent whose policy they are — so `jkb task land`
/// records what it could not move and something outside that session finishes it. This is that
/// something (design D49).
pub const REAP_LABEL: &str = "com.jkb.reap";

/// ...and for `jkb serve`, the daemon that owns the database for processes that must not open it
/// themselves — the dev container's `jkb` (design r3.2 H3). A third unit, not a job inside another:
/// a crashed file watcher must not take the container's access to the knowledge base with it.
pub const SERVE_LABEL: &str = "com.jkb.serve";

/// Which OS service manager to target (chosen by `cfg!` at the call site).
#[derive(Clone, Copy)]
enum Manager {
    Launchd,
    Systemd,
}

fn manager() -> Result<Manager> {
    if cfg!(target_os = "macos") {
        Ok(Manager::Launchd)
    } else if cfg!(target_os = "linux") {
        Ok(Manager::Systemd)
    } else {
        bail!("`jkb service` supports macOS (launchd) and Linux (systemd) only")
    }
}

/// One installable unit: its label, where it goes, and what goes in it.
type Unit = (&'static str, PathBuf, String);

/// Print the service units for this platform to stdout (a dry run of `install`).
///
/// # Errors
/// Returns an error if the current executable or working directory can't be resolved.
pub fn print(db: &Path) -> Result<()> {
    for (_, _, unit) in units_for_platform(db)? {
        print!("{unit}");
    }
    Ok(())
}

/// Write the service units to their platform locations and print the activation commands.
///
/// # Errors
/// Returns an error on an unsupported platform, or if a file can't be written.
pub fn install(db: &Path) -> Result<()> {
    install_units(manager()?, units_for_platform(db)?)
}

/// Write each unit to its own path and say how to activate it.
///
/// Split from [`install`] so the destinations are an ARGUMENT rather than something derived
/// from `$HOME` inside `units_for_platform`. That derivation is the reason nothing could
/// reach this code: reverting its `atomic::write` to `std::fs::write` passed the whole gate,
/// because `atomic::write`'s own tests exercise the seam in isolation and no test could get
/// here to assert that this caller goes through it. Same split, for the same reason, as
/// `commands::install_into`.
///
/// # Errors
/// Returns an error if a unit's directory cannot be created or the unit cannot be written.
fn install_units(manager: Manager, units: Vec<Unit>) -> Result<()> {
    for (label, path, unit) in units {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // `atomic::write`, never `fs::write`: a supervisor may be reading this unit as we
        // replace it (`launchctl load` / `systemctl daemon-reload`), and setup.sh re-runs
        // `service install` on every pull that touched `scripts/`.
        crate::atomic::write(&path, unit.as_bytes())?;
        println!("wrote {}", path.display());
        // A RESTART, not `load` / `enable --now`: those leave an already-running unit on the old
        // binary, and a daemon older than the database refuses every request (schema_newer). The
        // same rule as setup.sh's `activate_services`.
        match manager {
            Manager::Launchd => println!(
                "activate with: launchctl unload {path} 2>/dev/null; launchctl load {path}",
                path = path.display()
            ),
            Manager::Systemd => println!(
                "activate with: systemctl --user daemon-reload && systemctl --user enable {label} \
                 && systemctl --user restart {label}"
            ),
        }
    }
    Ok(())
}

/// Remove the installed service units (if present) and print the deactivation commands.
///
/// # Errors
/// Returns an error on an unsupported platform, or if removing a file fails.
pub fn uninstall(db: &Path) -> Result<()> {
    let manager = manager()?;
    for (label, path, _) in units_for_platform(db)? {
        match manager {
            Manager::Launchd => {
                println!("deactivate first with: launchctl unload {}", path.display());
            }
            Manager::Systemd => {
                println!("deactivate first with: systemctl --user disable --now {label}");
            }
        }
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            println!("removed {}", path.display());
        } else {
            println!("no service unit at {}", path.display());
        }
    }
    Ok(())
}

/// Print every unit [`install`] writes for this platform, one per line, tab-separated: its label, the
/// path it is installed at, and its role — `serve` for the daemon, `watcher` for the rest — so
/// setup.sh activates exactly these, derives no path of its own, and reports the daemon on its own
/// line.
///
/// # Errors
/// As [`install`], for an unsupported platform or no `HOME`.
pub fn units(db: &Path) -> Result<()> {
    for (label, path, _) in units_for_platform(db)? {
        let role = if label == SERVE_LABEL {
            "serve"
        } else {
            "watcher"
        };
        println!("{label}\t{}\t{role}", path.display());
    }
    Ok(())
}

/// Where `jkb serve` listening on `port` writes its token when not told otherwise:
/// `~/.jkb/daemon/<port>/token`, **whichever database it serves**. The daemon writes it only once its
/// port is bound, which is what setup.sh waits for.
///
/// **Keyed by what a client knows.** A client knows the daemon's address and never its database:
/// the notification hook opens none, and the dev container sees the host's `~/.jkb` through a bind
/// while its own `JKB_DB` names a container-local file. Beside the database, a host set up with
/// `--db ~/kb/jkb.db` wrote `~/kb/daemon/token` where no client looked. One path per home instead let
/// a second daemon on another port overwrite the first's live token, locking out every client of
/// the first (both found by the stage-5 reviews). The port is what separates two daemons in one home.
#[must_use]
pub fn serve_token_path(port: u16) -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join(format!(".jkb/daemon/{port}/token"))
}

/// Every `(label, install path, contents)` for the current platform.
fn units_for_platform(db: &Path) -> Result<Vec<Unit>> {
    let exe = std::env::current_exe().context("resolving the jkb executable path")?;
    let db = absolute(db)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    Ok(match manager()? {
        Manager::Launchd => vec![
            (
                LABEL,
                home.join("Library/LaunchAgents")
                    .join(format!("{LABEL}.plist")),
                launchd_plist(&exe, &db),
            ),
            (
                REAP_LABEL,
                home.join("Library/LaunchAgents")
                    .join(format!("{REAP_LABEL}.plist")),
                launchd_reap_plist(&exe, &db),
            ),
            (
                SERVE_LABEL,
                home.join("Library/LaunchAgents")
                    .join(format!("{SERVE_LABEL}.plist")),
                launchd_serve_plist(&exe, &db),
            ),
        ],
        Manager::Systemd => vec![
            (
                LABEL,
                home.join(".config/systemd/user")
                    .join(format!("{LABEL}.service")),
                systemd_unit(&exe, &db),
            ),
            (
                REAP_LABEL,
                home.join(".config/systemd/user")
                    .join(format!("{REAP_LABEL}.service")),
                systemd_reap_unit(&exe, &db),
            ),
            (
                SERVE_LABEL,
                home.join(".config/systemd/user")
                    .join(format!("{SERVE_LABEL}.service")),
                systemd_serve_unit(&exe, &db),
            ),
        ],
    })
}

/// Make `path` absolute (join the cwd if relative) so the service works regardless of
/// where it is launched from.
fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// A launchd agent plist running the all-mounts watcher, kept alive across restarts.
fn launchd_plist(exe: &Path, db: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    // Logs live beside the database, which is the one directory we already know exists and is
    // the user's. `sync --watch`'s reports are the only record of a destructive resolution.
    let log_dir = db.parent().unwrap_or_else(|| Path::new("/tmp"));
    let log_out = xml_escape(&log_dir.join("sync.log").to_string_lossy());
    let log_err = xml_escape(&log_dir.join("sync.log").to_string_lossy());
    let db = xml_escape(&db.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--db</string>
        <string>{db}</string>
        <string>sync</string>
        <string>--watch</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <!-- launchd sends a job's stdio to /dev/null unless told otherwise, so without these the
         watcher's reporting is a no-op on the platform this is developed on: a `kb_wins`
         resolution discards one side's edits, settles the journal `ok`, and leaves no trace on
         any surface — `jkb doctor` included. -->
    <key>StandardOutPath</key>
    <string>{log_out}</string>
    <key>StandardErrorPath</key>
    <string>{log_err}</string>
</dict>
</plist>
"#
    )
}

/// A systemd **user** unit running the all-mounts watcher, restarted on failure.
fn systemd_unit(exe: &Path, db: &Path) -> String {
    let exe = exe.to_string_lossy();
    let db = db.to_string_lossy();
    format!(
        "[Unit]\n\
         Description=jkb knowledge base file-sync watcher\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} --db {db} sync --watch\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// A launchd agent plist running the worktree reaper.
///
/// `--watch` rather than launchd's own `StartInterval`, so the two platforms run the same code
/// path: one long-lived process sweeping on a timer, restarted if it dies. A `StartInterval` job
/// here and a systemd timer there would be two schedulers to reason about for one sweep.
fn launchd_reap_plist(exe: &Path, db: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    let log_dir = db.parent().unwrap_or_else(|| Path::new("/tmp"));
    let log = xml_escape(&log_dir.join("reap.log").to_string_lossy());
    let db = xml_escape(&db.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{REAP_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--db</string>
        <string>{db}</string>
        <string>task</string>
        <string>reap</string>
        <string>--watch</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <!-- The sweep archives a worktree and, a month later, deletes that archive. Both are things
         somebody may want to look up afterwards, and launchd sends a job's stdio to /dev/null
         unless told otherwise. -->
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// A systemd **user** unit running the worktree reaper. See [`launchd_reap_plist`] for why this
/// is a long-lived `--watch` process rather than a timer.
fn systemd_reap_unit(exe: &Path, db: &Path) -> String {
    let exe = exe.to_string_lossy();
    let db = db.to_string_lossy();
    format!(
        "[Unit]\n\
         Description=jkb session worktree archiver\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} --db {db} task reap --watch\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// A launchd agent plist running `jkb serve`, kept alive, logging beside the database.
fn launchd_serve_plist(exe: &Path, db: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    let log_dir = db.parent().unwrap_or_else(|| Path::new("/tmp"));
    let log = xml_escape(&log_dir.join("serve.log").to_string_lossy());
    let db = xml_escape(&db.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--db</string>
        <string>{db}</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// A systemd **user** unit running `jkb serve`, restarted on failure.
fn systemd_serve_unit(exe: &Path, db: &Path) -> String {
    let exe = exe.to_string_lossy();
    let db = db.to_string_lossy();
    format!(
        "[Unit]\n\
         Description=jkb knowledge base daemon (serves the dev container)\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} --db {db} serve\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Minimal XML escaping for text that goes inside plist `<string>` elements.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::{
        install_units, launchd_plist, launchd_reap_plist, launchd_serve_plist, systemd_reap_unit,
        systemd_serve_unit, systemd_unit, xml_escape, Manager, LABEL, REAP_LABEL, SERVE_LABEL,
    };
    use std::path::Path;

    #[test]
    fn launchd_plist_runs_the_watcher() {
        let plist = launchd_plist(
            Path::new("/usr/local/bin/jkb"),
            Path::new("/home/u/.jkb/jkb.db"),
        );
        assert!(plist.contains(&format!("<string>{LABEL}</string>")));
        assert!(plist.contains("<string>/usr/local/bin/jkb</string>"));
        assert!(plist.contains("<string>/home/u/.jkb/jkb.db</string>"));
        assert!(plist.contains("<string>--watch</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        // Without these launchd discards the watcher's output, which is where a destructive
        // `kb_wins` resolution is reported — and nowhere else.
        assert!(plist.contains("<key>StandardErrorPath</key>"));
        assert!(plist.contains("sync.log"));
    }

    #[test]
    fn systemd_unit_runs_the_watcher() {
        let unit = systemd_unit(Path::new("/usr/bin/jkb"), Path::new("/home/u/.jkb/jkb.db"));
        assert!(unit.contains("ExecStart=/usr/bin/jkb --db /home/u/.jkb/jkb.db sync --watch"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_reap_plist_runs_the_reaper_under_its_own_label() {
        let plist = launchd_reap_plist(
            Path::new("/usr/local/bin/jkb"),
            Path::new("/home/u/.jkb/jkb.db"),
        );
        assert!(plist.contains(&format!("<string>{REAP_LABEL}</string>")));
        assert_ne!(REAP_LABEL, LABEL, "two jobs cannot share one launchd label");
        assert!(plist.contains("<string>task</string>"));
        assert!(plist.contains("<string>reap</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        // Its own log: an archive made and an archive deleted are both worth looking up later,
        // and without these launchd sends the whole record to /dev/null.
        assert!(plist.contains("reap.log"));
    }

    #[test]
    fn the_serve_units_run_the_daemon_under_their_own_label() {
        let plist = launchd_serve_plist(
            Path::new("/usr/local/bin/jkb"),
            Path::new("/home/u/.jkb/jkb.db"),
        );
        assert!(plist.contains(&format!("<string>{SERVE_LABEL}</string>")));
        assert!(![LABEL, REAP_LABEL].contains(&SERVE_LABEL));
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("serve.log"));
        let unit = systemd_serve_unit(Path::new("/usr/bin/jkb"), Path::new("/home/u/.jkb/jkb.db"));
        assert!(unit.contains("ExecStart=/usr/bin/jkb --db /home/u/.jkb/jkb.db serve"));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn systemd_reap_unit_runs_the_reaper() {
        let unit = systemd_reap_unit(Path::new("/usr/bin/jkb"), Path::new("/home/u/.jkb/jkb.db"));
        assert!(unit.contains("ExecStart=/usr/bin/jkb --db /home/u/.jkb/jkb.db task reap --watch"));
        assert!(unit.contains("Restart=on-failure"));
    }

    /// Pins the CALLER, not the seam. `atomic::write` has its own test, and it stays green
    /// when this function is reverted to `std::fs::write` — writing works; it is the reader
    /// mid-`launchctl load` that an in-place rewrite corrupts. So this holds a handle open
    /// across a reinstall, the way a supervisor does, and asserts it still sees the whole old
    /// unit. Reverting `install_units`'s `atomic::write` fails it with "the reinstall rewrote
    /// a unit a supervisor was already reading".
    ///
    /// It can exist at all only because `install_units` takes its destinations as an
    /// argument. While `install` derived them from `$HOME`, this call site was unreachable.
    #[test]
    #[cfg(unix)]
    fn installing_a_unit_replaces_it_by_rename_never_rewriting_it_in_place() {
        use std::io::Read;
        const PREVIOUS: &str = "<!-- the unit that was installed before this upgrade -->\n";

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub").join(format!("{LABEL}.plist"));
        let unit = || {
            vec![(
                LABEL,
                path.clone(),
                launchd_plist(Path::new("/usr/local/bin/jkb"), Path::new("/home/u/jkb.db")),
            )]
        };

        install_units(Manager::Launchd, unit()).expect("first install");
        assert!(path.is_file(), "install_units created no unit");
        // Stand the installed unit in for an older version of itself. The generators are
        // deterministic, so a second install writes identical bytes and the assertion below
        // could not tell a rename from a truncate-and-rewrite.
        std::fs::write(&path, PREVIOUS).expect("plant an older version");

        let mut supervisor = std::fs::File::open(&path).expect("open the installed unit");
        install_units(Manager::Launchd, unit()).expect("reinstall");

        let mut seen = String::new();
        supervisor
            .read_to_string(&mut seen)
            .expect("read through the open handle");
        assert_eq!(
            seen, PREVIOUS,
            "the reinstall rewrote a unit a supervisor was already reading"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            launchd_plist(Path::new("/usr/local/bin/jkb"), Path::new("/home/u/jkb.db")),
            "the upgrade did not land"
        );
    }

    #[test]
    fn xml_escape_handles_specials() {
        assert_eq!(xml_escape("a & b <c>"), "a &amp; b &lt;c&gt;");
    }
}
