//! Install the sync watcher as an OS service so it runs at login (task 11.6).
//!
//! `jkb_sync::watch`/`watch_all` are foreground/blocking — fine for a `jkb sync
//! --watch` you run by hand, but not durable. This generates (and optionally writes)
//! a launchd agent (macOS) or systemd **user** unit (Linux) that runs
//! `jkb --db <db> sync --watch` — the all-mounts watcher — and keeps it alive. The
//! unit generators are pure so they're unit-tested; installation just writes the file
//! and prints the one command to activate it (we don't spawn the service ourselves).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The launchd label / systemd unit stem.
pub const LABEL: &str = "com.jkb.sync";

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

/// Print the service unit for this platform to stdout (a dry run of `install`).
///
/// # Errors
/// Returns an error if the current executable or working directory can't be resolved.
pub fn print(db: &Path) -> Result<()> {
    let (_, unit) = unit_for_platform(db)?;
    print!("{unit}");
    Ok(())
}

/// Write the service unit to its platform location and print the activation command.
///
/// # Errors
/// Returns an error on an unsupported platform, or if the file can't be written.
pub fn install(db: &Path) -> Result<()> {
    let (path, unit) = unit_for_platform(db)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    match manager()? {
        Manager::Launchd => println!("activate with: launchctl load {}", path.display()),
        Manager::Systemd => {
            println!("activate with: systemctl --user daemon-reload && systemctl --user enable --now {LABEL}");
        }
    }
    Ok(())
}

/// Remove the installed service unit (if present) and print the deactivation command.
///
/// # Errors
/// Returns an error on an unsupported platform, or if removing the file fails.
pub fn uninstall(db: &Path) -> Result<()> {
    let (path, _) = unit_for_platform(db)?;
    match manager()? {
        Manager::Launchd => println!("deactivate first with: launchctl unload {}", path.display()),
        Manager::Systemd => {
            println!("deactivate first with: systemctl --user disable --now {LABEL}");
        }
    }
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        println!("removed {}", path.display());
    } else {
        println!("no service unit at {}", path.display());
    }
    Ok(())
}

/// The `(install path, unit contents)` for the current platform.
fn unit_for_platform(db: &Path) -> Result<(PathBuf, String)> {
    let exe = std::env::current_exe().context("resolving the jkb executable path")?;
    let db = absolute(db)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    match manager()? {
        Manager::Launchd => Ok((
            home.join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
            launchd_plist(&exe, &db),
        )),
        Manager::Systemd => Ok((
            home.join(".config/systemd/user")
                .join(format!("{LABEL}.service")),
            systemd_unit(&exe, &db),
        )),
    }
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
    use super::{launchd_plist, systemd_unit, xml_escape, LABEL};
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
    fn xml_escape_handles_specials() {
        assert_eq!(xml_escape("a & b <c>"), "a &amp; b &lt;c&gt;");
    }
}
