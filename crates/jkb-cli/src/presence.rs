//! Whether a path is there — asked wherever an absence licenses an action.
//!
//! One rule, and the codebase already stated it twice in prose before it was ever a function:
//! `owner.rs` said **an absence is only proof where you can see the place it would be**, and
//! `archive::reap` said (citing D45.5) *an absent directory is evidence of removal only when the
//! repo it lives under is reachable*. Those are not two rules. They are one rule with two
//! different **anchors** — the containing directory, and the repo root — each hard-coded into its
//! own predicate, which is precisely why a call site had to pick one by feel and why fixing one
//! site kept breaking its neighbour:
//!
//! - Round 8 made the git questions three-valued.
//! - Round 9 found `worktree_identity` proving absence with `Path::exists()`, which answers
//!   `false` for ANY stat error, so an unreadable path became a PROVEN absence — `Fact::No`, i.e.
//!   "proven clean" — and routed it through the parent-anchored probe.
//! - Round 10's self-review found four more sites with the same weak predicate and gave them a
//!   *third* probe, anchored on nothing.
//! - Round 10's review found that the round-9 fix had imported the parent anchor into a path
//!   where it is wrong, wedging `jkb task land` for ever.
//!
//! So the anchor is an argument. A caller states the evidence that makes its answer meaningful
//! instead of choosing between predicates, and there is one place to read the rule.

use std::path::Path;

use jkb_fsm::Fact;

/// Is `path` there, judged against an `anchor` whose presence is what makes an absence mean
/// anything?
///
/// - [`Fact::Yes`] — it is there.
/// - [`Fact::No`] — it is not there **and** the anchor is, so the absence is established.
/// - [`Fact::Unknown`] — either stat failed, or the anchor is not visible either, in which case
///   nothing has been established about `path`: you may simply be looking at the wrong
///   filesystem.
///
/// **The anchor is re-stat'd here, not taken on trust**, and that is the load-bearing part. A
/// caller that forgets its own reachability precheck degrades to `Unknown` and holds — it cannot
/// degrade to a proven absence. That is the difference between a rule stated in a doc comment and
/// one the code enforces: the container-bind incident that produced this rule was exactly a
/// caller reading an absence that its own filesystem could not speak to.
///
/// **Choosing an anchor:** the nearest directory the caller has independent evidence for and that
/// no ordinary operation removes. For anything under a repo that is the **repo root**. It is
/// specifically NOT `.jkb/work`, which `git clean -xdf` removes wholesale — `.jkb/` is in
/// `.git/info/exclude`, so git treats it as junk — and anchoring the landing path there is the
/// round-10 wedge. Only an owner id, which is a bare string with no provenance at all, has
/// nothing better than its own parent to anchor on (see [`crate::owner`]).
///
/// Getting the anchor too *near* costs a visible hold; it can never manufacture a false `No`,
/// because the anchor only ever upgrades `Ok(false)` to `No`. Of the two ways to be wrong, the
/// one that costs a command wins (D34.4).
///
/// Polarity is presence, never absence: the first version of this returned "is it gone" and every
/// arm was flipped at the call site, which is one edit away from reading backwards in the probe
/// that decides whether a claim may be freed.
pub fn present_under(path: &Path, anchor: &Path) -> Fact {
    match path.try_exists() {
        Ok(true) => Fact::Yes,
        // The stat never came back. Nothing about `path` is established, whatever the anchor says.
        Err(_) => Fact::Unknown,
        Ok(false) => match anchor.try_exists() {
            // The place it would be is visible, so it really is gone.
            Ok(true) => Fact::No,
            _ => Fact::Unknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::present_under;
    use jkb_fsm::Fact;

    #[test]
    fn an_absence_is_proven_only_when_the_anchor_is_visible() {
        let t = tempfile::tempdir().expect("tempdir");
        let anchor = t.path();
        let gone = anchor.join("gone");
        assert_eq!(present_under(anchor, anchor), Fact::Yes);
        assert_eq!(
            present_under(&gone, anchor),
            Fact::No,
            "absent, and we can see the place it would be"
        );
        assert_eq!(
            present_under(&gone, &anchor.join("no-such-anchor")),
            Fact::Unknown,
            "absent, but so is the anchor — this may simply be the wrong filesystem"
        );
    }

    /// The anchor is checked HERE, so forgetting to check it elsewhere cannot produce a proven
    /// absence — which is the whole reason it is an argument rather than a precondition.
    #[test]
    fn a_caller_that_forgot_its_precheck_degrades_to_unknown_not_to_proven_gone() {
        let t = tempfile::tempdir().expect("tempdir");
        let unreachable = t.path().join("not-mounted-here");
        assert_eq!(
            present_under(&unreachable.join("sess"), &unreachable),
            Fact::Unknown
        );
    }

    /// A stat error is never an absence, whatever the anchor says.
    #[test]
    fn an_unreadable_path_is_unknown_even_beside_a_visible_anchor() {
        use std::os::unix::fs::PermissionsExt;

        let t = tempfile::tempdir().expect("tempdir");
        let locked = t.path().join("locked");
        let inside = locked.join("wt");
        std::fs::create_dir_all(&inside).expect("mkdir");

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let stat = std::fs::metadata(&inside).err().map(|e| e.kind());
        // Anchored on the tempdir root, which is plainly visible: only the stat of `inside` fails.
        let answer = present_under(&inside, t.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("restore");

        assert_eq!(
            stat,
            Some(std::io::ErrorKind::PermissionDenied),
            "the premise — the stat must actually fail, or this test is about nothing"
        );
        assert_eq!(
            answer,
            Fact::Unknown,
            "a visible anchor licenses reading an ENOENT, never a stat error"
        );
    }
}
