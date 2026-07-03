//! Strongly-typed identifiers.
//!
//! Each id is a distinct newtype around `i64` (the `SQLite` rowid). Because they are
//! different types, the compiler will reject passing a `NamespaceId` where an
//! `ItemId` is expected — the "can't be crossed" guarantee is enforced at compile
//! time, not by discipline.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Define an `i64`-backed identifier newtype with the usual deriving, a `new`/`get`
/// pair, and a `Display` that prints the raw number.
macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        // `transparent` (memory + serde): the newtype has the exact layout of an
        // `i64` and serializes as a bare integer, not `{"0": 42}`.
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name(i64);

        impl $name {
            /// Wrap a raw database row id.
            #[must_use]
            pub const fn new(value: i64) -> Self {
                Self(value)
            }

            /// The underlying row id.
            #[must_use]
            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_newtype!(
    /// Row id of an item — the atomic unit of knowledge and node in the graph.
    ItemId
);
id_newtype!(
    /// Row id of a namespace — a logical folder in the virtual filesystem.
    NamespaceId
);
id_newtype!(
    /// Row id of an edge — a typed directed link between two items.
    EdgeId
);

/// A stable, human-meaningful string identity for an item (e.g. `book:sicp`,
/// `b3:<hash>:<index>`, `task:reread-sicp-ch1`).
///
/// Distinct from [`ItemId`]: the `Uid` survives moves and re-placements, whereas
/// `ItemId` is the numeric rowid used for joins and indexing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Uid(String);

impl Uid {
    /// Create a `Uid` from anything string-like.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Uid {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Uid {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{ItemId, Uid};
    use proptest::prelude::*;

    #[test]
    fn display_matches_inner() {
        assert_eq!(ItemId::new(42).to_string(), "42");
        assert_eq!(Uid::new("book:sicp").to_string(), "book:sicp");
    }

    proptest! {
        /// An `ItemId` serializes as a bare integer and round-trips for any `i64`.
        #[test]
        fn item_id_json_roundtrip(v in any::<i64>()) {
            let id = ItemId::new(v);
            let json = serde_json::to_string(&id).unwrap();
            prop_assert_eq!(&json, &v.to_string());
            let back: ItemId = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(id, back);
        }
    }
}
