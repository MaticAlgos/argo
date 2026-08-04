//! Splitting messages to Telegram's per-message ceiling.
//!
//! Agent replies routinely exceed 4096 characters. Cutting at an arbitrary index
//! can slice an escape or Markdown entity in half. Code fences are closed and
//! reopened; any other chunk whose entities are no longer self-contained is
//! explicitly marked for plain-text delivery.

/// Telegram's maximum message length, in UTF-16 code units.
pub const MAX_MESSAGE_CHARS: usize = 4096;

/// One independently sendable message chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageChunk {
    /// Text to send. Escapes are removed when `markdown` is false.
    pub text: String,
    /// Whether Telegram may parse this chunk as MarkdownV2.
    pub markdown: bool,
}

/// Returns Telegram's length measure for a string: UTF-16 code units.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Splits rendered MarkdownV2 into sendable chunks.
///
/// This compatibility surface returns text only. The daemon should use
/// [`split_message_safe`] so it also receives the per-chunk parse mode.
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    split_raw(text, limit)
}

/// Splits rendered MarkdownV2 and selects a safe parse mode for every chunk.
pub fn split_message_safe(text: &str, limit: usize) -> Vec<MessageChunk> {
    split_raw(text, limit)
        .into_iter()
        .map(|chunk| safe_message_chunk(&chunk))
        .collect()
}

/// Classifies one already-sized MarkdownV2 message.
pub fn safe_message_chunk(text: &str) -> MessageChunk {
    if markdown_is_self_contained(text) {
        MessageChunk {
            text: text.to_string(),
            markdown: true,
        }
    } else {
        MessageChunk {
            text: plain_text(text),
            markdown: false,
        }
    }
}

/// Removes MarkdownV2 escapes for literal delivery.
pub fn plain_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(character);
        }
    }
    out
}

/// Prefers a blank line, then a single newline, then a space; falls back to a
/// hard cut only for text with no break at all, such as one enormous token.
fn split_raw(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(64);
    if utf16_len(text) <= limit {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![text.to_string()]
        };
    }

    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if utf16_len(rest) <= limit {
            chunks.push(rest.to_string());
            break;
        }
        // Eight UTF-16 units cover "```\n" on both sides. Reopened fences omit
        // the optional language tag so an attacker-controlled long tag cannot
        // push a continuation over Telegram's ceiling.
        let budget = limit - 8;
        let cut = boundary(rest, budget);
        let (head, tail) = rest.split_at(cut);
        chunks.push(head.trim_end().to_string());
        rest = tail.trim_start_matches('\n');
    }

    balance_fences(chunks)
}

/// Byte index to cut at, at or before `budget` UTF-16 code units.
fn boundary(text: &str, budget: usize) -> usize {
    let mut units = 0;
    let mut hard = text.len();
    for (index, character) in text.char_indices() {
        let next = units + character.len_utf16();
        if next > budget {
            hard = index;
            break;
        }
        units = next;
    }
    let window = &text[..hard];
    for separator in ["\n\n", "\n", " "] {
        if let Some(index) = window.rfind(separator) {
            if index > hard / 4 {
                return index + separator.len();
            }
        }
    }
    hard
}

/// Closes a fence left open by a cut and reopens it on the next chunk.
fn balance_fences(chunks: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(chunks.len());
    let mut reopen = false;

    for chunk in chunks {
        let mut chunk = if reopen {
            format!("```\n{chunk}")
        } else {
            chunk
        };
        reopen = false;
        if fence_count(&chunk) % 2 == 1 {
            reopen = true;
            if !chunk.ends_with('\n') {
                chunk.push('\n');
            }
            chunk.push_str("```");
        }
        out.push(chunk);
    }
    out
}

/// Counts unescaped triple backticks.
fn fence_count(text: &str) -> usize {
    text.match_indices("```")
        .filter(|(index, _)| !is_escaped(text.as_bytes(), *index))
        .count()
}

/// Conservatively checks whether a cut left a MarkdownV2 entity open.
fn markdown_is_self_contained(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut fence = false;
    let mut inline_code = false;
    let mut bold = false;
    let mut italic = false;
    let mut underline = false;
    let mut strike = false;
    let mut spoiler = false;
    let mut brackets = 0_i32;
    let mut link_targets = 0_i32;

    while index < bytes.len() {
        if bytes[index] == b'\\' && !is_escaped(bytes, index) {
            index += 2;
            continue;
        }
        if bytes[index..].starts_with(b"```") && !is_escaped(bytes, index) {
            fence = !fence;
            index += 3;
            continue;
        }
        if fence {
            index += 1;
            continue;
        }
        if bytes[index] == b'`' && !is_escaped(bytes, index) {
            inline_code = !inline_code;
            index += 1;
            continue;
        }
        if inline_code {
            index += 1;
            continue;
        }
        match bytes[index] {
            b'*' if !is_escaped(bytes, index) => bold = !bold,
            b'_' if !is_escaped(bytes, index) => {
                if bytes.get(index + 1) == Some(&b'_') {
                    underline = !underline;
                    index += 1;
                } else {
                    italic = !italic;
                }
            }
            b'~' if !is_escaped(bytes, index) => strike = !strike,
            b'|' if !is_escaped(bytes, index) && bytes.get(index + 1) == Some(&b'|') => {
                spoiler = !spoiler;
                index += 1;
            }
            b'[' if !is_escaped(bytes, index) => brackets += 1,
            b']' if !is_escaped(bytes, index) => brackets -= 1,
            b'(' if !is_escaped(bytes, index) && index > 0 && bytes[index - 1] == b']' => {
                link_targets += 1;
            }
            b')' if !is_escaped(bytes, index) && link_targets > 0 => link_targets -= 1,
            _ => {}
        }
        if brackets < 0 {
            return false;
        }
        index += 1;
    }

    !fence
        && !inline_code
        && !bold
        && !italic
        && !underline
        && !strike
        && !spoiler
        && brackets == 0
        && link_targets == 0
        && !has_dangling_escape(bytes)
}

fn is_escaped(bytes: &[u8], index: usize) -> bool {
    let mut slashes = 0;
    let mut cursor = index;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        slashes += 1;
        cursor -= 1;
    }
    slashes % 2 == 1
}

fn has_dangling_escape(bytes: &[u8]) -> bool {
    let mut slashes = 0;
    for byte in bytes.iter().rev() {
        if *byte != b'\\' {
            break;
        }
        slashes += 1;
    }
    slashes % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_chunk_and_empty_text_is_none() {
        assert_eq!(split_message("hello", MAX_MESSAGE_CHARS), vec!["hello"]);
        assert!(split_message("", MAX_MESSAGE_CHARS).is_empty());
    }

    #[test]
    fn every_chunk_stays_within_the_utf16_limit() {
        let text = "paragraph 😀 text here\n\n".repeat(600);
        let chunks = split_message(&text, MAX_MESSAGE_CHARS);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(
                utf16_len(chunk) <= MAX_MESSAGE_CHARS,
                "chunk of {} units exceeds the ceiling",
                utf16_len(chunk)
            );
        }
    }

    #[test]
    fn splitting_prefers_a_paragraph_break() {
        let text = format!("{}\n\n{}", "a".repeat(100), "b".repeat(100));
        let chunks = split_message(&text, 150);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].chars().all(|c| c == 'a'));
        assert!(chunks[1].chars().all(|c| c == 'b'));
    }

    #[test]
    fn a_code_block_cut_in_half_is_closed_and_reopened_within_limit() {
        let body = (0..200)
            .map(|index| format!("let value_{index} = {index};"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("```rust\n{body}\n```");
        let chunks = split_message_safe(&text, 900);

        assert!(chunks.len() > 1, "expected a split");
        for chunk in &chunks {
            assert_eq!(fence_count(&chunk.text) % 2, 0, "{}", chunk.text);
            assert!(utf16_len(&chunk.text) <= 900, "{}", chunk.text.len());
            assert!(chunk.markdown, "balanced code chunks stay formatted");
        }
        assert!(chunks[1].text.starts_with("```\n"), "{}", chunks[1].text);
    }

    #[test]
    fn a_long_fence_language_cannot_overflow_a_continuation() {
        let text = format!("```{}\n{}\n```", "x".repeat(300), "body ".repeat(300));
        let chunks = split_message_safe(&text, 256);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| utf16_len(&chunk.text) <= 256));
    }

    #[test]
    fn an_entity_cut_in_half_falls_back_to_plain_text() {
        let text = format!("*{}*", "word ".repeat(100));
        let chunks = split_message_safe(&text, 100);
        assert!(chunks.len() > 1);
        assert!(!chunks[0].markdown);
        assert!(!chunks.last().expect("last").markdown);
    }

    #[test]
    fn a_cut_escape_is_never_sent_as_broken_markdown() {
        let chunk = safe_message_chunk("path\\");
        assert!(!chunk.markdown);
        assert_eq!(chunk.text, "path");
    }

    #[test]
    fn text_with_no_break_at_all_is_still_split() {
        let text = "x".repeat(500);
        let chunks = split_message(&text, 100);
        assert!(chunks.len() >= 5);
        assert_eq!(chunks.concat().chars().count(), 500);
    }

    #[test]
    fn a_tiny_limit_is_raised_to_something_workable() {
        let chunks = split_message(&"word ".repeat(100), 1);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn plain_text_removes_escapes_without_panicking() {
        assert_eq!(plain_text("run\\.rs in argo\\-tui"), "run.rs in argo-tui");
        assert_eq!(plain_text("a\\\\b"), "a\\b");
        assert_eq!(plain_text("trailing\\"), "trailing");
    }
}
