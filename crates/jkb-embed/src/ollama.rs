//! The default [`Embedder`]: a local ollama server over HTTP.
//!
//! ollama listens on `localhost:11434` and is likely already running on the
//! author's machine; this impl needs no native build (design D12). Calls are
//! synchronous (`reqwest::blocking`) because the ingest path that drives them is
//! synchronous — async lives only at the process edges (design D8).

use std::time::Duration;

use jkb_types::{Embedder, Error, Result};
use serde::{Deserialize, Serialize};

use crate::truncate_to_chars;

/// Default ollama endpoint (where ollama listens out of the box).
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Default embedding model: no native build, pulled via `ollama pull nomic-embed-text`.
pub const DEFAULT_MODEL: &str = "nomic-embed-text";

/// `nomic-embed-text` produces 768-dimensional vectors.
pub const DEFAULT_DIM: usize = 768;

/// Upper bound on input length before we truncate.
///
/// The 8192 in `nomic-embed-text`'s model card is its *architectural* max sequence
/// length, not what ollama runs it at: ollama loads the model with its default
/// `num_ctx` of 2048 tokens and returns a 500 (`the input length exceeds the context
/// length`) past that. Passing `options.num_ctx` does not raise it for the embeddings
/// endpoint.
///
/// Chars are a poor proxy for tokens — density swings the ratio several-fold — so this
/// is sized for the *worst* case rather than the typical one. Measured against a live
/// ollama: space-separated prose fails between 10k and 11k chars, but dense text with no
/// spaces fails between 4k and 8k. At ~2 chars/token worst case, 2048 tokens is ~4k chars.
///
/// This is a backstop, not the main defence: an over-long *document* should never reach
/// the embedder at all (`jkb-ingest` chunks it and derives the document's vector from its
/// chunks). What this bound actually protects is the paths with no chunker in front of
/// them — above all a long search query, which is embedded verbatim.
pub const MAX_INPUT_CHARS: usize = 2048 * 2;

/// Configuration for the ollama embedder.
///
/// Serde defaults let a config file omit any field and still get a working
/// local setup.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OllamaConfig {
    /// Base URL of the ollama server (no trailing path).
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Model name as known to ollama (e.g. `nomic-embed-text`).
    #[serde(default = "default_model")]
    pub model: String,
    /// Expected vector dimensionality for `model`.
    #[serde(default = "default_dim")]
    pub dim: usize,
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}
fn default_model() -> String {
    DEFAULT_MODEL.to_owned()
}
fn default_dim() -> usize {
    DEFAULT_DIM
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            model: default_model(),
            dim: default_dim(),
        }
    }
}

/// An [`Embedder`] backed by a local ollama HTTP server.
pub struct OllamaEmbedder {
    client: reqwest::blocking::Client,
    base_url: String,
    model: String,
    dim: usize,
}

/// Request body for `POST /api/embeddings`.
#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

/// Response body for `POST /api/embeddings`.
#[derive(Deserialize)]
struct EmbedResponse {
    embedding: Vec<f32>,
}

/// Response body for `GET /api/tags` (the installed-model listing).
#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagModel>,
}

#[derive(Deserialize)]
struct TagModel {
    name: String,
    /// ollama's content digest for the model (a stable, resolved identity).
    #[serde(default)]
    digest: String,
}

impl OllamaEmbedder {
    /// Build an embedder from `config`.
    ///
    /// # Errors
    /// Returns [`Error::EmbedderUnavailable`] if the HTTP client cannot be built.
    pub fn new(config: OllamaConfig) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            // Fail fast when nothing is listening, but allow slow embeds to finish.
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_mins(1))
            .build()
            .map_err(|e| Error::EmbedderUnavailable(format!("could not build HTTP client: {e}")))?;
        Ok(Self {
            client,
            base_url: config.base_url,
            model: config.model,
            dim: config.dim,
        })
    }

    /// The `base_url` with any trailing slash removed, ready for path joins.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// Turn a transport failure into an actionable "is ollama running?" error.
    fn unreachable(&self, err: &reqwest::Error) -> Error {
        Error::EmbedderUnavailable(format!(
            "cannot reach ollama at {}: {err}. Is it running? Start it with `ollama serve` \
             (install from https://ollama.com)",
            self.base_url
        ))
    }

    /// True when `installed` names `self.model`, accounting for ollama's implicit
    /// `:latest` tag (a bare `nomic-embed-text` shows up as `nomic-embed-text:latest`).
    fn model_matches(&self, installed: &str) -> bool {
        installed == self.model
            || installed
                .split_once(':')
                .is_some_and(|(base, _tag)| base == self.model)
    }

    /// Fetch the installed-model listing from `GET /api/tags`.
    fn fetch_tags(&self) -> Result<TagsResponse> {
        let resp = self
            .client
            .get(self.endpoint("/api/tags"))
            .send()
            .map_err(|e| self.unreachable(&e))?;
        if !resp.status().is_success() {
            return Err(Error::EmbedderUnavailable(format!(
                "ollama /api/tags returned status {} at {}",
                resp.status(),
                self.base_url
            )));
        }
        resp.json().map_err(|e| {
            Error::EmbedderUnavailable(format!("could not parse ollama /api/tags response: {e}"))
        })
    }
}

impl Embedder for OllamaEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let prompt = truncate_to_chars(text, MAX_INPUT_CHARS);
        let resp = self
            .client
            .post(self.endpoint("/api/embeddings"))
            .json(&EmbedRequest {
                model: &self.model,
                prompt,
            })
            .send()
            .map_err(|e| self.unreachable(&e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            // ollama answers 404 when the model isn't pulled.
            let hint = if status == reqwest::StatusCode::NOT_FOUND {
                format!(". Run `ollama pull {}`", self.model)
            } else {
                String::new()
            };
            return Err(Error::EmbedderUnavailable(format!(
                "ollama returned {status} for model `{}`: {}{hint}",
                self.model,
                body.trim()
            )));
        }

        let parsed: EmbedResponse = resp.json().map_err(|e| {
            Error::EmbedderUnavailable(format!("could not parse ollama embedding response: {e}"))
        })?;

        if parsed.embedding.len() != self.dim {
            return Err(Error::Validation(format!(
                "embedder `{}` returned dim {} but {} was configured",
                self.model,
                parsed.embedding.len(),
                self.dim
            )));
        }
        Ok(parsed.embedding)
    }

    fn health_check(&self) -> Result<()> {
        let tags = self.fetch_tags()?;
        if tags.models.iter().any(|m| self.model_matches(&m.name)) {
            Ok(())
        } else {
            Err(Error::EmbedderUnavailable(format!(
                "model `{}` is not installed on ollama at {}. Run `ollama pull {}`",
                self.model, self.base_url, self.model
            )))
        }
    }

    fn resolved_version(&self) -> Result<Option<String>> {
        // The digest pins what `:latest` (or any tag) currently resolves to, so a
        // later re-point to new weights shows up as a changed version.
        let tags = self.fetch_tags()?;
        Ok(tags
            .models
            .iter()
            .find(|m| self.model_matches(&m.name))
            .map(|m| m.digest.clone())
            .filter(|d| !d.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::{OllamaConfig, OllamaEmbedder, DEFAULT_DIM, DEFAULT_MODEL};
    use jkb_types::{Embedder, Error};

    fn embedder() -> OllamaEmbedder {
        OllamaEmbedder::new(OllamaConfig::default()).expect("client builds")
    }

    #[test]
    fn config_default_is_local_nomic() {
        let c = OllamaConfig::default();
        assert_eq!(c.model, DEFAULT_MODEL);
        assert_eq!(c.dim, DEFAULT_DIM);
        assert!(c.base_url.contains("11434"));
    }

    #[test]
    fn model_and_dim_report_config() {
        let e = embedder();
        assert_eq!(e.model(), DEFAULT_MODEL);
        assert_eq!(e.dim(), DEFAULT_DIM);
    }

    #[test]
    fn model_matches_bare_and_tagged() {
        let e = embedder();
        assert!(e.model_matches("nomic-embed-text"));
        assert!(e.model_matches("nomic-embed-text:latest"));
        assert!(!e.model_matches("nomic-embed-text-v2"));
        assert!(!e.model_matches("mxbai-embed-large"));
    }

    #[test]
    fn endpoint_trims_trailing_slash() {
        let e = OllamaEmbedder::new(OllamaConfig {
            base_url: "http://localhost:11434/".to_owned(),
            ..OllamaConfig::default()
        })
        .expect("client builds");
        assert_eq!(e.endpoint("/api/tags"), "http://localhost:11434/api/tags");
    }

    #[test]
    fn unreachable_server_reports_actionable_error() {
        // Port 1 has nothing listening, so this exercises the transport-failure path.
        let e = OllamaEmbedder::new(OllamaConfig {
            base_url: "http://127.0.0.1:1".to_owned(),
            ..OllamaConfig::default()
        })
        .expect("client builds");
        let err = e.embed("hello").expect_err("no server on port 1");
        assert!(matches!(err, Error::EmbedderUnavailable(_)));
        assert!(err.to_string().contains("ollama serve"));
    }

    /// Live smoke test against a running ollama with `nomic-embed-text` pulled.
    /// Ignored by default; run with `cargo test -p jkb-embed -- --ignored`.
    #[test]
    #[ignore = "requires a running ollama server with nomic-embed-text"]
    fn live_embed_returns_correct_dim() {
        let e = embedder();
        e.health_check().expect("ollama healthy");
        let v = e.embed("the quick brown fox").expect("embed succeeds");
        assert_eq!(v.len(), DEFAULT_DIM);
    }
}
