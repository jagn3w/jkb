//! This machine's name — what a `host:pid` claim owner is qualified by, and what decides whether its
//! pid may be probed here at all.

/// This machine's name, asked of the kernel when the environment does not say.
///
/// The environment comes first so a test (and an operator) can pin it. What matters is the fallback:
/// it used to be the literal `"localhost"`, which both a host and the dev container running on it would
/// answer, so `host:pid` owner ids from either side compared EQUAL and the container's pid namespace was
/// probed as if it were this one. A rule whose two sides answer the same name is not a rule, so the last
/// resort is `uname`, which names the machine.
///
/// One copy: the CLI qualifies its owners with it and `jkb serve` refuses, with it, a client's claim
/// that one of this host's processes is gone (`task.reclaim`).
#[must_use]
pub fn name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok())
        .filter(|h| !h.is_empty())
        .or_else(nodename)
        .unwrap_or_else(|| "localhost".to_owned())
}

#[cfg(unix)]
fn nodename() -> Option<String> {
    let uts = rustix::system::uname();
    let node = uts.nodename().to_string_lossy().into_owned();
    (!node.is_empty()).then_some(node)
}

#[cfg(not(unix))]
const fn nodename() -> Option<String> {
    None
}
