//! In-process [`Embedder`] via `fastembed` (ONNX runtime).
//!
//! Feature-gated (`--features fastembed`) and off by default: it is a heavy native
//! build (`ort`/ONNX) plus a first-run model download — the most likely first-run
//! failure mode, so the HTTP ollama impl is the default (design D12). Enabling this
//! feature buys embedding with no external server.

use std::sync::Mutex;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use jkb_types::{Embedder, Error, Result};
use serde::{Deserialize, Serialize};

use crate::truncate_to_chars;

/// Default fastembed model — matches the ollama default's family and dim (768).
pub const DEFAULT_MODEL: &str = "nomic-embed-text-v1.5";

/// `nomic-embed-text-v1.5` produces 768-dimensional vectors.
pub const DEFAULT_DIM: usize = 768;

/// Upper bound on input chars before truncation (see [`crate::ollama::MAX_INPUT_CHARS`]).
pub const MAX_INPUT_CHARS: usize = 8192 * 4;

/// Configuration for the fastembed embedder.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FastembedConfig {
    /// Model identifier (see [`resolve_model`] for supported names).
    #[serde(default = "default_model")]
    pub model: String,
    /// Expected vector dimensionality for `model`.
    #[serde(default = "default_dim")]
    pub dim: usize,
}

fn default_model() -> String {
    DEFAULT_MODEL.to_owned()
}
fn default_dim() -> usize {
    DEFAULT_DIM
}

impl Default for FastembedConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            dim: default_dim(),
        }
    }
}

/// Map a config model name to a `fastembed` model.
///
/// # Errors
/// Returns [`Error::Validation`] for an unsupported name.
fn resolve_model(name: &str) -> Result<EmbeddingModel> {
    match name {
        "nomic-embed-text-v1.5" | "nomic-embed-text" => Ok(EmbeddingModel::NomicEmbedTextV15),
        other => Err(Error::Validation(format!(
            "unsupported fastembed model `{other}` (known: nomic-embed-text-v1.5)"
        ))),
    }
}

/// An [`Embedder`] that runs the model in-process via ONNX.
///
/// `fastembed`'s `embed` takes `&mut self`, but [`Embedder::embed`] is `&self`, so
/// the model sits behind a [`Mutex`] (which also makes the embedder `Send + Sync`).
pub struct FastembedEmbedder {
    model_name: String,
    dim: usize,
    inner: Mutex<TextEmbedding>,
}

impl FastembedEmbedder {
    /// Build the embedder, downloading the model on first use.
    ///
    /// # Errors
    /// Returns [`Error::Validation`] for an unknown model name, or
    /// [`Error::EmbedderUnavailable`] if the model fails to initialize/download.
    pub fn new(config: FastembedConfig) -> Result<Self> {
        let model = resolve_model(&config.model)?;
        let inner =
            TextEmbedding::try_new(InitOptions::new(model).with_show_download_progress(false))
                .map_err(|e| {
                    Error::EmbedderUnavailable(format!(
                        "could not initialize fastembed model `{}`: {e}",
                        config.model
                    ))
                })?;
        Ok(Self {
            model_name: config.model,
            dim: config.dim,
            inner: Mutex::new(inner),
        })
    }
}

impl Embedder for FastembedEmbedder {
    fn model(&self) -> &str {
        &self.model_name
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let prompt = truncate_to_chars(text, MAX_INPUT_CHARS);
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| Error::EmbedderUnavailable("fastembed model lock poisoned".to_owned()))?;
        let mut out = guard
            .embed(vec![prompt], None)
            .map_err(|e| Error::EmbedderUnavailable(format!("fastembed embed failed: {e}")))?;
        let vector = out.pop().ok_or_else(|| {
            Error::EmbedderUnavailable("fastembed returned no embedding".to_owned())
        })?;
        if vector.len() != self.dim {
            return Err(Error::Validation(format!(
                "fastembed model `{}` returned dim {} but {} was configured",
                self.model_name,
                vector.len(),
                self.dim
            )));
        }
        Ok(vector)
    }

    fn health_check(&self) -> Result<()> {
        // The model is loaded in-process at construction, so a successful build is
        // the health check. A tiny embed confirms it can actually run.
        self.embed("health check").map(|_| ())
    }

    fn resolved_version(&self) -> Result<Option<String>> {
        // fastembed models are content-pinned releases (the name carries the
        // version, e.g. `-v1.5`), so the name is a stable identity — no drift like
        // ollama's mutable tags, and no I/O needed. Prefixed to distinguish it from
        // an ollama model of the same name.
        Ok(Some(format!("fastembed:{}", self.model_name)))
    }
}
