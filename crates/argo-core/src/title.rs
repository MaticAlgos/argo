//! Deterministic conversation titles.
//!
//! A title is metadata, not something worth another paid model call. The first
//! user request is already the best description of the session; this module turns
//! its first meaningful line into a compact stable label shared by daemon and TUI.

/// Maximum title length in terminal columns/Unicode scalar values.
const MAX_TITLE_CHARS: usize = 72;

/// Derives a compact title from the first user request.
pub fn conversation_title(prompt: &str) -> String {
    let first = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```"))
        .unwrap_or("New conversation");

    // Strip common markdown list/heading markers without damaging paths or flags.
    let first = first
        .trim_start_matches('#')
        .trim_start()
        .strip_prefix("- ")
        .or_else(|| {
            first
                .trim_start_matches('#')
                .trim_start()
                .strip_prefix("* ")
        })
        .unwrap_or_else(|| first.trim_start_matches('#').trim_start());
    let normalized = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return "New conversation".to_string();
    }
    if normalized.chars().count() <= MAX_TITLE_CHARS {
        return normalized;
    }

    // Prefer a word boundary, but never return an empty title for one long token.
    let prefix: String = normalized.chars().take(MAX_TITLE_CHARS - 1).collect();
    let boundary = prefix.rfind(char::is_whitespace).unwrap_or(prefix.len());
    let kept = if boundary >= MAX_TITLE_CHARS / 2 {
        prefix[..boundary].trim_end()
    } else {
        prefix.trim_end()
    };
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_meaningful_line_becomes_the_title() {
        assert_eq!(
            conversation_title("\n\n  Add authentication\nthen tests"),
            "Add authentication"
        );
    }

    #[test]
    fn markdown_markers_are_removed() {
        assert_eq!(conversation_title("## Fix the queue"), "Fix the queue");
        assert_eq!(
            conversation_title("- Review this patch"),
            "Review this patch"
        );
        assert_eq!(conversation_title("```rust\nfn main() {}"), "fn main() {}");
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(conversation_title("fix    the\tbuild"), "fix the build");
    }

    #[test]
    fn long_titles_are_bounded_and_end_with_an_ellipsis() {
        let title = conversation_title(&"word ".repeat(40));
        assert!(title.chars().count() <= MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn blank_prompts_have_a_stable_fallback() {
        assert_eq!(conversation_title(" \n```"), "New conversation");
    }
}
