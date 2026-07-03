//! Embedding-catalog consistency checks.
//!
//! These are pure functions over the [`Embedder`](crate::Embedder) trait, so they
//! live in the vocabulary crate: both `jkb-embed` (which produces embedders) and
//! `jkb-index` (which writes vectors and owns the `embeddings_meta` catalog) use
//! them without either depending on the other, and without pulling any HTTP/ONNX
//! implementation.

use crate::{Embedder, Error, Result};

/// The identity of a populated `vec_items_<dim>` table, read back from the
/// `embeddings_meta` catalog, that an embedder must be compatible with.
#[derive(Debug, Clone, Copy)]
pub struct CatalogIdentity<'a> {
    /// The model name recorded when the table was populated.
    pub model: &'a str,
    /// The vector width of the target `vec_items_<dim>` table.
    pub dim: usize,
}

/// Verify the active `embedder` is compatible with what a populated vec table was
/// built from. Two invariants, both load-bearing:
///
/// - **dim** must match, or writes corrupt the fixed-width `float[dim]` vec table;
/// - **model** must match, because vectors from different models occupy
///   incomparable spaces *even at equal dim* — and 768 is near-universal, so a
///   dim-only check would wave through a space-poisoning swap.
///
/// Cheap and I/O-free; call before writing vectors. v1 supports one model/dim
/// (design D9); switching models means rebuilding the index.
///
/// # Errors
/// Returns [`Error::Validation`] describing the first mismatch (dim, then model).
pub fn ensure_compatible(catalog: CatalogIdentity, embedder: &dyn Embedder) -> Result<()> {
    if catalog.dim != embedder.dim() {
        return Err(Error::Validation(format!(
            "embedding dim mismatch: the target vec table is float[{}] but the active \
             embedder `{}` produces {}. A new dim needs a new vec_items_<dim> table \
             (migrations are additive; see design D9/D13)",
            catalog.dim,
            embedder.model(),
            embedder.dim()
        )));
    }
    if catalog.model != embedder.model() {
        return Err(Error::Validation(format!(
            "embedding model mismatch: this vec table was populated by `{}` but the active \
             embedder is `{}`. Vectors from different models are not comparable even at the \
             same dim; rebuild the index to switch models (v1 supports one model, design D9)",
            catalog.model,
            embedder.model()
        )));
    }
    Ok(())
}

/// Outcome of comparing the catalog's recorded model version against the live one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionDrift {
    /// Recorded and live versions agree.
    Match,
    /// The backend re-pointed the model under a stable name (e.g. ollama's
    /// `:latest` now resolves to a different digest). Stored vectors may be
    /// incomparable to newly-embedded ones.
    Drifted {
        /// The version recorded in `embeddings_meta` when the table was populated.
        stored: String,
        /// The version the backend resolves the configured model to right now.
        live: String,
    },
    /// One side has no version handle, so drift cannot be determined.
    Unknown,
}

/// Compare the model version recorded in the catalog against the embedder's live
/// [`Embedder::resolved_version`]. A diagnostic for `doctor`, **not** a hot-path
/// guard — it performs I/O for backends like ollama.
///
/// # Errors
/// Propagates [`Error::EmbedderUnavailable`] if the backend must be queried and is
/// unreachable.
pub fn check_version_drift(stored: Option<&str>, embedder: &dyn Embedder) -> Result<VersionDrift> {
    let live = embedder.resolved_version()?;
    Ok(match (stored, live.as_deref()) {
        (Some(s), Some(l)) if s == l => VersionDrift::Match,
        (Some(s), Some(l)) => VersionDrift::Drifted {
            stored: s.to_owned(),
            live: l.to_owned(),
        },
        _ => VersionDrift::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::{check_version_drift, ensure_compatible, CatalogIdentity, VersionDrift};
    use crate::{Embedder, Error, Result};

    /// A no-I/O embedder for exercising the pure catalog checks.
    struct FakeEmbedder {
        model: &'static str,
        dim: usize,
        version: Option<String>,
    }
    impl Embedder for FakeEmbedder {
        fn model(&self) -> &str {
            self.model
        }
        fn dim(&self) -> usize {
            self.dim
        }
        fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![0.0; self.dim])
        }
        fn health_check(&self) -> Result<()> {
            Ok(())
        }
        fn resolved_version(&self) -> Result<Option<String>> {
            Ok(self.version.clone())
        }
    }

    fn fake(model: &'static str, dim: usize) -> FakeEmbedder {
        FakeEmbedder {
            model,
            dim,
            version: None,
        }
    }

    #[test]
    fn compatible_when_model_and_dim_agree() {
        let e = fake("nomic-embed-text", 768);
        let catalog = CatalogIdentity {
            model: "nomic-embed-text",
            dim: 768,
        };
        assert!(ensure_compatible(catalog, &e).is_ok());
    }

    #[test]
    fn dim_mismatch_is_refused_with_detail() {
        let e = fake("nomic-embed-text", 768);
        let catalog = CatalogIdentity {
            model: "nomic-embed-text",
            dim: 384,
        };
        let err = ensure_compatible(catalog, &e).expect_err("dims differ");
        assert!(matches!(err, Error::Validation(_)));
        let msg = err.to_string();
        assert!(msg.contains("dim"));
        assert!(msg.contains("384"));
    }

    #[test]
    fn model_mismatch_is_refused_even_at_same_dim() {
        // Same 768 dim, different model — the space-poisoning case dim can't catch.
        let e = fake("nomic-embed-text", 768);
        let catalog = CatalogIdentity {
            model: "mxbai-embed-large",
            dim: 768,
        };
        let err = ensure_compatible(catalog, &e).expect_err("models differ");
        assert!(matches!(err, Error::Validation(_)));
        assert!(err.to_string().contains("model"));
    }

    #[test]
    fn version_drift_matches_when_equal() {
        let e = FakeEmbedder {
            version: Some("sha256:abc".to_owned()),
            ..fake("fake", 768)
        };
        assert_eq!(
            check_version_drift(Some("sha256:abc"), &e).expect("no i/o"),
            VersionDrift::Match
        );
    }

    #[test]
    fn version_drift_detects_repointed_tag() {
        let e = FakeEmbedder {
            version: Some("sha256:new".to_owned()),
            ..fake("fake", 768)
        };
        let drift = check_version_drift(Some("sha256:old"), &e).expect("no i/o");
        assert_eq!(
            drift,
            VersionDrift::Drifted {
                stored: "sha256:old".to_owned(),
                live: "sha256:new".to_owned(),
            }
        );
    }

    #[test]
    fn version_drift_unknown_when_no_handle() {
        let e = fake("fake", 768);
        assert_eq!(
            check_version_drift(Some("sha256:old"), &e).expect("no i/o"),
            VersionDrift::Unknown
        );
    }
}
