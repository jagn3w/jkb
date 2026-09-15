//! Rendering helpers: fetch item rows for display and print them as human-readable
//! lines or `--json`.

use anyhow::Result;
use jkb_core::Db;
use jkb_types::ItemId;
use serde_json::{json, Value};

/// A denormalized item row for listing output — the read set's row (`jkb_api::kb`), so a listing
/// printed here and one the daemon served are the same row.
pub use jkb_api::kb::ItemRow as DisplayItem;

fn item_json(item: &DisplayItem) -> Value {
    json!({
        "id": item.id,
        "uid": item.uid,
        "kind": item.kind,
        "status": item.status,
        "resolution": item.resolution,
        "priority": item.priority,
        "due": item.due,
        "namespace": item.namespace,
        "snippet": item.snippet,
        "updated": item.updated,
    })
}

fn item_line(item: &DisplayItem) -> String {
    let mut parts = vec![format!("{:<24} [{}]", item.uid, item.kind)];
    if let Some(s) = &item.status {
        parts.push(format!("({s})"));
    }
    if let Some(r) = &item.resolution {
        parts.push(format!("<{r}>"));
    }
    if let Some(p) = item.priority {
        parts.push(format!("!p{p}"));
    }
    if let Some(d) = &item.due {
        parts.push(format!("@{d}"));
    }
    if let Some(ns) = &item.namespace {
        parts.push(format!("<{ns}>"));
    }
    if let Some(snip) = &item.snippet {
        if !snip.is_empty() {
            parts.push(format!("— {snip}"));
        }
    }
    parts.join(" ")
}

pub(crate) use jkb_core::item::{first_nonblank, title_of};

/// Fetch display rows for `ids`, preserving their order and skipping any that no
/// longer exist.
///
/// # Errors
/// Returns an error if a read fails.
pub fn fetch_items(db: &Db, ids: &[ItemId]) -> Result<Vec<DisplayItem>> {
    let ids = ids.to_vec();
    // The host's own listing: unbounded, like the CLI it serves (only the daemon budgets a read).
    Ok(db.read(move |conn| {
        jkb_api::kb::item_rows(conn, &ids, &mut jkb_api::kb::Budget::UNLIMITED.clone())
    })?)
}

/// Print items as JSON or human lines.
pub fn print_items(items: &[DisplayItem], as_json: bool) {
    if as_json {
        let arr: Vec<Value> = items.iter().map(item_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Array(arr)).unwrap_or_default()
        );
    } else if items.is_empty() {
        println!("(no results)");
    } else {
        for item in items {
            println!("{}", item_line(item));
        }
    }
}
