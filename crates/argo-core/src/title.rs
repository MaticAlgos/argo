//! Deterministic conversation titles and descriptions.
//!
//! A title is metadata, not something worth another paid model call. The latest
//! user request identifies the current focus, while a short description
//! retains where the conversation began. Metadata stays useful as a long chat
//! evolves without spending another model call.

/// Maximum title length in terminal columns/Unicode scalar values.
const MAX_TITLE_CHARS: usize = 72;
const MAX_DESCRIPTION_CHARS: usize = 240;

/// Derives a compact title from the latest user request.
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

/// Describes the arc from the first request to the latest request.
pub fn conversation_description(prompts: &[String]) -> String {
    let meaningful = prompts
        .iter()
        .map(|prompt| compact_prompt(prompt))
        .filter(|prompt| !prompt.is_empty())
        .collect::<Vec<_>>();
    let Some(current) = meaningful.last() else {
        return String::new();
    };
    if meaningful.len() == 1 {
        return truncate(current, MAX_DESCRIPTION_CHARS);
    }
    let started = meaningful.first().unwrap_or(current);
    if started == current {
        return truncate(current, MAX_DESCRIPTION_CHARS);
    }
    truncate(
        &format!(
            "Started with: {}. Current focus: {}",
            truncate(started, 88).trim_end_matches(['.', '…']),
            current
        ),
        MAX_DESCRIPTION_CHARS,
    )
}

fn compact_prompt(prompt: &str) -> String {
    prompt
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("```"))
        .take(4)
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let prefix: String = value.chars().take(max.saturating_sub(1)).collect();
    let boundary = prefix.rfind(char::is_whitespace).unwrap_or(prefix.len());
    let kept = if boundary >= max / 2 {
        &prefix[..boundary]
    } else {
        prefix.as_str()
    };
    format!("{}…", kept.trim_end())
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

    #[test]
    fn description_tracks_the_start_and_current_focus() {
        let prompts = vec![
            "Build the authentication screen".to_string(),
            "Now fix keyboard navigation and add tests".to_string(),
        ];
        let description = conversation_description(&prompts);
        assert!(description.contains("Started with: Build the authentication screen"));
        assert!(description.contains("Current focus: Now fix keyboard navigation"));
    }

    #[test]
    fn descriptions_are_bounded() {
        let description = conversation_description(&["word ".repeat(200)]);
        assert!(description.chars().count() <= MAX_DESCRIPTION_CHARS);
        assert!(description.ends_with('…'));
    }
}
