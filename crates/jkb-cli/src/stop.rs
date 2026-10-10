//! The stop request of jkb's long-running commands — `jkb serve`, `sync --watch`, `task reap
//! --watch`: SIGINT (Ctrl-C) and SIGTERM (what launchd and systemd send to stop a service).
//!
//! **SIGHUP is not handled, on purpose.** ctrlc's `termination` feature adds SIGTERM, but SIGHUP
//! with it, and `ctrlc::set_handler` installs over whatever disposition was there (ctrlc 3.5.2,
//! `init_os_handler(overwrite = true)`; only `try_set_handler` refuses a non-default one, and it
//! refuses all three, rolled back). Under `nohup`, whose whole point is an ignored SIGHUP, the hangup
//! became a stop. So SIGTERM is registered here on its own, through tokio's signal handling (safe,
//! and the tokio already locked), and SIGHUP keeps the disposition it was started with: ignored under
//! `nohup`, the default (terminate) otherwise.
//!
//! Residual: ctrlc still installs over an ignored SIGINT — a shell's `&` ignores it — so `kill -INT`
//! stops a backgrounded job cleanly where it was ignored before. Nothing sends that by accident: a
//! background job does not get the terminal's Ctrl-C.

use std::sync::Arc;

use anyhow::{Context as _, Result};

/// Call `stop` with the signal's name on each SIGINT or SIGTERM. Both handlers are in place when this
/// returns; on an error, one may be.
///
/// # Errors
/// Installing either handler failed.
pub fn on_stop(stop: impl Fn(&'static str) + Send + Sync + 'static) -> Result<()> {
    let stop = Arc::new(stop);
    let on_int = Arc::clone(&stop);
    ctrlc::set_handler(move || on_int("SIGINT")).context("installing the SIGINT handler")?;
    on_term(move || stop("SIGTERM"))
}

#[cfg(unix)]
fn on_term(stop: impl Fn() + Send + 'static) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the SIGTERM listener")?;
    // Registered here, before returning, not on the thread: the handler is in place once this has.
    let mut term = {
        let _in = runtime.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("installing the SIGTERM handler")?
    };
    std::thread::Builder::new()
        .name("jkb-sigterm".to_owned())
        .spawn(move || {
            runtime.block_on(async move {
                while term.recv().await.is_some() {
                    stop();
                }
            });
        })
        .context("starting the SIGTERM listener")?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // the unix one can fail
fn on_term(_stop: impl Fn() + Send + 'static) -> Result<()> {
    Ok(())
}
