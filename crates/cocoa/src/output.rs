//! Cached Markdown presentation. Raw messages remain separate from displayed copy text.

use std::{
    ops::Range,
    sync::LazyLock,
    time::{Duration, Instant},
};

use pulldown_cmark::{CodeBlockKind, Event, Options as ParseOptions, Parser, Tag};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use similar::{ChangeTag, TextDiff};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use tui_markdown::{Options, StyleSheet};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::Theme;

pub(crate) fn activity_diff(source: &str, theme: Theme) -> Text<'_> {
    let palette = theme.palette();
    Text::from(
        source
            .lines()
            .map(|line| {
                let color = if line.starts_with('+') {
                    palette.success
                } else if line.starts_with('-') {
                    palette.error
                } else {
                    palette.muted
                };
                Line::styled(line, Style::new().fg(color))
            })
            .collect::<Vec<_>>(),
    )
}

pub(crate) fn web_links(text: &str) -> Vec<Range<usize>> {
    let mut links = Vec::new();
    let mut cursor = 0;
    let mut lowercase = text.match_indices('h').peekable();
    let mut uppercase = text.match_indices('H').peekable();
    loop {
        // Single-byte searches skip UTF-8 decoding across large messages without links.
        let next = match (lowercase.peek(), uppercase.peek()) {
            (_, None) => lowercase.next(),
            (Some(lower), Some(upper)) if lower.0 < upper.0 => lowercase.next(),
            _ => uppercase.next(),
        };
        let Some((start, _)) = next else { break };
        if start < cursor {
            continue;
        }
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_alphanumeric() || matches!(character, '_' | '/'))
        {
            continue;
        }
        let remaining = &text[start..];
        if !remaining
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
            && !remaining
                .get(..7)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        {
            continue;
        }
        let length = remaining
            .find(|character: char| {
                character.is_whitespace()
                    || character.is_control()
                    || matches!(character, '<' | '>' | '"' | '\'' | '`' | '│' | '|')
            })
            .unwrap_or(remaining.len());
        cursor = start + length;
        let destination = trim_url(&remaining[..length]);
        if destination.len() > 2048 {
            continue;
        }
        if let Ok(url) = url::Url::parse(destination)
            && url.host_str().is_some()
        {
            links.push(start..start + destination.len());
        }
    }
    links
}

const CODE_CACHE_BYTES_MAX: usize = 256 * 1024;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Presentation cache for a transcript message.
#[derive(Default)]
pub(crate) struct Markdown {
    source: String,
    width: u16,
    theme: Theme,
    rendered: Rendered,
    code_blocks: Vec<Option<CachedCodeBlock>>,
}

#[derive(Default)]
pub(crate) struct Rendered {
    text: Text<'static>,
    /// Visible logical lines before wrapping, used for selection and copying.
    pub plain: String,
}

impl Markdown {
    /// Reuse parsing and highlighting until source, width or theme changes.
    pub fn render(&mut self, source: &str, width: u16, theme: Theme) -> &Rendered {
        if self.width != width || self.source != source || self.theme != theme {
            self.source.clear();
            self.source.push_str(source);
            self.width = width;
            if self.theme != theme {
                self.code_blocks.clear();
            }
            self.theme = theme;
            self.rendered = render(source, width, theme, &mut self.code_blocks);
        }
        &self.rendered
    }
}

impl Rendered {
    /// Borrow styled spans without copying their content on animation frames.
    pub fn text(&self) -> Text<'_> {
        Text::from(
            self.text
                .lines
                .iter()
                .map(|line| {
                    Line::from(
                        line.spans
                            .iter()
                            .map(|span| Span::styled(span.content.as_ref(), span.style))
                            .collect::<Vec<_>>(),
                    )
                    .style(line.style)
                })
                .collect::<Vec<_>>(),
        )
    }
}

#[derive(Clone)]
struct Styles(Theme);

impl StyleSheet for Styles {
    fn heading(&self, _level: u8) -> Style {
        Style::new()
            .fg(self.0.palette().foreground)
            .add_modifier(Modifier::BOLD)
    }

    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }

    fn code(&self) -> Style {
        Style::new().fg(self.0.palette().accent)
    }

    fn link(&self) -> Style {
        Style::new()
            .fg(self.0.palette().accent)
            .add_modifier(Modifier::UNDERLINED)
    }

    fn code_block_fence(&self) -> &str {
        ""
    }

    fn table_border(&self) -> Style {
        Style::new().fg(self.0.palette().muted)
    }
}

struct CodeBlock {
    range: Range<usize>,
    language: String,
    code: String,
}

struct CachedCodeBlock {
    language: String,
    code: String,
    lines: Vec<Line<'static>>,
    bytes: usize,
}

struct Highlighted {
    lines: Vec<Line<'static>>,
    complete: bool,
}

fn render(
    raw_source: &str,
    width: u16,
    theme: Theme,
    cache: &mut Vec<Option<CachedCodeBlock>>,
) -> Rendered {
    let mut previous = std::mem::take(cache).into_iter();
    let mut cache_bytes = 0usize;
    let options = ParseOptions::ENABLE_STRIKETHROUGH
        | ParseOptions::ENABLE_TASKLISTS
        | ParseOptions::ENABLE_HEADING_ATTRIBUTES
        | ParseOptions::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | ParseOptions::ENABLE_SUPERSCRIPT
        | ParseOptions::ENABLE_SUBSCRIPT
        | ParseOptions::ENABLE_MATH
        | ParseOptions::ENABLE_FOOTNOTES
        | ParseOptions::ENABLE_DEFINITION_LIST
        | ParseOptions::ENABLE_GFM
        | ParseOptions::ENABLE_TABLES;
    let source = preserve_breaks(raw_source, options);
    let source = source.as_str();
    let parser = Parser::new_ext(source, options);
    // References and footnotes can span prose segments. Keep those documents whole.
    let mut whole = parser.reference_definitions().iter().next().is_some();
    let mut depth = 0usize;
    let mut code_blocks = Vec::<CodeBlock>::new();
    let mut tables = Vec::new();
    for (index, (event, range)) in parser.into_offset_iter().enumerate() {
        if index >= 16384 || depth >= 64 {
            return finish(Text::from(raw_source));
        }
        match event {
            Event::Start(tag) => {
                if let Tag::Table(columns) = &tag {
                    if depth != 0 || columns.len() > 16 {
                        return finish(Text::from(raw_source));
                    }
                    tables.push(range.clone());
                }
                whole |= matches!(tag, Tag::FootnoteDefinition(_));
                if depth == 0
                    && let Tag::CodeBlock(kind) = tag
                {
                    if code_blocks.len() >= 128 {
                        return finish(Text::from(raw_source));
                    }
                    let language = match kind {
                        CodeBlockKind::Fenced(info) => {
                            info.split_whitespace().next().unwrap_or("").to_owned()
                        }
                        CodeBlockKind::Indented => String::new(),
                    };
                    code_blocks.push(CodeBlock {
                        range,
                        language,
                        code: String::new(),
                    });
                }
                depth += 1;
            }
            Event::End(_) => {
                debug_assert!(depth > 0, "Markdown events close only open elements");
                depth -= 1;
            }
            Event::Text(text) if depth == 1 => {
                if let Some(block) = code_blocks.last_mut()
                    && block.range.contains(&range.start)
                {
                    block.code.push_str(&text);
                }
            }
            _ => {}
        }
    }
    let options = Options::new(Styles(theme)).table_width(width);
    for range in tables {
        let table = tui_markdown::from_str_with_options(&source[range], &options);
        if table.width() > usize::from(width) {
            return finish(Text::from(raw_source));
        }
    }
    if whole || code_blocks.is_empty() {
        return finish(tui_markdown::from_str_with_options(source, &options));
    }
    let mut lines = Vec::new();
    let mut start = 0;
    if code_blocks.iter().any(|block| {
        !block.language.is_empty() && !matches!(block.language.as_str(), "diff" | "patch")
    }) {
        LazyLock::force(&SYNTAXES);
        LazyLock::force(&THEMES);
    }
    let deadline = Instant::now() + Duration::from_millis(20);
    let mut diff_deadline = None;
    for block in code_blocks {
        let previous = previous.next().flatten();
        let prose =
            tui_markdown::from_str_with_options(&source[start..block.range.start], &options);
        append(&mut lines, prose.lines);
        let label = if block.language.is_empty() {
            "code"
        } else {
            &block.language
        };
        let mut code = vec![Line::styled(
            label.to_owned(),
            Style::new().fg(theme.palette().muted),
        )];
        let mut cached = None;
        let highlighted = if matches!(block.language.as_str(), "diff" | "patch") {
            diff(
                &block.code,
                *diff_deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(10)),
                theme,
            )
        } else {
            // EOF blocks can still grow or change fence interpretation while streaming.
            let stable = block.range.end < source.len();
            if let Some(entry) = previous
                && stable
                && entry.language == block.language
                && entry.code == block.code
            {
                if entry.bytes <= CODE_CACHE_BYTES_MAX - cache_bytes {
                    let lines = entry.lines.clone();
                    cache_bytes += entry.bytes;
                    cached = Some(entry);
                    lines
                } else {
                    entry.lines
                }
            } else {
                let highlighted = highlight(&block.code, &block.language, deadline, theme);
                if stable && highlighted.complete {
                    let bytes = code_cache_bytes(&block, &highlighted.lines);
                    if bytes <= CODE_CACHE_BYTES_MAX - cache_bytes {
                        cache_bytes += bytes;
                        cached = Some(CachedCodeBlock {
                            language: block.language,
                            code: block.code,
                            lines: highlighted.lines.clone(),
                            bytes,
                        });
                    }
                }
                highlighted.lines
            }
        };
        cache.push(cached);
        code.extend(highlighted);
        append(&mut lines, code);
        start = block.range.end;
    }
    let prose = tui_markdown::from_str_with_options(&source[start..], &options);
    append(&mut lines, prose.lines);
    finish(Text::from(lines))
}

fn code_cache_bytes(block: &CodeBlock, lines: &[Line<'_>]) -> usize {
    let mut bytes = size_of::<CachedCodeBlock>()
        .saturating_add(block.language.capacity())
        .saturating_add(block.code.capacity());
    for line in lines {
        bytes = bytes.saturating_add(size_of::<Line<'_>>());
        for span in &line.spans {
            bytes = bytes
                .saturating_add(size_of::<Span<'_>>())
                .saturating_add(span.content.len());
        }
    }
    bytes
}

fn preserve_breaks(source: &str, options: ParseOptions) -> String {
    let mut output = String::with_capacity(source.len());
    let mut start = 0;
    // Transcript copy and scroll positions retain model-authored line breaks.
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        if matches!(event, Event::SoftBreak) {
            output.push_str(&source[start..range.start]);
            output.push_str("\\\n");
            start = range.end;
        }
    }
    output.push_str(&source[start..]);
    output
}

fn append<'a>(lines: &mut Vec<Line<'a>>, mut next: Vec<Line<'a>>) {
    if !lines.is_empty() && !next.is_empty() {
        lines.push(Line::default());
    }
    lines.append(&mut next);
}

fn highlight(source: &str, language: &str, deadline: Instant, theme: Theme) -> Highlighted {
    let Some(syntax) = SYNTAXES.find_syntax_by_token(language) else {
        return Highlighted {
            lines: Text::from(source.to_owned()).lines,
            complete: true,
        };
    };
    let Some(theme) = theme
        .syntax_theme()
        .and_then(|name| THEMES.themes.get(name))
    else {
        return Highlighted {
            lines: Text::from(source.to_owned()).lines,
            complete: true,
        };
    };
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    let mut complete = true;
    for (index, line) in LinesWithEndings::from(source).enumerate() {
        if index >= 512 || line.len() > 4096 || Instant::now() >= deadline {
            complete = false;
            lines.extend(
                source
                    .lines()
                    .skip(index)
                    .map(|line| Line::raw(line.to_owned())),
            );
            break;
        }
        match highlighter.highlight_line(line, &SYNTAXES) {
            Ok(tokens) => lines.push(Line::from(
                tokens
                    .into_iter()
                    .map(|(style, token)| {
                        let color = style.foreground;
                        let mut output = Style::new().fg(Color::Rgb(color.r, color.g, color.b));
                        for (font, modifier) in [
                            (FontStyle::BOLD, Modifier::BOLD),
                            (FontStyle::ITALIC, Modifier::ITALIC),
                            (FontStyle::UNDERLINE, Modifier::UNDERLINED),
                        ] {
                            if style.font_style.contains(font) {
                                output = output.add_modifier(modifier);
                            }
                        }
                        Span::styled(token.trim_end_matches('\n').to_owned(), output)
                    })
                    .collect::<Vec<_>>(),
            )),
            Err(error) => {
                complete = false;
                tracing::debug!(%error, "syntax highlighting failed; retaining literal code");
                lines.push(Line::raw(line.trim_end_matches('\n').to_owned()));
            }
        }
    }
    Highlighted { lines, complete }
}

fn diff(source: &str, deadline: Instant, theme: Theme) -> Vec<Line<'static>> {
    let palette = theme.palette();
    let Ok(patch) = diffy::Patch::from_str(source) else {
        return Text::from(source.to_owned()).lines;
    };
    if patch.hunks().is_empty() {
        return Text::from(source.to_owned()).lines;
    }
    let lines = source.lines().collect::<Vec<_>>();
    let mut output = Vec::with_capacity(lines.len());
    let mut index = 0;
    let mut in_hunk = false;
    while index < lines.len() {
        let line = lines[index];
        if line.starts_with("@@ ") {
            in_hunk = true;
        }
        if in_hunk
            && let Some(removed) = line.strip_prefix('-')
            && let Some(added) = lines.get(index + 1).and_then(|line| line.strip_prefix('+'))
            && !index
                .checked_sub(1)
                .is_some_and(|index| lines[index].starts_with('-'))
            && !lines
                .get(index + 2)
                .is_some_and(|line| line.starts_with('+'))
            && removed.len() + added.len() <= 4096
            && Instant::now() < deadline
        {
            let changes = TextDiff::configure()
                .deadline(deadline)
                .diff_graphemes(removed, added);
            let mut before = vec![Span::styled("-", Style::new().fg(palette.error))];
            let mut after = vec![Span::styled("+", Style::new().fg(palette.success))];
            for change in changes.iter_all_changes() {
                let value = change.value().to_owned();
                match change.tag() {
                    ChangeTag::Equal => {
                        before.push(Span::styled(value.clone(), Style::new().fg(palette.error)));
                        after.push(Span::styled(value, Style::new().fg(palette.success)));
                    }
                    ChangeTag::Delete => before.push(Span::styled(
                        value,
                        Style::new()
                            .fg(palette.error)
                            .bg(palette.surface)
                            .add_modifier(Modifier::BOLD),
                    )),
                    ChangeTag::Insert => after.push(Span::styled(
                        value,
                        Style::new()
                            .fg(palette.success)
                            .bg(palette.surface)
                            .add_modifier(Modifier::BOLD),
                    )),
                }
            }
            output.push(Line::from(before));
            output.push(Line::from(after));
            index += 2;
            continue;
        }
        let color = if !in_hunk || line.starts_with("@@ ") {
            palette.muted
        } else if line.starts_with('-') {
            palette.error
        } else if line.starts_with('+') {
            palette.success
        } else {
            palette.foreground
        };
        output.push(Line::styled(line.to_owned(), Style::new().fg(color)));
        index += 1;
    }
    output
}

fn finish(text: Text<'_>) -> Rendered {
    let text = owned(text);
    let plain = text.to_string();
    Rendered { text, plain }
}

fn owned(text: Text<'_>) -> Text<'static> {
    Text::from(
        text.lines
            .into_iter()
            .map(|line| {
                let mut content = String::new();
                let mut styles = Vec::with_capacity(line.spans.len());
                for span in line.spans {
                    // Markdown entities are decoded after the observation sanitizer runs.
                    content.extend(
                        span.content
                            .chars()
                            .filter(|character| !character.is_control()),
                    );
                    styles.push((content.len(), span.style));
                }
                let mut spans = Vec::<Span<'static>>::new();
                let mut index = 0;
                // Ratatui segments each span independently. Keep combining marks and ZWJ
                // sequences with their first character even when Markdown splits their styles.
                for (offset, grapheme) in content.grapheme_indices(true) {
                    while offset >= styles[index].0 {
                        index += 1;
                    }
                    let style = styles[index].1;
                    if let Some(span) = spans.last_mut()
                        && span.style == style
                    {
                        span.content.to_mut().push_str(grapheme);
                    } else {
                        spans.push(Span::styled(grapheme.to_owned(), style));
                    }
                }
                Line::from(spans).style(line.style)
            })
            .collect::<Vec<_>>(),
    )
}

fn trim_url(mut text: &str) -> &str {
    let mut parentheses = 0i32;
    let mut brackets = 0i32;
    for byte in text.bytes() {
        match byte {
            b'(' => parentheses += 1,
            b')' => parentheses -= 1,
            b'[' => brackets += 1,
            b']' => brackets -= 1,
            _ => {}
        }
    }
    while let Some(character) = text.chars().next_back() {
        match character {
            '.' | ',' | ';' | ':' | '!' | '?' => {}
            ')' if parentheses < 0 => parentheses += 1,
            ']' if brackets < 0 => brackets += 1,
            _ => break,
        }
        text = &text[..text.len() - character.len_utf8()];
    }
    text
}
