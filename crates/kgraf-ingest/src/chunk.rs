//! Chunking extracted text to a budget with `text-splitter`.
//!
//! Uses the character-count sizer (`text_splitter::TextSplitter::new`)
//! rather than a tokenizer-backed one. A token-accurate budget would need
//! the `tokenizers` (or a model-specific) feature and a loaded vocabulary,
//! which is a per-provider concern this crate does not own (token
//! accounting lives in one place, `kgraf-core`/the provider adapters).
//! A character budget is a stable, dependency-free proxy the interpretation
//! pass can convert with its own token estimator
//! (`kgraf_core::budget::estimate_tokens`) if it needs one.

use text_splitter::TextSplitter;

/// Split `text` into chunks of at most `max_characters`, breaking at the
/// most semantically sensible boundary `text-splitter` finds under that
/// budget (paragraph, then sentence, then word).
pub fn chunk_text(text: &str, max_characters: usize) -> Vec<String> {
    let splitter = TextSplitter::new(max_characters);
    splitter.chunks(text).map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_respect_the_character_budget() {
        let text =
            "Sentence one is here. Sentence two follows it. Sentence three ends the paragraph.";
        let chunks = chunk_text(text, 40);

        assert!(chunks.len() > 1, "text longer than the budget must split");
        for chunk in &chunks {
            assert!(
                chunk.chars().count() <= 40,
                "chunk {chunk:?} exceeds the character budget"
            );
        }
        // No text is dropped: the chunks reassemble (modulo the whitespace
        // the splitter trims at boundaries) into the original content.
        let rejoined: String = chunks.concat();
        for word in ["Sentence", "one", "two", "three", "paragraph"] {
            assert!(rejoined.contains(word), "missing {word:?} after chunking");
        }
    }

    #[test]
    fn short_text_is_a_single_chunk() {
        let chunks = chunk_text("short", 1000);
        assert_eq!(chunks, vec!["short".to_string()]);
    }
}
