//! What `jkb serve` remembers of the requests that carried an `Idempotency-Key`, so a client may
//! resend a request that went unanswered — a write included — without it being applied twice.
//!
//! **Why.** Claude Code's sandbox proxy stalls about 1 in 200 requests to `jkb serve` for 10–30 s:
//! the request connects and no answer comes back. The client cannot tell a request the daemon never
//! saw from one whose answer was lost, so before this it could only wait out its whole deadline.
//! Now each logical call carries one random key on every attempt ([`crate::client::RemoteBackend`]),
//! and the daemon answers a repeat of a key from here rather than running the op again:
//!
//! - **Done**: the answer recorded — the bytes that went back, an error the op returned included — is
//!   sent again. A refusal before the op ran (authentication, a malformed body, a busy budget) is not
//!   recorded, so a resend of one is tried afresh.
//! - **In progress**: the repeat waits for the original to finish, and gets its answer.
//!
//! Keys are scoped by **who asked** — the hash of the bearer token the request presented — so one
//! caller's key never returns another caller's answer. A key reused for a different request body is
//! refused rather than answered with the first body's result.
//!
//! **Residual.** This is memory only: a daemon restart forgets every key, so a request whose first
//! attempt was applied just before a restart and whose resend arrives after it is applied twice. A
//! key also lives only [`Store::ttl`] past its answer.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// Which recorded request: the caller's scope (a hash of its bearer token) and its key.
pub type Key = (String, String);

/// The repeats-of-a-request a [`Store`] answers.
pub enum Begin<V> {
    /// Nothing recorded: run the op, then [`Store::finish`] or [`Store::abandon`] with this id.
    Run(Running<V>),
    /// Answered already: send this again.
    Replay(V),
    /// Running now: wait on this for its answer. It closes without one if the original was refused
    /// before it ran.
    Wait(watch::Receiver<Option<V>>),
    /// The key was first used for a different request.
    Mismatch,
    /// Nothing recorded, and no room to record it: the store is at a cap with every answer in it too
    /// young to evict. The request must be refused (`busy`) without running.
    Full,
}

/// The original of a key, while it runs.
pub struct Running<V> {
    /// Which entry this is, so a finish never lands on a newer entry under the same key.
    pub id: u64,
    /// Where its waiters hear the answer.
    pub tx: watch::Sender<Option<V>>,
}

enum State<V> {
    InProgress(watch::Receiver<Option<V>>),
    Done { value: V, bytes: usize, at: Instant },
}

struct Entry<V> {
    id: u64,
    fingerprint: u64,
    state: State<V>,
}

/// The recorded requests. Answers expire [`Store::ttl`] after they were recorded, and past
/// `max_entries` entries or `max_bytes` of answers the oldest answers go first — but never one younger
/// than `min_age`, the longest a client may still resend it (its whole deadline, with a margin):
/// evicted then, the resend would run the write again. While nothing can be evicted the store is full,
/// and new keyed work is refused ([`Begin::Full`]) rather than recorded at the cost of a young answer.
/// An entry still in progress is never evicted: its waiters would lose the answer, and a resend would
/// run it again.
pub struct Store<V> {
    entries: HashMap<Key, Entry<V>>,
    next_id: u64,
    done_bytes: usize,
    /// How long an answer is kept.
    pub ttl: Duration,
    min_age: Duration,
    max_entries: usize,
    max_bytes: usize,
}

impl<V: Clone> Store<V> {
    /// An empty store.
    #[must_use]
    pub fn new(ttl: Duration, min_age: Duration, max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 0,
            done_bytes: 0,
            ttl,
            min_age,
            max_entries,
            max_bytes,
        }
    }

    /// What to do with a request for `key` whose body hashes to `fingerprint`.
    pub fn begin(&mut self, key: Key, fingerprint: u64, now: Instant) -> Begin<V> {
        self.expire(now);
        if let Some(entry) = self.entries.get(&key) {
            if entry.fingerprint != fingerprint {
                return Begin::Mismatch;
            }
            return match &entry.state {
                State::Done { value, .. } => Begin::Replay(value.clone()),
                State::InProgress(rx) => Begin::Wait(rx.clone()),
            };
        }
        if !self.evict(now, 1) {
            return Begin::Full;
        }
        let id = self.next_id;
        self.next_id += 1;
        let (tx, rx) = watch::channel(None);
        self.entries.insert(
            key,
            Entry {
                id,
                fingerprint,
                state: State::InProgress(rx),
            },
        );
        Begin::Run(Running { id, tx })
    }

    /// Record the answer of the entry `id` under `key`, `bytes` long.
    pub fn finish(&mut self, key: &Key, id: u64, value: V, bytes: usize, now: Instant) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        if entry.id != id || !matches!(entry.state, State::InProgress(_)) {
            return;
        }
        entry.state = State::Done {
            value,
            bytes,
            at: now,
        };
        self.done_bytes += bytes;
        // Kept even past the byte cap when nothing older may go: the op ran, and its answer is what a
        // resend must get. New work is refused until there is room again.
        self.evict(now, 0);
    }

    /// Forget the entry `id` under `key` without an answer: it was refused before it ran, so a resend
    /// should run it.
    pub fn abandon(&mut self, key: &Key, id: u64) {
        if self
            .entries
            .get(key)
            .is_some_and(|e| e.id == id && matches!(e.state, State::InProgress(_)))
        {
            self.entries.remove(key);
        }
    }

    /// Whether anything is recorded under `key`, running or answered. An answer past its time may be
    /// counted until the next [`Store::begin`] expires it; that one then runs.
    #[must_use]
    pub fn contains(&self, key: &Key) -> bool {
        self.entries.contains_key(key)
    }

    /// How many entries are running.
    #[must_use]
    pub fn in_progress(&self) -> usize {
        self.entries
            .values()
            .filter(|e| matches!(e.state, State::InProgress(_)))
            .count()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn expire(&mut self, now: Instant) {
        let ttl = self.ttl;
        let mut freed = 0;
        self.entries.retain(|_, e| match &e.state {
            State::Done { at, bytes, .. } if now.saturating_duration_since(*at) >= ttl => {
                freed += bytes;
                false
            }
            _ => true,
        });
        self.done_bytes -= freed;
    }

    /// Drop the oldest answers at least `min_age` old while the store, with room for `extra` more
    /// entries, is over either cap. Whether it is then within them. In-progress entries and young
    /// answers stay.
    fn evict(&mut self, now: Instant, extra: usize) -> bool {
        let min_age = self.min_age;
        loop {
            if self.entries.len() + extra <= self.max_entries && self.done_bytes <= self.max_bytes {
                return true;
            }
            let oldest = self
                .entries
                .iter()
                .filter_map(|(k, e)| match &e.state {
                    State::Done { at, bytes, .. }
                        if now.saturating_duration_since(*at) >= min_age =>
                    {
                        Some((*at, *bytes, k))
                    }
                    _ => None,
                })
                .min_by_key(|(at, _, _)| *at)
                .map(|(_, bytes, k)| (bytes, k.clone()));
            let Some((bytes, key)) = oldest else {
                return false;
            };
            self.entries.remove(&key);
            self.done_bytes -= bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Begin, Key, Store};
    use std::time::{Duration, Instant};

    fn key(k: &str) -> Key {
        ("caller".to_owned(), k.to_owned())
    }

    fn run(store: &mut Store<u32>, k: &str, now: Instant) -> u64 {
        match store.begin(key(k), 0, now) {
            Begin::Run(r) => r.id,
            _ => panic!("{k}: expected to run"),
        }
    }

    const MIN_AGE: Duration = Duration::from_mins(1);

    #[test]
    fn an_answer_is_replayed_until_its_ttl_then_runs_again() {
        let t0 = Instant::now();
        let mut s = Store::new(Duration::from_mins(10), MIN_AGE, 16, 1 << 20);
        let id = run(&mut s, "a", t0);
        assert!(matches!(s.begin(key("a"), 0, t0), Begin::Wait(_)));
        s.finish(&key("a"), id, 7, 1, t0);
        let at = t0 + Duration::from_secs(599);
        assert!(matches!(s.begin(key("a"), 0, at), Begin::Replay(7)));
        assert!(matches!(s.begin(key("a"), 1, at), Begin::Mismatch));
        let later = t0 + Duration::from_mins(10);
        assert!(
            matches!(s.begin(key("a"), 0, later), Begin::Run(_)),
            "expired"
        );
    }

    /// Past a cap the oldest answer old enough to go goes first; one in progress never does.
    #[test]
    fn the_caps_evict_the_oldest_answers_and_never_one_in_progress() {
        let t0 = Instant::now();
        let late = t0 + MIN_AGE * 2;
        let mut s = Store::new(Duration::from_mins(10), MIN_AGE, 3, 1 << 20);
        let running = run(&mut s, "running", t0);
        for (k, secs) in [("a", 1), ("b", 2)] {
            let id = run(&mut s, k, t0);
            s.finish(&key(k), id, 1, 1, t0 + Duration::from_secs(secs));
        }
        // A fourth entry: "a", the oldest answer, goes — not "running", older still.
        let c = run(&mut s, "c", late);
        assert_eq!(s.len(), 3);
        assert!(
            matches!(s.begin(key("b"), 0, late), Begin::Replay(1)),
            "b stays"
        );
        // "b" is the only answer left, so it is the one to go for the next.
        assert!(
            matches!(s.begin(key("a"), 0, late), Begin::Run(_)),
            "a went"
        );
        assert!(matches!(s.begin(key("running"), 0, late), Begin::Wait(_)));
        assert!(
            matches!(s.begin(key("d"), 0, late), Begin::Full),
            "nothing but entries in progress: full"
        );
        s.finish(&key("running"), running, 2, 1, t0);
        s.finish(&key("c"), c, 3, 1, late);
        assert_eq!(s.len(), 3);

        let mut s = Store::new(Duration::from_mins(10), MIN_AGE, 16, 10);
        let a = run(&mut s, "a", t0);
        s.finish(&key("a"), a, 1, 6, t0);
        let b = run(&mut s, "b", late);
        s.finish(&key("b"), b, 2, 6, late);
        assert!(
            matches!(s.begin(key("b"), 0, late), Begin::Replay(2)),
            "the newer answer stays"
        );
        assert!(
            matches!(s.begin(key("a"), 0, late), Begin::Run(_)),
            "the byte cap evicted the older"
        );
    }

    /// A burst of answers cannot evict one younger than the client's whole deadline: the store fills,
    /// and new keyed work is refused instead, until the young answer is old enough to go.
    #[test]
    fn a_young_answer_is_never_evicted_and_new_work_is_refused_instead() {
        let t0 = Instant::now();
        let mut s = Store::new(Duration::from_mins(10), MIN_AGE, 16, 100);
        let write = run(&mut s, "write", t0);
        s.finish(&key("write"), write, 1, 10, t0);
        let soon = t0 + Duration::from_secs(1);
        let big = run(&mut s, "big", soon);
        s.finish(&key("big"), big, 2, 95, soon);
        assert!(
            matches!(s.begin(key("write"), 0, soon), Begin::Replay(1)),
            "kept past the byte cap"
        );
        assert!(matches!(s.begin(key("next"), 0, soon), Begin::Full));
        assert!(
            matches!(s.begin(key("write"), 0, soon), Begin::Replay(1)),
            "still kept"
        );
        let later = t0 + MIN_AGE;
        assert!(
            matches!(s.begin(key("next"), 0, later), Begin::Run(_)),
            "the write's answer is old enough to go now"
        );
        assert!(matches!(s.begin(key("write"), 0, later), Begin::Run(_)));
    }

    #[test]
    fn an_abandoned_entry_runs_again_and_its_waiters_hear_no_answer() {
        let t0 = Instant::now();
        let mut s = Store::<u32>::new(Duration::from_mins(10), MIN_AGE, 16, 1 << 20);
        let Begin::Run(r) = s.begin(key("a"), 0, t0) else {
            panic!("run")
        };
        let Begin::Wait(rx) = s.begin(key("a"), 0, t0) else {
            panic!("wait")
        };
        s.abandon(&key("a"), r.id);
        drop(r.tx);
        assert!(rx.has_changed().is_err(), "closed without an answer");
        assert!(matches!(s.begin(key("a"), 0, t0), Begin::Run(_)));
    }
}
