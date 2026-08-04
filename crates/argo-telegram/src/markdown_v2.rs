//! Agent markdown to Telegram MarkdownV2.
//!
//! Telegram's MarkdownV2 is unforgiving: every one of eighteen characters must be
//! backslash-escaped wherever it is not acting as markup, and a single stray one
//! makes the whole `sendMessage` fail rather than render badly. Agent output is
//! full of them — file paths, diffs, regexes, code.
//!
//! So the text is parsed as CommonMark and re-emitted, escaping literal text and
//! placing markup deliberately. Constructs Telegram has no equivalent for
//! (tables, headings, images) are flattened into something readable instead of
//! being dropped.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Characters MarkdownV2 reserves in ordinary text.
const RESERVED: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!', '\\',
];

/// Rows a table may have before it is rendered as a code block instead.
const MAX_BULLETED_TABLE_ROWS: usize = 4;

/// Escapes text appearing outside any markup.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        if RESERVED.contains(&character) {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// Escapes text inside a code span or block.
///
/// Only the fence characters themselves are special there, and over-escaping
/// would put visible backslashes into the user's code.
fn escape_code(text: &str) -> String {
    text.replace('\\', "\\\\").replace('`', "\\`")
}

/// Converts CommonMark to MarkdownV2.
pub fn to_markdown_v2(source: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let mut out = String::with_capacity(source.len() + source.len() / 4);
    let mut state = Renderer::default();
    for event in Parser::new_ext(source, options) {
        state.push(&mut out, event);
    }
    state.finish(&mut out);
    out.trim_matches('\n').to_string()
}

/// Tracks the nesting a MarkdownV2 rendering has to remember.
#[derive(Default)]
struct Renderer {
    /// Depth and per-level ordinal for enclosing lists; `None` is a bullet list.
    lists: Vec<Option<u64>>,
    /// True while inside a fenced or indented code block.
    in_code_block: bool,
    /// Accumulated table, rendered when the table closes.
    table: Option<Table>,
    /// True while inside a block quote, which prefixes each line.
    in_quote: bool,
    /// Destinations of the links currently open, innermost last.
    links: Vec<String>,
}

#[derive(Default)]
struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    in_header: bool,
    cell: String,
    row: Vec<String>,
}

impl Renderer {
    fn push(&mut self, out: &mut String, event: Event<'_>) {
        // Table cells capture their text rather than emitting it inline.
        if let Some(table) = self.table.as_mut() {
            match &event {
                Event::Text(text) | Event::Code(text) => {
                    table.cell.push_str(text);
                    return;
                }
                Event::SoftBreak | Event::HardBreak => {
                    table.cell.push(' ');
                    return;
                }
                _ => {}
            }
        }

        match event {
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => out.push_str("\n\n"),

            // Telegram has no headings. Bold on its own line is the closest
            // thing that still reads as a heading in a chat bubble.
            Event::Start(Tag::Heading { .. }) => out.push('*'),
            Event::End(TagEnd::Heading(_)) => out.push_str("*\n\n"),

            Event::Start(Tag::BlockQuote(_)) => {
                self.in_quote = true;
                out.push('>');
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.in_quote = false;
                out.push_str("\n\n");
            }

            Event::Start(Tag::CodeBlock(kind)) => {
                self.in_code_block = true;
                let language = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
                out.push_str("```");
                out.push_str(&language);
                out.push('\n');
            }
            Event::End(TagEnd::CodeBlock) => {
                self.in_code_block = false;
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n\n");
            }

            Event::Start(Tag::List(first)) => self.lists.push(first),
            Event::End(TagEnd::List(_)) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    out.push('\n');
                }
            }
            Event::Start(Tag::Item) => {
                let depth = self.lists.len().saturating_sub(1);
                out.push_str(&"  ".repeat(depth));
                match self.lists.last_mut() {
                    Some(Some(ordinal)) => {
                        // The dot after a list number is reserved, so it is
                        // escaped like any other literal.
                        out.push_str(&format!("{ordinal}\\. "));
                        *ordinal += 1;
                    }
                    _ => out.push_str("• "),
                }
            }
            Event::End(TagEnd::Item) => out.push('\n'),

            Event::Start(Tag::Emphasis) => out.push('_'),
            Event::End(TagEnd::Emphasis) => out.push('_'),
            Event::Start(Tag::Strong) => out.push('*'),
            Event::End(TagEnd::Strong) => out.push('*'),
            Event::Start(Tag::Strikethrough) => out.push('~'),
            Event::End(TagEnd::Strikethrough) => out.push('~'),

            Event::Start(Tag::Link { dest_url, .. }) => {
                out.push('[');
                // Held until the link closes, when the destination is written.
                self.links.push(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                let destination = self.links.pop().unwrap_or_default();
                // Inside a link destination only `)` and `\` are special;
                // escaping the rest would corrupt the URL.
                let destination = destination.replace('\\', "\\\\").replace(')', "\\)");
                out.push_str(&format!("]({destination})"));
            }

            // An image cannot be rendered inline; its alt text already came
            // through as ordinary text.
            Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => {}

            Event::Start(Tag::Table(_)) => self.table = Some(Table::default()),
            Event::Start(Tag::TableHead) => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = false;
                    table.header = std::mem::take(&mut table.row);
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    if !row.is_empty() {
                        table.rows.push(row);
                    }
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = self.table.as_mut() {
                    let cell = std::mem::take(&mut table.cell);
                    table.row.push(cell.trim().to_string());
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = self.table.take() {
                    out.push_str(&render_table(&table));
                }
            }

            Event::Text(text) => {
                if self.in_code_block {
                    out.push_str(&escape_code(&text));
                } else {
                    out.push_str(&escape(&text));
                }
            }
            Event::Code(code) => {
                out.push('`');
                out.push_str(&escape_code(&code));
                out.push('`');
            }
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push('\n'),
            Event::Rule => out.push_str("\n\\-\\-\\-\n\n"),
            Event::Html(html) | Event::InlineHtml(html) => out.push_str(&escape(&html)),
            _ => {}
        }
    }

    fn finish(&mut self, out: &mut String) {
        // An unterminated code block would make the whole message unparseable,
        // so it is closed rather than sent broken.
        if self.in_code_block {
            out.push_str("\n```");
            self.in_code_block = false;
        }
    }
}

/// Renders a table Telegram cannot display natively.
///
/// Small tables become labelled bullet groups, which stay readable on a phone.
/// Larger ones become a fixed-width code block, because at that size alignment
/// carries more meaning than prose does.
fn render_table(table: &Table) -> String {
    if table.rows.is_empty() {
        return String::new();
    }
    if table.rows.len() <= MAX_BULLETED_TABLE_ROWS {
        let mut out = String::new();
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate() {
                if cell.is_empty() {
                    continue;
                }
                match table.header.get(index) {
                    Some(header) if !header.is_empty() => {
                        out.push_str(&format!("• *{}*: {}\n", escape(header), escape(cell)));
                    }
                    _ => out.push_str(&format!("• {}\n", escape(cell))),
                }
            }
            out.push('\n');
        }
        return out;
    }

    let mut widths: Vec<usize> = table
        .header
        .iter()
        .map(|cell| cell.chars().count())
        .collect();
    for row in &table.rows {
        for (index, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            match widths.get_mut(index) {
                Some(current) => *current = (*current).max(width),
                None => widths.push(width),
            }
        }
    }
    let line = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| format!("{:width$}", cell, width = widths[index]))
            .collect::<Vec<_>>()
            .join("  ")
    };

    let mut out = String::from("```\n");
    if !table.header.is_empty() {
        out.push_str(&escape_code(line(&table.header).trim_end()));
        out.push('\n');
    }
    for row in &table.rows {
        out.push_str(&escape_code(line(row).trim_end()));
        out.push('\n');
    }
    out.push_str("```\n\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reserved_character_is_escaped_in_literal_text() {
        // One unescaped character rejects the entire message, so this is the
        // single most load-bearing behaviour in the bridge.
        for reserved in RESERVED {
            let escaped = escape(&format!("x{reserved}y"));
            assert_eq!(escaped, format!("x\\{reserved}y"), "unescaped {reserved:?}");
        }
    }

    #[test]
    fn prose_that_only_looks_like_markup_is_escaped_not_interpreted() {
        // Agent replies are full of punctuation that CommonMark ignores but
        // MarkdownV2 reserves; those are the characters that break sends.
        let rendered = to_markdown_v2("exit code -1 (see run #3) = failed! {retry}");
        for fragment in [
            "\\-1",
            "\\(see",
            "\\#3\\)",
            "\\=",
            "failed\\!",
            "\\{retry\\}",
        ] {
            assert!(
                rendered.contains(fragment),
                "missing {fragment} in {rendered}"
            );
        }
    }

    #[test]
    fn file_paths_and_versions_survive_intact() {
        // The two shapes that appear in almost every agent reply.
        let rendered = to_markdown_v2("see crates/argo-tui/src/run.rs for v0.1.6");
        assert!(rendered.contains("argo\\-tui"), "{rendered}");
        assert!(rendered.contains("run\\.rs"), "{rendered}");
        assert!(rendered.contains("v0\\.1\\.6"), "{rendered}");
    }

    #[test]
    fn code_blocks_keep_their_language_and_do_not_escape_their_contents() {
        let rendered = to_markdown_v2("```rust\nlet x = a_b.c(1);\n```");
        assert!(rendered.starts_with("```rust\n"), "{rendered}");
        // Inside a fence the text is code, not markup: escaping it would put
        // visible backslashes into the user's snippet.
        assert!(rendered.contains("let x = a_b.c(1);"), "{rendered}");
        assert!(rendered.trim_end().ends_with("```"), "{rendered}");
    }

    #[test]
    fn an_unterminated_fence_is_closed_rather_than_sent_broken() {
        // Streaming means a partial reply is rendered constantly, and a dangling
        // fence would make every edit fail until the block finished.
        let rendered = to_markdown_v2("here you go:\n\n```rust\nlet x = 1;");
        assert_eq!(
            rendered.matches("```").count(),
            2,
            "fence must be balanced: {rendered}"
        );
    }

    #[test]
    fn inline_code_is_preserved_with_backticks_escaped_inside() {
        let rendered = to_markdown_v2("run `cargo test --workspace` now");
        assert!(rendered.contains("`cargo test --workspace`"), "{rendered}");
    }

    #[test]
    fn emphasis_and_strong_map_to_telegram_markup() {
        let rendered = to_markdown_v2("*bold* and _italic_ and ~~gone~~");
        assert!(
            rendered.contains("_bold_") || rendered.contains("*bold*"),
            "{rendered}"
        );
        assert!(rendered.contains('~'), "{rendered}");
    }

    #[test]
    fn ordered_list_numbers_keep_their_dot_escaped() {
        let rendered = to_markdown_v2("1. first\n2. second");
        assert!(rendered.contains("1\\. first"), "{rendered}");
        assert!(rendered.contains("2\\. second"), "{rendered}");
    }

    #[test]
    fn a_small_table_becomes_labelled_bullets() {
        let source = "| CLI | Resume |\n| --- | --- |\n| claude | yes |\n| codex | yes |";
        let rendered = to_markdown_v2(source);
        assert!(rendered.contains("*CLI*: claude"), "{rendered}");
        assert!(rendered.contains("*Resume*: yes"), "{rendered}");
        assert!(!rendered.contains("```"), "{rendered}");
    }

    #[test]
    fn a_large_table_becomes_a_code_block_so_columns_still_line_up() {
        let mut source = String::from("| CLI | Resume |\n| --- | --- |\n");
        for index in 0..8 {
            source.push_str(&format!("| agent{index} | yes |\n"));
        }
        let rendered = to_markdown_v2(&source);
        assert!(rendered.contains("```"), "{rendered}");
        assert!(rendered.contains("agent7"), "{rendered}");
    }

    #[test]
    fn empty_input_produces_empty_output() {
        assert_eq!(to_markdown_v2(""), "");
        assert_eq!(to_markdown_v2("   \n\n  "), "");
    }
}
