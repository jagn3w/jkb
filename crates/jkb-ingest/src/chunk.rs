//! Chunking: split document text into overlapping character windows.
//!
//! v1 uses character windows (token windows are a later refinement). Each window is
//! `max_chars` wide and starts `max_chars - overlap_chars` after the previous, so
//! adjacent chunks share `overlap_chars` of context. A short trailing window is
//! extended back to a full window so no chunk is smaller than `min_chars`.

/// Error returned when a [`ChunkConfig`] violates a windowing invariant.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// `max_chars` was zero, which cannot produce any window.
    #[error("max_chars must be non-zero")]
    ZeroMax,
    /// `overlap_chars >= max_chars`, which would collapse the stride toward one
    /// character per chunk (a near-quadratic blowup).
    #[error("overlap_chars ({overlap}) must be less than max_chars ({max})")]
    OverlapTooLarge {
        /// The offending `overlap_chars`.
        overlap: usize,
        /// The configured `max_chars`.
        max: usize,
    },
}

/// Configuration for [`chunk_text`].
///
/// The fields are public for ergonomic struct-literal construction, but a
/// `overlap_chars >= max_chars` would collapse the stride to one character and make
/// a long text explode into ~one chunk per character. [`chunk_text`] defensively
/// clamps the overlap below `max_chars` (see [`ChunkConfig::step`]) so that mistake
/// cannot blow up; prefer [`ChunkConfig::new`] to have it surfaced as an error
/// instead of silently clamped.
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    /// Maximum characters per chunk.
    pub max_chars: usize,
    /// Characters shared between adjacent chunks. Must be `< max_chars`; a value
    /// `>= max_chars` is clamped by [`chunk_text`] rather than honoured.
    pub overlap_chars: usize,
    /// Floor below which a lone trailing chunk is folded into a full final window.
    pub min_chars: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_chars: 1000,
            overlap_chars: 100,
            min_chars: 200,
        }
    }
}

impl ChunkConfig {
    /// Build a validated config.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ZeroMax`] if `max_chars == 0`, or
    /// [`ConfigError::OverlapTooLarge`] if `overlap_chars >= max_chars` (which would
    /// collapse the stride and produce a near-quadratic number of chunks).
    pub fn new(
        max_chars: usize,
        overlap_chars: usize,
        min_chars: usize,
    ) -> Result<Self, ConfigError> {
        if max_chars == 0 {
            return Err(ConfigError::ZeroMax);
        }
        if overlap_chars >= max_chars {
            return Err(ConfigError::OverlapTooLarge {
                overlap: overlap_chars,
                max: max_chars,
            });
        }
        Ok(Self {
            max_chars,
            overlap_chars,
            min_chars,
        })
    }

    /// The stride between window starts.
    ///
    /// A sane config keeps `overlap_chars < max_chars`, giving a stride of
    /// `max_chars - overlap_chars`. A caller that built the config via the public
    /// fields with `overlap_chars >= max_chars` would otherwise get a stride of `1`
    /// (or, at `overlap == max`, zero before the old `max(1)`), exploding a long
    /// text into ~one chunk per character. We defend by treating an out-of-range
    /// overlap as *no* overlap, so the stride is the full window width — no data is
    /// lost, only the (nonsensical) requested overlap is dropped. Prefer
    /// [`ChunkConfig::new`] to surface the mistake as an error instead.
    fn step(self) -> usize {
        // `max(1)` covers a degenerate `max_chars` of 0 without panicking.
        let max = self.max_chars.max(1);
        let overlap = if self.overlap_chars >= max {
            0
        } else {
            self.overlap_chars
        };
        (max - overlap).max(1)
    }
}

/// Split `text` into overlapping character windows per `cfg`. Returns one whole
/// chunk when the text already fits, and an empty vector for empty text.
#[must_use]
pub fn chunk_text(text: &str, cfg: &ChunkConfig) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    if chars.len() <= cfg.max_chars {
        return vec![text.to_owned()];
    }

    let step = cfg.step();
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + cfg.max_chars).min(chars.len());
        chunks.push(chars[start..end].iter().collect::<String>());
        if end == chars.len() {
            break;
        }
        start += step;
    }

    // Fold a too-small trailing window into a full final window ending at the text
    // end, so no chunk is shorter than `min_chars`.
    let tail_too_small = chunks.len() > 1
        && chunks
            .last()
            .is_some_and(|last| last.chars().count() < cfg.min_chars);
    if tail_too_small {
        let start = chars.len().saturating_sub(cfg.max_chars);
        let full: String = chars[start..].iter().collect();
        if let Some(slot) = chunks.last_mut() {
            *slot = full;
        }
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::{chunk_text, ChunkConfig, ConfigError};

    fn cfg(max: usize, overlap: usize, min: usize) -> ChunkConfig {
        ChunkConfig {
            max_chars: max,
            overlap_chars: overlap,
            min_chars: min,
        }
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(chunk_text("", &ChunkConfig::default()).is_empty());
    }

    #[test]
    fn short_text_is_one_whole_chunk() {
        assert_eq!(chunk_text("hello", &cfg(100, 10, 5)), vec!["hello"]);
    }

    #[test]
    fn windows_overlap_by_configured_amount() {
        // 30 chars, max 10, overlap 4 => step 6 => starts 0,6,12,18,24
        let text: String = ('a'..='z').chain('0'..='9').take(30).collect();
        let chunks = chunk_text(&text, &cfg(10, 4, 1));
        assert!(chunks.len() >= 3);
        // adjacent chunks share the overlap tail/head
        let a: Vec<char> = chunks[0].chars().collect();
        let b: Vec<char> = chunks[1].chars().collect();
        assert_eq!(&a[6..10], &b[0..4]);
    }

    #[test]
    fn new_rejects_zero_max() {
        assert_eq!(ChunkConfig::new(0, 0, 0).unwrap_err(), ConfigError::ZeroMax);
    }

    #[test]
    fn new_rejects_overlap_at_or_above_max() {
        assert_eq!(
            ChunkConfig::new(10, 10, 2).unwrap_err(),
            ConfigError::OverlapTooLarge {
                overlap: 10,
                max: 10
            }
        );
        assert_eq!(
            ChunkConfig::new(10, 20, 2).unwrap_err(),
            ConfigError::OverlapTooLarge {
                overlap: 20,
                max: 10
            }
        );
    }

    #[test]
    fn new_accepts_valid_config() {
        let c = ChunkConfig::new(10, 4, 2).expect("valid");
        assert_eq!((c.max_chars, c.overlap_chars, c.min_chars), (10, 4, 2));
    }

    #[test]
    fn overlap_at_or_above_max_does_not_explode() {
        // Directly constructing via the public fields with overlap >= max used to
        // yield step 1 => ~one chunk per character (memory/CPU blowup + a flood of
        // near-identical embeddings). The out-of-range overlap is now dropped, so
        // the stride is the full window width (max) and the chunk count stays near
        // len/max rather than len.
        let text: String = "a".repeat(1000);
        for overlap in [10usize, 20, 100] {
            let chunks = chunk_text(&text, &cfg(10, overlap, 2));
            assert!(!chunks.is_empty());
            // step == max == 10 over 1000 chars => ~100 windows, well under 200.
            assert!(
                chunks.len() <= 200,
                "overlap {overlap} produced {} chunks",
                chunks.len()
            );
        }
    }

    #[test]
    fn overlap_below_max_still_strides_normally() {
        // Sanity: clamping only kicks in when overlap >= max; a valid overlap is
        // honoured exactly (step = max - overlap).
        let text: String = ('a'..='z').chain('0'..='9').take(30).collect();
        let chunks = chunk_text(&text, &cfg(10, 4, 1));
        // step 6 over 30 chars => starts 0,6,12,18,24 => 5 windows.
        assert_eq!(chunks.len(), 5);
    }

    #[test]
    fn multibyte_windows_never_split_a_codepoint() {
        let text = "😀😁😂🤣🙂🙃😉😊".repeat(3); // 24 emoji
        let chunks = chunk_text(&text, &cfg(5, 1, 2));
        assert!(chunks.iter().all(|c| !c.is_empty()));
        // reconstructing is not the goal; correctness = valid UTF-8 chunks
        assert!(chunks
            .iter()
            .all(|c| c.chars().all(|ch| ch.len_utf8() == 4)));
    }
}
