//! Text embedding implementations behind [`jkb_types::Embedder`].
//!
//! The default ([`ollama::OllamaEmbedder`]) talks to a local ollama server over
//! HTTP and needs no native build; an in-process `fastembed` (ONNX) impl lives
//! behind the `fastembed` feature (design D12). Callers select one via
//! [`EmbedderConfig`] and [`build`], depending only on the `Embedder` trait.
//!
//! ## Dimension/model catalog
//! Recording the active model into the `embeddings_meta` catalog is the job of the
//! layer that owns the database ([`jkb-index`]/[`jkb-ingest`]); this crate has no
//! `jkb-core` dependency. What lives here are the pure checks that layer calls:
//! [`ensure_compatible`] (dim **and** model must match before writing vectors — a
//! same-dim model swap silently poisons the space, so dim alone is not enough), and
//! [`check_version_drift`] (a `doctor` diagnostic comparing the recorded
//! [`Embedder::resolved_version`] against the live one, to catch a mutable tag like
//! ollama's `:latest` being re-pointed at new weights).
//!
//! [`jkb-index`]: https://docs.rs/jkb-index
//! [`jkb-ingest`]: https://docs.rs/jkb-ingest

pub mod ollama;

#[cfg(feature = "fastembed")]
pub mod fastembed;

use jkb_types::{Embedder, Result};
use serde::{Deserialize, Serialize};

// The catalog-consistency checks moved to `jkb-types` (pure functions over the
// `Embedder` trait, so `jkb-index` can use them without pulling this crate's HTTP
// stack). Re-exported here for callers that already reach for `jkb_embed::…`.
pub use jkb_types::{check_version_drift, ensure_compatible, CatalogIdentity, VersionDrift};

pub use ollama::{OllamaConfig, OllamaEmbedder};

#[cfg(feature = "fastembed")]
pub use fastembed::{FastembedConfig, FastembedEmbedder};

/// Which embedder backend to use, plus its configuration.
///
/// Serializes with a `backend` tag so a config file reads naturally, e.g.
/// `{"backend": "ollama", "model": "nomic-embed-text", ...}`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum EmbedderConfig {
    /// Local ollama HTTP server (the default).
    Ollama(OllamaConfig),
    /// In-process ONNX via `fastembed` (requires the `fastembed` build feature).
    #[cfg(feature = "fastembed")]
    Fastembed(FastembedConfig),
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self::Ollama(OllamaConfig::default())
    }
}

/// Build the configured [`Embedder`].
///
/// # Errors
/// Returns [`Error::EmbedderUnavailable`] if the backend cannot be initialized,
/// or [`Error::Validation`] for an unsupported model name.
pub fn build(config: &EmbedderConfig) -> Result<Box<dyn Embedder>> {
    match config {
        EmbedderConfig::Ollama(c) => Ok(Box::new(OllamaEmbedder::new(c.clone())?)),
        #[cfg(feature = "fastembed")]
        EmbedderConfig::Fastembed(c) => Ok(Box::new(FastembedEmbedder::new(c.clone())?)),
    }
}

/// Deterministically truncate `text` to at most `max_chars` characters.
///
/// Returns a subslice on a `char` boundary (never mid-codepoint), or the whole
/// string when it is already short enough. Embedders call this so an overlong
/// document is truncated rather than rejected (design D12).
#[must_use]
pub fn truncate_to_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &text[..byte_idx],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_compatible, truncate_to_chars, CatalogIdentity, EmbedderConfig, OllamaConfig,
        OllamaEmbedder,
    };
    use jkb_types::Error;

    fn embedder() -> OllamaEmbedder {
        OllamaEmbedder::new(OllamaConfig::default()).expect("client builds")
    }

    #[test]
    fn truncate_returns_whole_string_when_short() {
        assert_eq!(truncate_to_chars("hello", 100), "hello");
        assert_eq!(truncate_to_chars("", 100), "");
    }

    #[test]
    fn truncate_caps_at_char_count() {
        assert_eq!(truncate_to_chars("hello world", 5), "hello");
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        // "café" then padding: cap between codepoints, never mid-`é`.
        let s = "café-au-lait";
        let out = truncate_to_chars(s, 4);
        assert_eq!(out, "café");
        // multibyte-only string, truncated inside the run
        let emojis = "😀😁😂🤣";
        assert_eq!(truncate_to_chars(emojis, 2), "😀😁");
    }

    #[test]
    fn truncate_at_exact_length_keeps_all() {
        assert_eq!(truncate_to_chars("abc", 3), "abc");
    }

    #[test]
    fn compatible_when_model_and_dim_agree() {
        let e = embedder();
        let catalog = CatalogIdentity {
            model: "nomic-embed-text",
            dim: 768,
        };
        assert!(ensure_compatible(catalog, &e).is_ok());
    }

    #[test]
    fn dim_mismatch_is_refused_with_detail() {
        let e = embedder();
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
        let e = embedder();
        let catalog = CatalogIdentity {
            model: "mxbai-embed-large",
            dim: 768,
        };
        let err = ensure_compatible(catalog, &e).expect_err("models differ");
        assert!(matches!(err, Error::Validation(_)));
        assert!(err.to_string().contains("model"));
    }

    #[test]
    fn default_config_is_ollama() {
        let json = serde_json::to_string(&EmbedderConfig::default()).expect("serializes");
        assert!(json.contains("\"backend\":\"ollama\""));
        assert!(json.contains("nomic-embed-text"));
    }

    #[test]
    fn config_roundtrips_through_serde() {
        let cfg = EmbedderConfig::default();
        let json = serde_json::to_string(&cfg).expect("serializes");
        let back: EmbedderConfig = serde_json::from_str(&json).expect("deserializes");
        assert!(matches!(back, EmbedderConfig::Ollama(_)));
    }

    #[test]
    fn config_deserializes_with_defaults_filled() {
        // Only the tag provided; per-field serde defaults fill the rest.
        let cfg: EmbedderConfig =
            serde_json::from_str(r#"{"backend":"ollama"}"#).expect("deserializes");
        match cfg {
            EmbedderConfig::Ollama(c) => {
                assert_eq!(c.model, "nomic-embed-text");
                assert_eq!(c.dim, 768);
            }
            #[cfg(feature = "fastembed")]
            EmbedderConfig::Fastembed(_) => panic!("expected ollama"),
        }
    }
}
