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
/// `max_entries` entries or `max_bytes` of answers the oldest answers go first. An entry still in
/// progress is never evicted: its waiters would lose the answer, and a resend would run it again.
pub struct Store<V> {
    entries: HashMap<Key, Entry<V>>,
    next_id: u64,
    done_bytes: usize,
    /// How long an answer is kept.
    pub ttl: Duration,
    max_entries: usize,
    max_bytes: usize,
}

impl<V: Clone> Store<V> {
    /// An empty store.
    #[must_use]
    pub fn new(ttl: Duration, max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 0,
            done_bytes: 0,
            ttl,
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
        self.evict();
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
        self.evict();
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

    /// Drop the oldest answers while over either cap. In-progress entries stay, even over the cap:
    /// they are bounded by the daemon's op permits.
    fn evict(&mut self) {
        while self.entries.len() > self.max_entries || self.done_bytes > self.max_bytes {
            let oldest = self
                .entries
                .iter()
                .filter_map(|(k, e)| match &e.state {
                    State::Done { at, bytes, .. } => Some((*at, *bytes, k)),
                    State::InProgress(_) => None,
                })
                .min_by_key(|(at, _, _)| *at)
                .map(|(_, bytes, k)| (bytes, k.clone()));
            let Some((bytes, key)) = oldest else {
                return;
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

    #[test]
    fn an_answer_is_replayed_until_its_ttl_then_runs_again() {
        let t0 = Instant::now();
        let mut s = Store::new(Duration::from_mins(10), 16, 1 << 20);
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

    #[test]
    fn the_caps_evict_the_oldest_answers_and_never_one_in_progress() {
        let t0 = Instant::now();
        let mut s = Store::new(Duration::from_mins(10), 3, 1 << 20);
        let running = run(&mut s, "running", t0);
        for (k, secs) in [("a", 1), ("b", 2)] {
            let id = run(&mut s, k, t0);
            s.finish(&key(k), id, 1, 1, t0 + Duration::from_secs(secs));
        }
        // A fourth entry: "a", the oldest answer, goes — not "running", older still.
        let id = run(&mut s, "c", t0 + Duration::from_secs(10));
        assert_eq!(s.len(), 3);
        // Asking for "a" again starts it afresh, a fourth entry again: now "b" is the only answer,
        // so it goes.
        assert!(matches!(s.begin(key("a"), 0, t0), Begin::Run(_)), "a went");
        assert!(matches!(s.begin(key("running"), 0, t0), Begin::Wait(_)));
        assert!(matches!(s.begin(key("b"), 0, t0), Begin::Run(_)), "b went");
        assert_eq!(
            s.len(),
            4,
            "over the cap with nothing but entries in progress"
        );
        s.finish(&key("running"), running, 2, 1, t0);
        s.finish(&key("c"), id, 3, 1, t0 + Duration::from_secs(11));
        assert_eq!(s.len(), 3, "a finished answer is evicted down to the cap");

        let mut s = Store::new(Duration::from_mins(10), 16, 10);
        let a = run(&mut s, "a", t0);
        s.finish(&key("a"), a, 1, 6, t0);
        let b = run(&mut s, "b", t0);
        s.finish(&key("b"), b, 2, 6, t0 + Duration::from_secs(1));
        assert!(
            matches!(s.begin(key("b"), 0, t0), Begin::Replay(2)),
            "the newer answer stays"
        );
        assert!(
            matches!(s.begin(key("a"), 0, t0), Begin::Run(_)),
            "the byte cap evicted the older"
        );
    }

    #[test]
    fn an_abandoned_entry_runs_again_and_its_waiters_hear_no_answer() {
        let t0 = Instant::now();
        let mut s = Store::<u32>::new(Duration::from_mins(10), 16, 1 << 20);
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
