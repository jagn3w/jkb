//! Chunking: split document text into overlapping character windows.
//!
//! v1 uses character windows (token windows are a later refinement). Each window is
//! `max_chars` wide and starts `max_chars - overlap_chars` after the previous, so
//! adjacent chunks share `overlap_chars` of context. A short trailing window is
//! extended back to a full window so no chunk is smaller than `min_chars`.

/// Configuration for [`chunk_text`].
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    /// Maximum characters per chunk.
    pub max_chars: usize,
    /// Characters shared between adjacent chunks.
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
    /// The stride between window starts.
    fn step(self) -> usize {
        // Guaranteed positive: `new`/callers keep `overlap < max`.
        self.max_chars.saturating_sub(self.overlap_chars).max(1)
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
    use super::{chunk_text, ChunkConfig};

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
