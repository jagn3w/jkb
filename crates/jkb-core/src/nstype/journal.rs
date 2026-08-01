//! The `journal` contract: a marker namespace over a system table, holding **no items**
//! (design D33.3).
//!
//! `_sys/transactions`, `_sys/ingestions` and `_sys/sync` exist to surface a system table
//! in the virtual filesystem. Their contents are owned by a migration and by the repo that
//! writes the table; nothing may file an item into them.
//!
//! One type covers all three rather than minting a `sync` type and two identical siblings:
//! that is one contract, so it is stated once. "Accepts nothing" is a real, enforceable
//! guarantee — [`crate::nstype::check_placement`] rejects every placement into these
//! namespaces, whichever writer attempts it.

use crate::nstype::{NamespaceType, NodeKindSpec, TypeRole};

/// The name stored in `namespaces.metadata.type`.
pub const NAME: &str = "journal";

/// The `journal` contract type.
pub struct Journal;

impl NamespaceType for Journal {
    fn name(&self) -> &'static str {
        NAME
    }

    fn about(&self) -> &'static str {
        "surfaces a system table in the VFS; holds no items"
    }

    fn role(&self) -> TypeRole {
        TypeRole::Contract
    }

    /// No kinds and no base kinds — the whole point.
    fn node_kinds(&self) -> &'static [NodeKindSpec] {
        &[]
    }

    fn base_kinds(&self) -> &'static [&'static str] {
        &[]
    }
}
