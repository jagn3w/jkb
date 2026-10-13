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
/// The fields are private so the windowing invariant — `max_chars > 0` and
/// `overlap_chars < max_chars` — holds by construction: the only ways to obtain a
/// `ChunkConfig` are [`ChunkConfig::new`], which refuses a violating config with a
/// [`ConfigError`], and [`Default`], which is itself built through `new` at compile
/// time. An `overlap_chars >= max_chars` would otherwise collapse the stride toward
/// one character and explode a long text into ~one chunk per character.
///
/// No struct literal can be written outside this module — not even a
/// functional-update one, which would let a caller overwrite a field unchecked:
///
/// ```compile_fail,E0451
/// use jkb_ingest::chunk::ChunkConfig;
/// let _ = ChunkConfig { ..ChunkConfig::default() };
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    /// Maximum characters per chunk. Always non-zero.
    max: usize,
    /// Characters shared between adjacent chunks. Always `< max`.
    overlap: usize,
    /// Floor below which a lone trailing chunk is folded into a full final window.
    min: usize,
}

/// The default config, validated by [`ChunkConfig::new`] during constant
/// evaluation: an invalid default is a compile error, not a runtime one.
const DEFAULT_CONFIG: ChunkConfig = match ChunkConfig::new(1000, 100, 200) {
    Ok(cfg) => cfg,
    Err(_) => panic!("the default ChunkConfig violates its own invariant"),
};

impl Default for ChunkConfig {
    fn default() -> Self {
        DEFAULT_CONFIG
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
    pub const fn new(
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
            max: max_chars,
            overlap: overlap_chars,
            min: min_chars,
        })
    }

    /// Maximum characters per chunk (always non-zero).
    #[must_use]
    pub const fn max_chars(&self) -> usize {
        self.max
    }

    /// Characters shared between adjacent chunks (always `< max_chars`).
    #[must_use]
    pub const fn overlap_chars(&self) -> usize {
        self.overlap
    }

    /// Floor below which a lone trailing chunk is folded into a full final window.
    #[must_use]
    pub const fn min_chars(&self) -> usize {
        self.min
    }

    /// The stride between window starts: `max_chars - overlap_chars`, which is at
    /// least 1 because [`ChunkConfig::new`] guarantees `overlap_chars < max_chars`.
    /// No clamp is needed — an out-of-range overlap cannot be constructed.
    const fn step(self) -> usize {
        self.max - self.overlap
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
    if chars.len() <= cfg.max {
        return vec![text.to_owned()];
    }

    let step = cfg.step();
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + cfg.max).min(chars.len());
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
            .is_some_and(|last| last.chars().count() < cfg.min);
    if tail_too_small {
        let start = chars.len().saturating_sub(cfg.max);
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
        ChunkConfig::new(max, overlap, min).expect("valid test config")
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
        assert_eq!(
            (c.max_chars(), c.overlap_chars(), c.min_chars()),
            (10, 4, 2)
        );
    }

    #[test]
    fn default_is_the_documented_valid_config() {
        let d = ChunkConfig::default();
        assert_eq!(
            (d.max_chars(), d.overlap_chars(), d.min_chars()),
            (1000, 100, 200)
        );
        assert_eq!(d.step(), 900);
    }

    #[test]
    fn overlap_below_max_still_strides_normally() {
        // A valid overlap is honoured exactly (step = max - overlap).
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
