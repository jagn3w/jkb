//! The `views` contract: a namespace that holds saved views and nothing else
//! (design D33.3).
//!
//! Applied automatically to `_sys/views` (see [`crate::nstype::RESERVED_TYPES`]), the
//! namespace `crate::view::save` files every saved query under.

use crate::nstype::{NamespaceType, NodeKindSpec, TypeRole};

/// The name stored in `namespaces.metadata.type`.
pub const NAME: &str = "views";

/// The one item kind a `views` namespace accepts.
pub const KIND_VIEW: &str = "view";

const KINDS: &[NodeKindSpec] = &[NodeKindSpec {
    kind: KIND_VIEW,
    base: crate::nstype::BaseKind::Node,
    about: "a saved query: `uid = view:<name>`, `content` = the DSL string",
}];

/// The `views` contract type.
pub struct Views;

impl NamespaceType for Views {
    fn name(&self) -> &'static str {
        NAME
    }

    fn about(&self) -> &'static str {
        "holds saved views only (`kind=view`, content = a query DSL string)"
    }

    fn role(&self) -> TypeRole {
        TypeRole::Contract
    }

    fn node_kinds(&self) -> &'static [NodeKindSpec] {
        KINDS
    }

    fn base_kinds(&self) -> &'static [&'static str] {
        &[]
    }
}
