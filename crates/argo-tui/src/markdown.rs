//! Terminal-safe Markdown presentation for assistant transcript entries.
//!
//! Canonical message text remains untouched in `App`; this module only converts
//! it into styled, pre-wrapped terminal rows. Pre-wrapping is important because
//! transcript scroll offsets are measured in visual rows rather than messages.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};

const HEADING: Color = Color::Rgb(130, 205, 255);
const CODE: Color = Color::Rgb(180, 225, 190);
const CODE_BG: Color = Color::Rgb(38, 43, 52);
const LINK: Color = Color::Rgb(105, 190, 255);
const QUOTE: Color = Color::Rgb(150, 160, 185);
const BULLET: Color = Color::Rgb(110, 205, 255);

#[derive(Debug, Clone, PartialEq)]
struct Segment {
    text: String,
    style: Style,
}

#[derive(Debug, Clone, Copy)]
struct ListState {
    next: Option<u64>,
}

struct MarkdownBuilder {
    lines: Vec<Vec<Segment>>,
    current: Vec<Segment>,
    style: Style,
    style_stack: Vec<Style>,
    lists: Vec<ListState>,
    quote_depth: usize,
    in_code_block: bool,
    table_cell: usize,
}

impl MarkdownBuilder {
    fn new(base: Style) -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            style: base,
            style_stack: Vec::new(),
            lists: Vec::new(),
            quote_depth: 0,
            in_code_block: false,
            table_cell: 0,
        }
    }

    fn line_prefix(&mut self) {
        if self.current.is_empty() && self.quote_depth > 0 {
            self.push_styled(
                &"▎ ".repeat(self.quote_depth),
                self.style.fg(QUOTE).add_modifier(Modifier::ITALIC),
            );
        }
    }

    fn push(&mut self, text: &str) {
        self.line_prefix();
        self.push_styled(text, self.style);
    }

    fn push_styled(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.current.last_mut() {
            if last.style == style {
                last.text.push_str(text);
                return;
            }
        }
        self.current.push(Segment {
            text: text.to_string(),
            style,
        });
    }

    fn finish_line(&mut self) {
        self.lines.push(std::mem::take(&mut self.current));
        self.table_cell = 0;
    }

    fn ensure_new_block(&mut self) {
        if !self.current.is_empty() {
            self.finish_line();
        }
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(Vec::new());
        }
    }

    fn finish_block(&mut self) {
        if !self.current.is_empty() {
            self.finish_line();
        }
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(Vec::new());
        }
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(self.style);
        self.style = style;
    }

    fn modify_style(&mut self, modifier: Modifier) {
        self.push_style(self.style.add_modifier(modifier));
    }

    fn pop_style(&mut self) {
        if let Some(style) = self.style_stack.pop() {
            self.style = style;
        }
    }

    fn push_text_with_breaks(&mut self, text: &str) {
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            self.push(part);
            if parts.peek().is_some() {
                self.finish_line();
            }
        }
    }

    fn start_item(&mut self) {
        if !self.current.is_empty() {
            self.finish_line();
        }
        let depth = self.lists.len().saturating_sub(1);
        self.push_styled(&"  ".repeat(depth), self.style);
        let marker = match self.lists.last_mut() {
            Some(ListState { next: Some(next) }) => {
                let marker = format!("{next}. ");
                *next += 1;
                marker
            }
            _ => "• ".to_string(),
        };
        self.push_styled(&marker, self.style.fg(BULLET).add_modifier(Modifier::BOLD));
    }

    fn finish(mut self) -> Vec<Vec<Segment>> {
        if !self.current.is_empty() {
            self.finish_line();
        }
        while self.lines.last().is_some_and(Vec::is_empty) {
            self.lines.pop();
        }
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        self.lines
    }
}

/// Renders Markdown into assistant rows with the response rail and exact wrapping.
pub(crate) fn render(
    source: &str,
    prefix: &str,
    base: Style,
    inner_width: usize,
) -> Vec<TextLine<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);

    let mut builder = MarkdownBuilder::new(base);
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if !builder.current.is_empty() {
                        builder.finish_line();
                    }
                }
                Tag::Heading { .. } => {
                    builder.ensure_new_block();
                    builder.push_style(builder.style.fg(HEADING).add_modifier(Modifier::BOLD));
                }
                Tag::BlockQuote(_) => {
                    if !builder.current.is_empty() {
                        builder.finish_line();
                    }
                    builder.quote_depth += 1;
                }
                Tag::CodeBlock(kind) => {
                    builder.ensure_new_block();
                    builder.in_code_block = true;
                    let language = match kind {
                        CodeBlockKind::Fenced(language) if !language.is_empty() => {
                            Some(language.to_string())
                        }
                        _ => None,
                    };
                    if let Some(language) = language {
                        builder.push_styled(
                            &format!("  {language}"),
                            base.fg(QUOTE).add_modifier(Modifier::ITALIC),
                        );
                        builder.finish_line();
                    }
                    builder.push_style(base.fg(CODE).bg(CODE_BG));
                }
                Tag::List(start) => {
                    if builder.lists.is_empty() {
                        builder.ensure_new_block();
                    } else if !builder.current.is_empty() {
                        builder.finish_line();
                    }
                    builder.lists.push(ListState { next: start });
                }
                Tag::Item => builder.start_item(),
                Tag::Emphasis => builder.modify_style(Modifier::ITALIC),
                Tag::Strong => builder.modify_style(Modifier::BOLD),
                Tag::Strikethrough => builder.modify_style(Modifier::CROSSED_OUT),
                Tag::Link { .. } => {
                    builder.push_style(builder.style.fg(LINK).add_modifier(Modifier::UNDERLINED))
                }
                Tag::Image { .. } => {
                    builder.push_styled("image: ", base.fg(QUOTE));
                    builder.modify_style(Modifier::ITALIC);
                }
                Tag::Table(_) => builder.ensure_new_block(),
                Tag::TableHead | Tag::TableRow => {
                    if !builder.current.is_empty() {
                        builder.finish_line();
                    }
                }
                Tag::TableCell => {
                    if builder.table_cell > 0 {
                        builder.push_styled(" │ ", base.fg(QUOTE));
                    }
                    builder.table_cell += 1;
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => builder.finish_block(),
                TagEnd::Heading(_) => {
                    builder.pop_style();
                    builder.finish_block();
                }
                TagEnd::BlockQuote(_) => {
                    if !builder.current.is_empty() {
                        builder.finish_line();
                    }
                    builder.quote_depth = builder.quote_depth.saturating_sub(1);
                    builder.finish_block();
                }
                TagEnd::CodeBlock => {
                    if !builder.current.is_empty() {
                        builder.finish_line();
                    }
                    builder.pop_style();
                    builder.in_code_block = false;
                    builder.finish_block();
                }
                TagEnd::List(_) => {
                    if !builder.current.is_empty() {
                        builder.finish_line();
                    }
                    builder.lists.pop();
                    if builder.lists.is_empty() {
                        builder.finish_block();
                    }
                }
                TagEnd::Item => {
                    if !builder.current.is_empty() {
                        builder.finish_line();
                    }
                }
                TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Link
                | TagEnd::Image => builder.pop_style(),
                TagEnd::TableHead => {
                    if !builder.current.is_empty() {
                        builder.finish_line();
                    }
                    builder.push_styled("────────", base.fg(QUOTE));
                    builder.finish_line();
                }
                TagEnd::TableRow => {
                    if !builder.current.is_empty() {
                        builder.finish_line();
                    }
                }
                TagEnd::Table => builder.finish_block(),
                _ => {}
            },
            Event::Text(text) => {
                if builder.in_code_block {
                    builder.push_text_with_breaks(&text);
                } else {
                    builder.push(&text);
                }
            }
            Event::Code(code) => builder.push_styled(
                &code,
                builder
                    .style
                    .fg(CODE)
                    .bg(CODE_BG)
                    .add_modifier(Modifier::BOLD),
            ),
            Event::SoftBreak => builder.push(" "),
            Event::HardBreak => builder.finish_line(),
            Event::Rule => {
                builder.ensure_new_block();
                builder.push_styled("────────────────", base.fg(QUOTE));
                builder.finish_block();
            }
            Event::TaskListMarker(checked) => builder.push_styled(
                if checked { "☑ " } else { "☐ " },
                base.fg(BULLET).add_modifier(Modifier::BOLD),
            ),
            Event::Html(html) | Event::InlineHtml(html) => {
                builder.push_styled(&html, base.fg(QUOTE));
            }
            Event::FootnoteReference(label) => {
                builder.push_styled(&format!("[{label}]"), base.fg(LINK));
            }
            _ => {}
        }
    }

    let text_width = inner_width.saturating_sub(prefix.chars().count());
    let indent = " ".repeat(prefix.chars().count());
    let mut output = Vec::new();
    for logical in builder.finish() {
        for (index, row) in wrap_segments(&logical, text_width).into_iter().enumerate() {
            let marker = if index == 0 { prefix } else { &indent };
            let mut spans = Vec::with_capacity(row.len() + 1);
            spans.push(Span::styled(marker.to_string(), base));
            spans.extend(
                row.into_iter()
                    .map(|segment| Span::styled(segment.text, segment.style)),
            );
            output.push(TextLine::from(spans));
        }
    }
    output
}

fn wrap_segments(segments: &[Segment], width: usize) -> Vec<Vec<Segment>> {
    if segments.is_empty() || width == 0 {
        return vec![segments.to_vec()];
    }
    let chars: Vec<(char, Style)> = segments
        .iter()
        .flat_map(|segment| segment.text.chars().map(move |ch| (ch, segment.style)))
        .collect();
    let mut rows = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let available = chars.len() - start;
        let mut end = start + available.min(width);
        let mut next = end;
        if available > width {
            if let Some(space) = (start..end).rev().find(|index| chars[*index].0 == ' ') {
                if space > start {
                    end = space;
                    next = space + 1;
                }
            }
        }
        while next < chars.len() && chars[next].0 == ' ' {
            next += 1;
        }
        rows.push(coalesce(&chars[start..end]));
        start = next;
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

fn coalesce(chars: &[(char, Style)]) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    for (ch, style) in chars {
        if let Some(last) = segments.last_mut() {
            if last.style == *style {
                last.text.push(*ch);
                continue;
            }
        }
        segments.push(Segment {
            text: ch.to_string(),
            style: *style,
        });
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Style {
        Style::default().fg(Color::White)
    }

    #[test]
    fn markdown_markers_become_terminal_styles() {
        let lines = render(
            "## Result\n\n**bold** and *italic* with `code`",
            "│ ",
            base(),
            80,
        );
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!text.contains("##"));
        assert!(!text.contains("**"));
        assert!(text.contains("Result"));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "italic" && span.style.add_modifier.contains(Modifier::ITALIC)
        }));
        assert!(lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| { span.content == "code" && span.style.bg == Some(CODE_BG) }));
    }

    #[test]
    fn lists_quotes_tasks_and_code_blocks_are_presented() {
        let source = "> quoted\n\n- [x] done\n- pending\n\n```rust\nfn main() {}\n```";
        let lines = render(source, "│ ", base(), 40);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("▎ quoted"), "{text}");
        assert!(text.contains("• ☑ done"), "{text}");
        assert!(text.contains("• pending"), "{text}");
        assert!(text.contains("rust"), "{text}");
        assert!(text.contains("fn main() {}"), "{text}");
        assert!(!text.contains("```"));
    }

    #[test]
    fn styled_content_wraps_without_losing_or_leaking_markers() {
        let lines = render("**one two three four five**", "│ ", base(), 12);
        assert!(lines.len() >= 3);
        assert!(lines.iter().all(|line| {
            line.spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>()
                <= 12
        }));
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter().skip(1))
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(text, "one two three four five");
    }
}
