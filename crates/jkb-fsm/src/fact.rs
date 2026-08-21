//! [`Fact`]: a three-valued answer about the world outside the machine (design S1.2).
//!
//! Every fact a lifecycle guard reads — did this branch merge, is that checkout clean, does
//! the process holding this claim still exist — comes from git, the filesystem or another
//! agent, and **every one of them can be unobtainable**. Spelling that third answer as `false`
//! is the single most repeated defect in this repository's review history: `ahead_count`
//! returned `0` (which means *nothing to land*) for a branch it could not resolve;
//! `has_own_commits` answered *no* when `rev-list` exited non-zero; the land gate could not
//! tell *no must-fix findings* from *the findings namespace resolved to nothing*.
//!
//! So `Fact` has no method that collapses [`Fact::Unknown`] into a `bool`. It has
//! [`Fact::is_yes`] and [`Fact::is_no`], **both** of which mean *proven*, and `Unknown` is
//! false for both. A guard therefore has to state its polarity in the code:
//!
//! ```
//! # use jkb_fsm::Fact;
//! # let dirty = Fact::Unknown;
//! # let merged = Fact::Unknown;
//! // "the checkout is clean" — refuses when we could not look.
//! assert!(!dirty.is_no());
//! // "the branch merged" — holds when git failed.
//! assert!(!merged.is_yes());
//! ```
//!
//! Both readings above are safe, and they are safe *in opposite directions*. That choice was
//! invisible while both facts were `bool`.

/// A three-valued fact: proven true, proven false, or not established.
///
/// `Unknown` is not a failure to be handled once and discarded — it is a normal answer that
/// must survive to the guard that reads it, because only the guard knows which direction is
/// safe. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Fact {
    /// Established true.
    Yes,
    /// Established false.
    No,
    /// Not established. Could not be observed, was never asked, or the answer was ambiguous.
    #[default]
    Unknown,
}

impl Fact {
    /// Whether this is **proven true**. `Unknown` is not.
    #[must_use]
    pub fn is_yes(self) -> bool {
        matches!(self, Self::Yes)
    }

    /// Whether this is **proven false**. `Unknown` is not.
    ///
    /// Deliberately not `!is_yes()`: a guard asking "is this checkout clean?" must refuse when
    /// the checkout could not be read, and `!is_yes()` would let it through.
    #[must_use]
    pub fn is_no(self) -> bool {
        matches!(self, Self::No)
    }

    /// Whether the answer is not established either way.
    #[must_use]
    pub fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// A fact from an answer that may be missing: `None` is [`Fact::Unknown`], not `No`.
    #[must_use]
    pub fn maybe(value: Option<bool>) -> Self {
        match value {
            Some(true) => Self::Yes,
            Some(false) => Self::No,
            None => Self::Unknown,
        }
    }

    /// A fact from a fallible observation: an error is [`Fact::Unknown`], not `No`.
    ///
    /// The constructor a probe should use, so "I shelled out to git and it failed" becomes
    /// `Unknown` at the boundary rather than three call frames later, where the type has
    /// already become `bool` and the information is gone.
    pub fn observed<T, E>(value: Result<T, E>) -> Self
    where
        T: Into<Self>,
    {
        value.map_or(Self::Unknown, Into::into)
    }

    /// Kleene conjunction. One proven `No` settles it; otherwise any `Unknown` wins.
    ///
    /// Composite facts stay three-valued instead of being collapsed to `bool` at the first
    /// join, which is where the third state was habitually lost.
    #[must_use]
    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::No, _) | (_, Self::No) => Self::No,
            (Self::Yes, Self::Yes) => Self::Yes,
            _ => Self::Unknown,
        }
    }

    /// Kleene disjunction. One proven `Yes` settles it; otherwise any `Unknown` wins.
    #[must_use]
    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Yes, _) | (_, Self::Yes) => Self::Yes,
            (Self::No, Self::No) => Self::No,
            _ => Self::Unknown,
        }
    }

    /// The word for this answer, for a message a person reads.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }

    /// Every value, for exhaustive test matrices ([`crate::Machine::audit`]).
    pub const ALL: &'static [Self] = &[Self::Yes, Self::No, Self::Unknown];
}

/// Kleene negation: `Unknown` negates to `Unknown`, so `!fact` never invents an answer.
impl std::ops::Not for Fact {
    type Output = Self;

    fn not(self) -> Self {
        match self {
            Self::Yes => Self::No,
            Self::No => Self::Yes,
            Self::Unknown => Self::Unknown,
        }
    }
}

impl From<bool> for Fact {
    fn from(value: bool) -> Self {
        if value {
            Self::Yes
        } else {
            Self::No
        }
    }
}

impl From<Option<bool>> for Fact {
    fn from(value: Option<bool>) -> Self {
        Self::maybe(value)
    }
}

#[cfg(test)]
mod tests {
    use super::Fact;

    /// The property the whole type exists for: `Unknown` proves nothing in **either**
    /// direction, so a guard reading it is refused whichever polarity it asked for.
    #[test]
    fn unknown_proves_nothing_either_way() {
        assert!(!Fact::Unknown.is_yes());
        assert!(!Fact::Unknown.is_no());
        assert!(Fact::Unknown.is_unknown());
    }

    #[test]
    fn is_no_is_not_the_negation_of_is_yes() {
        // If it were, "the checkout is clean" would pass for a checkout we could not read.
        for f in Fact::ALL {
            assert_eq!(f.is_no(), !f.is_yes() && !f.is_unknown());
        }
    }

    #[test]
    fn a_failed_observation_is_unknown_not_no() {
        let failed: Result<bool, &str> = Err("git exploded");
        assert_eq!(Fact::observed(failed), Fact::Unknown);
        let worked: Result<bool, &str> = Ok(false);
        assert_eq!(Fact::observed(worked), Fact::No);
        assert_eq!(Fact::maybe(None), Fact::Unknown);
    }

    #[test]
    fn kleene_ops_keep_the_third_value() {
        assert_eq!(!Fact::Unknown, Fact::Unknown);
        // A proven `No` settles a conjunction even beside an unknown.
        assert_eq!(Fact::Unknown.and(Fact::No), Fact::No);
        assert_eq!(Fact::Unknown.and(Fact::Yes), Fact::Unknown);
        // A proven `Yes` settles a disjunction.
        assert_eq!(Fact::Unknown.or(Fact::Yes), Fact::Yes);
        assert_eq!(Fact::Unknown.or(Fact::No), Fact::Unknown);
        assert_eq!(Fact::Yes.and(Fact::Yes), Fact::Yes);
        assert_eq!(Fact::No.or(Fact::No), Fact::No);
    }

    /// Kleene logic is commutative and De Morgan's laws hold — checked exhaustively over the
    /// nine pairs, because a hand-written truth table is exactly the kind of thing that ends
    /// up one row short.
    #[test]
    fn kleene_laws_hold_over_every_pair() {
        for a in Fact::ALL {
            for b in Fact::ALL {
                assert_eq!(a.and(*b), b.and(*a), "and is commutative");
                assert_eq!(a.or(*b), b.or(*a), "or is commutative");
                assert_eq!(!a.and(*b), (!*a).or(!*b), "de morgan");
                assert_eq!(!a.or(*b), (!*a).and(!*b), "de morgan");
            }
            assert_eq!(!!*a, *a, "double negation");
        }
    }
}
