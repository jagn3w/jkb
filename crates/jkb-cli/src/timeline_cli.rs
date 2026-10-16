//! `jkb timeline …`: the lifecycle timeline the Executions tab draws, as a client of the
//! `timeline.*` ops — the same answer on the host and through `jkb serve`.

use anyhow::Result;
use jkb_api::{Request, Response};
use jkb_core::timeline::{Event, SessionSummary};

use crate::ops_cli::{unexpected, Ops};
use crate::TimelineCmd;

/// Run one `jkb timeline` subcommand.
///
/// # Errors
/// The op's refusal, or an answer of the wrong shape.
pub fn run(ops: &Ops<'_>, cmd: TimelineCmd) -> Result<()> {
    match cmd {
        TimelineCmd::Sessions => sessions(ops),
        TimelineCmd::Session { session, after } => session_of(ops, session, after),
        TimelineCmd::Task { uid, after } => task(ops, uid, after),
        TimelineCmd::Plan { uid } => plan(ops, uid),
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// A Unix-ms time as `YYYY-MM-DD HH:MM:SSZ`.
fn when(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = crate::archive::civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}Z",
        h = rem / 3600,
        mi = (rem % 3600) / 60,
        s = rem % 60
    )
}

/// A duration in Unix ms, coarsely: `45s`, `12m`, `3h05m`, `2d04h`.
fn span(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600),
    }
}

fn summary_line(s: &SessionSummary) -> String {
    let last = s.last_event_ms.map_or_else(|| "no events".to_owned(), when);
    let stalled = match (s.stalled, s.stalled_for_ms) {
        (true, Some(q)) => format!("  STALLED {}", span(q)),
        (true, None) => "  STALLED".to_owned(),
        _ => String::new(),
    };
    format!(
        "{}  [{}]  claims {}  last {last}{stalled}",
        s.session, s.state, s.claims
    )
}

fn event_line(e: &Event) -> String {
    let mut parts = vec![format!(
        "{}  {:<8} {}",
        when(e.at_ms),
        e.source.as_str(),
        e.event
    )];
    match (&e.from, &e.to) {
        (Some(f), Some(t)) => parts.push(format!("{f} -> {t}")),
        (None, Some(t)) => parts.push(format!("-> {t}")),
        _ => {}
    }
    parts.extend(e.task.clone());
    if let Some(actor) = &e.actor {
        parts.push(match &e.role {
            Some(role) => format!("by {actor} ({role})"),
            None => format!("by {actor}"),
        });
    }
    if let Some(detail) = &e.detail {
        parts.push(format!("[{detail}]"));
    }
    parts.join("  ")
}

fn more(next: Option<&String>) {
    if let Some(next) = next {
        println!("… more: --after {next}");
    }
}

fn sessions(ops: &Ops<'_>) -> Result<()> {
    let overview = match ops.call(Request::TimelineSessions {})? {
        Response::TimelineSessions { overview } => overview,
        other => return unexpected("timeline.sessions", &other),
    };
    if ops.json {
        return print_json(&overview);
    }
    if overview.sessions.is_empty() {
        println!("(no live sessions and no session holds a claim)");
    }
    for s in &overview.sessions {
        println!("{}", summary_line(s));
        if let Some(cwd) = &s.cwd {
            println!("    {cwd}");
        }
    }
    if !overview.unattributed.is_empty() {
        println!();
        println!("unattributed holders:");
        for h in &overview.unattributed {
            let since = h
                .claimed_at_ms
                .map_or_else(String::new, |at| format!("  since {}", when(at)));
            println!("  {}  {}  by {}{since}", h.uid, h.title, h.owner);
        }
    }
    Ok(())
}

fn session_of(ops: &Ops<'_>, session: String, after: Option<String>) -> Result<()> {
    let timeline = match ops.call(Request::TimelineSession { session, after })? {
        Response::TimelineSession { timeline } => timeline,
        other => return unexpected("timeline.session", &other),
    };
    if ops.json {
        return print_json(&timeline);
    }
    println!("{}", summary_line(&timeline.summary));
    for t in &timeline.tasks {
        let held = if t.holds_claim { "holds" } else { "     " };
        let agents = if t.bound_agents.is_empty() {
            String::new()
        } else {
            format!("  agents {}", t.bound_agents.join(", "))
        };
        println!(
            "  {held}  {}  [{}]  {}{agents}",
            t.uid,
            t.status.as_deref().unwrap_or("-"),
            t.title
        );
    }
    println!();
    for e in &timeline.events {
        println!("{}", event_line(e));
    }
    more(timeline.next.as_ref());
    Ok(())
}

fn task(ops: &Ops<'_>, uid: String, after: Option<String>) -> Result<()> {
    let view = match ops.call(Request::TimelineTask { uid, after })? {
        Response::TimelineTask { task } => task,
        other => return unexpected("timeline.task", &other),
    };
    if ops.json {
        return print_json(&view);
    }
    let t = &view.timeline;
    println!(
        "{}  [{}]  {}",
        t.uid,
        t.status.as_deref().unwrap_or("-"),
        t.title
    );
    println!(
        "  strategy {} ({}), phase {}",
        t.strategy.as_deref().unwrap_or(&view.graph.strategy),
        view.graph.graph,
        view.graph.phase.as_deref().unwrap_or("-")
    );
    if let Some(c) = &t.claim {
        let since = c
            .claimed_at_ms
            .map_or_else(String::new, |at| format!(" since {}", when(at)));
        println!("  held by {}{since}", c.owner);
    }
    if t.landing.state != "none" {
        println!(
            "  {} {} -> {}{}",
            t.landing.state,
            t.landing.branch.as_deref().unwrap_or("-"),
            t.landing.onto.as_deref().unwrap_or("-"),
            t.landing
                .pr
                .map_or_else(String::new, |p| format!("  PR #{p}"))
        );
    }
    for v in &t.visited {
        println!("  took {} --{}--> {}  x{}", v.from, v.event, v.to, v.count);
    }
    println!();
    for e in &t.events {
        println!("{}", event_line(e));
    }
    more(t.next.as_ref());
    Ok(())
}

fn plan(ops: &Ops<'_>, uid: String) -> Result<()> {
    let plan = match ops.call(Request::TimelinePlan { uid })? {
        Response::TimelinePlan { plan } => plan,
        other => return unexpected("timeline.plan", &other),
    };
    if ops.json {
        return print_json(&plan);
    }
    println!("{}  {}  ({} tasks)", plan.uid, plan.title, plan.tasks);
    println!("  when                   open  in-progress  done  cancelled");
    for p in &plan.points {
        println!(
            "  {}  {:>4}  {:>11}  {:>4}  {:>9}",
            when(p.at_ms),
            p.open,
            p.in_progress,
            p.done,
            p.cancelled
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{span, when};

    #[test]
    fn times_print_as_utc_and_durations_coarsely() {
        assert_eq!(when(0), "1970-01-01 00:00:00Z");
        assert_eq!(when(1_800_000_000_000), "2027-01-15 08:00:00Z");
        assert_eq!(span(45_000), "45s");
        assert_eq!(span(31 * 60_000), "31m");
        assert_eq!(span(3_900_000), "1h05m");
        assert_eq!(span(2 * 86_400_000 + 4 * 3_600_000), "2d04h");
    }
}
