//! The `tasks` contract: a namespace that holds tasks and nothing else (design D33.3).
//!
//! Applied automatically to the reserved `tasks` root (see
//! [`crate::nstype::RESERVED_TYPES`]) and inherited by its whole subtree, so
//! `tasks/<repo>/inbox`, `tasks/<repo>/.backlog` and every mirror namespace carry it too.
//!
//! This type says only *what may live there*. It deliberately does **not** locate the tasks
//! root: `tasks/` is the tasks root because the D32 layout reserves it, not because it
//! carries this contract. Types are not location markers — see the note on
//! [`crate::nstype::RESERVED_TYPES`].
//!
//! It deliberately does **not** restate the status lifecycle. `TaskStatus::from_manual_str`
//! guards the string boundary and the `V006` CHECK constraint guards the column; a third
//! copy here would be a third place for the three to disagree.

use crate::nstype::{NamespaceType, NodeKindSpec, TypeRole};

/// The name stored in `namespaces.metadata.type`.
pub const NAME: &str = "tasks";

/// The one item kind a `tasks` namespace accepts.
pub const KIND_TASK: &str = "task";

const KINDS: &[NodeKindSpec] = &[NodeKindSpec {
    kind: KIND_TASK,
    base: crate::nstype::BaseKind::Node,
    about: "a unit of work with a status lifecycle and a `depends_on` DAG",
}];

/// The `tasks` contract type.
pub struct Tasks;

impl NamespaceType for Tasks {
    fn name(&self) -> &'static str {
        NAME
    }

    fn about(&self) -> &'static str {
        "holds tasks only"
    }

    fn role(&self) -> TypeRole {
        TypeRole::Contract
    }

    fn node_kinds(&self) -> &'static [NodeKindSpec] {
        KINDS
    }

    /// No base kinds: a tasks namespace accepts `task` and *only* `task`. Inheriting the
    /// four investigation base kinds would let a `goal` or a `reflection` be filed here,
    /// which is exactly the hole this contract closes.
    fn base_kinds(&self) -> &'static [&'static str] {
        &[]
    }
}
