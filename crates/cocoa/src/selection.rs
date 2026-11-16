//! Mouse selection over logical text mapped to rendered terminal cells.

use std::{
    collections::VecDeque,
    num::NonZeroU16,
    ops::Range,
    time::{Duration, Instant},
};

use ratatui::{
    buffer::{Buffer, CellDiffOption, CellWidth},
    crossterm::event::{MouseButton, MouseEvent, MouseEventKind},
    layout::Rect,
    style::Style,
    text::{Line, Span, Text},
};
use ratatui_textarea::TextArea;
use unicode_segmentation::UnicodeSegmentation;

use crate::chat::Action;
use crate::output::web_links;

const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(400);

/// Locate a scalar column, clamping past-the-end columns to the byte length.
/// Counting UTF-8 prefixes in bulk leaves at most four scalars to decode individually.
pub(crate) fn byte_offset(mut text: &str, mut column: usize) -> usize {
    let mut offset = 0;
    while column > 4 && !text.is_empty() {
        let boundary = text.floor_char_boundary(column.min(text.len()));
        let (prefix, remaining) = text.split_at(boundary);
        column -= prefix.chars().count();
        offset += boundary;
        text = remaining;
    }
    offset
        + text
            .char_indices()
            .nth(column)
            .map_or(text.len(), |(index, _)| index)
}

#[derive(Default)]
pub(crate) struct Selection {
    map: SelectionMap,
    drag: Option<Drag>,
    click: Option<Click>,
}

#[derive(Default, PartialEq, Eq)]
pub(crate) struct SelectionMap {
    text: String,
    rows: Vec<SelectionRow>,
    links: Vec<Range<usize>>,
}

#[derive(PartialEq, Eq)]
struct SelectionRow {
    x: u16,
    y: u16,
    region: Region,
    cells: Vec<Range<usize>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Region {
    History,
    Composer,
}

struct Drag {
    origin: (u16, u16),
    region: Region,
    anchor: Range<usize>,
    head: Range<usize>,
    selected: bool,
    active: bool,
    click_count: Option<u8>,
}

struct Click {
    position: (u16, u16),
    released_at: Instant,
    count: u8,
}

/// Byte ranges of wrapped rows, shared by drawing and mouse selection.
/// Source changes rebuild from the first changed logical line; width changes rebuild all rows.
/// Text and source contain the same logical lines, with controls already removed.
#[derive(Default)]
pub(crate) struct WrapLayout {
    source: String,
    width: u16,
    lines: Vec<usize>,
    rows: Vec<Range<usize>>,
    links: Vec<Range<usize>>,
}

impl WrapLayout {
    pub(crate) fn update(&mut self, text: &Text<'_>, source: &str, width: u16) {
        if self.width == width && self.source == source {
            return;
        }
        let prefix = if self.width == width {
            let shared = self
                .source
                .bytes()
                .zip(source.bytes())
                .take_while(|(previous, current)| previous == current)
                .count();
            source.as_bytes()[..shared]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1)
        } else {
            0
        };
        let retained_lines = self.lines.partition_point(|offset| *offset < prefix);
        self.lines.truncate(retained_lines);
        self.rows
            .truncate(self.rows.partition_point(|range| range.start < prefix));
        self.source.truncate(prefix);
        self.source.push_str(&source[prefix..]);
        self.links = web_links(source);
        self.width = width;
        let mut offset = prefix;
        for line in text.lines.iter().skip(retained_lines) {
            self.lines.push(offset);
            wrap_line(line, width, |segments| {
                self.rows.push(
                    offset + segments.first().map_or(0, |segment| segment.range.start)
                        ..offset + segments.last().map_or(0, |segment| segment.range.end),
                );
            });
            offset += line
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>()
                + 1;
        }
    }

    pub(crate) fn height(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn render(&self, text: &Text<'_>, skipped: usize, area: Rect, buffer: &mut Buffer) {
        for (row, range) in self
            .rows
            .iter()
            .skip(skipped)
            .take(usize::from(area.height))
            .enumerate()
        {
            let index = self.lines.partition_point(|offset| *offset <= range.start) - 1;
            let line = &text.lines[index];
            let mut offset = self.lines[index];
            let mut x = area.x;
            for span in &line.spans {
                let start = range.start.saturating_sub(offset).min(span.content.len());
                let end = range.end.saturating_sub(offset).min(span.content.len());
                let visible = Span::styled(&span.content[start..end], line.style.patch(span.style));
                for grapheme in visible.styled_graphemes(text.style) {
                    let width = grapheme.symbol.cell_width();
                    if width == 0 || width > area.width {
                        continue;
                    }
                    buffer[(x, area.y + row as u16)]
                        .set_symbol(grapheme.symbol)
                        .set_style(grapheme.style);
                    x += width;
                }
                offset += span.content.len();
                if offset >= range.end {
                    break;
                }
            }
        }
    }
}

impl Selection {
    pub(crate) fn clear(&mut self) {
        self.drag = None;
        self.click = None;
    }

    pub(crate) fn invalidate(&mut self) {
        self.clear();
        self.map = SelectionMap::default();
    }

    pub(crate) fn mouse(&mut self, mouse: MouseEvent, now: Instant) -> Action {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let count = self
                    .click
                    .take()
                    .filter(|click| {
                        click.position == (mouse.column, mouse.row)
                            && now
                                .checked_duration_since(click.released_at)
                                .is_some_and(|elapsed| elapsed < MULTI_CLICK_INTERVAL)
                    })
                    .map_or(1, |click| click.count % 3 + 1);
                self.clear();
                if let Some((region, anchor)) = self.map.position(mouse.column, mouse.row, None) {
                    let expanded = match count {
                        2 => self.map.word(&anchor),
                        3 => self.map.paragraph_range(&anchor),
                        _ => None,
                    };
                    let selected = expanded.is_some();
                    let anchor = expanded.unwrap_or(anchor);
                    self.drag = Some(Drag {
                        origin: (mouse.column, mouse.row),
                        region,
                        head: anchor.clone(),
                        anchor,
                        selected,
                        active: true,
                        click_count: Some(count),
                    });
                }
                Action::Redraw
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                let Some(drag) = &mut self.drag else {
                    return Action::None;
                };
                if !drag.active {
                    return Action::None;
                }
                if drag.origin != (mouse.column, mouse.row) {
                    drag.selected = true;
                    drag.click_count = None;
                }
                if let Some((_, head)) =
                    self.map
                        .position(mouse.column, mouse.row, Some(drag.region))
                {
                    drag.head = head;
                }
                if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
                    drag.active = false;
                    let range = drag.range();
                    if !range.is_empty() {
                        if let Some(count) = drag.click_count {
                            self.click = Some(Click {
                                position: drag.origin,
                                released_at: now,
                                count,
                            });
                        }
                        if drag.selected {
                            return Action::Copy(self.map.text[range].to_owned());
                        }
                    }
                }
                Action::Redraw
            }
            _ => Action::None,
        }
    }

    pub(crate) fn render(&mut self, map: SelectionMap, buffer: &mut Buffer, style: Style) {
        if self.map != map {
            self.clear();
        }
        self.map = map;
        let Some(drag) = &self.drag else {
            return;
        };
        if !drag.selected {
            return;
        }
        let selected = drag.range();
        for row in &self.map.rows {
            for (column, range) in row.cells.iter().enumerate() {
                if !range.is_empty() && range.start < selected.end && range.end > selected.start {
                    buffer[(row.x + column as u16, row.y)].set_style(style);
                }
            }
        }
    }

    pub(crate) fn render_hyperlinks(&self, buffer: &mut Buffer) {
        let links = &self.map.links;
        let mut link_index = 0;
        for row in &self.map.rows {
            if row.region != Region::History {
                continue;
            }
            let mut column = 0;
            while column < row.cells.len() {
                let range = &row.cells[column];
                while link_index < links.len() && links[link_index].end <= range.start {
                    link_index += 1;
                }
                let Some(link) = links.get(link_index) else {
                    return;
                };
                let cell = &buffer[(row.x + column as u16, row.y)];
                if range.is_empty()
                    || range.start < link.start
                    || range.end > link.end
                    || cell.symbol() != &self.map.text[range.clone()]
                {
                    column += usize::from(cell.cell_width().max(1));
                    continue;
                }
                let start = column;
                let style = cell.style();
                let destination = &self.map.text[link.clone()];
                let mut content = format!("\x1b]8;id=cocoa-{};{destination}\x1b\\", link.start);
                while column < row.cells.len() {
                    let range = &row.cells[column];
                    let cell = &buffer[(row.x + column as u16, row.y)];
                    if range.is_empty()
                        || range.start < link.start
                        || range.end > link.end
                        || cell.style() != style
                        || cell.symbol() != &self.map.text[range.clone()]
                    {
                        break;
                    }
                    content.push_str(cell.symbol());
                    column += usize::from(cell.cell_width().max(1));
                }
                content.push_str("\x1b]8;;\x1b\\");
                if let Some(width) = NonZeroU16::new((column - start) as u16) {
                    // A self-contained run keeps OSC state closed even when Ratatui
                    // skips unchanged cells. ForcedWidth excludes escape bytes from layout.
                    for trailing in start + 1..column {
                        // These cells are skipped while linked, but must redraw when
                        // a later frame removes the link without changing their text.
                        buffer[(row.x + trailing as u16, row.y)]
                            .set_diff_option(CellDiffOption::AlwaysUpdate);
                    }
                    buffer[(row.x + start as u16, row.y)]
                        .set_symbol(&content)
                        .set_diff_option(CellDiffOption::ForcedWidth(width));
                }
            }
        }
    }
}

impl SelectionMap {
    /// Map visible wrapped rows while retaining the complete logical text for copying.
    pub(crate) fn paragraph(&mut self, layout: &WrapLayout, area: Rect, skipped: usize) {
        let base = self.append(&layout.source);
        self.links.extend(
            layout
                .links
                .iter()
                .map(|range| base + range.start..base + range.end),
        );
        for (row, range) in layout
            .rows
            .iter()
            .skip(skipped)
            .take(usize::from(area.height))
            .enumerate()
        {
            let mut offset = base + range.start;
            let mut cells = Vec::with_capacity(usize::from(area.width));
            for grapheme in layout.source[range.clone()].graphemes(true) {
                let end = offset + grapheme.len();
                let width = grapheme.cell_width();
                if width <= area.width {
                    cells.extend(std::iter::repeat_n(offset..end, usize::from(width)));
                }
                offset = end;
            }
            cells.resize(usize::from(area.width), offset..offset);
            self.rows.push(SelectionRow {
                x: area.x,
                y: area.y + row as u16,
                region: Region::History,
                cells,
            });
        }
    }

    pub(crate) fn composer(&mut self, composer: &TextArea<'_>, area: Rect, buffer: &mut Buffer) {
        let base = self.append(&composer.lines().join("\n"));
        if area.is_empty() {
            return;
        }
        let (top, left) = composer.scroll_offset();
        let mut line_index = 0;
        let mut line_offset = base;
        let mut previous_start = None;
        for row in 0..area.height {
            let screen_row = usize::from(top) + usize::from(row);
            let mut start = composer.screen_to_data(screen_row, 0);
            if previous_start == Some(start) {
                break;
            }
            previous_start = Some(start);
            let end = composer.screen_to_data(screen_row, usize::MAX);
            for line in &composer.lines()[line_index..start.0] {
                line_offset += line.len() + 1;
            }
            line_index = start.0;
            let line = &composer.lines()[line_index];
            let column_first = start.1.saturating_sub(1);
            let byte_first = byte_offset(line, column_first);
            let remaining = &line[byte_first..];
            let byte_end = remaining
                .char_indices()
                .nth(end.1 - column_first + 1)
                .map_or(remaining.len(), |(offset, _)| offset);
            let mut ranges = Vec::with_capacity(end.1 - column_first + 2);
            let mut offset = line_offset + byte_first;
            for grapheme in Span::raw(&remaining[..byte_end]).styled_graphemes(Style::default()) {
                let end = offset + grapheme.symbol.len();
                ranges.extend(std::iter::repeat_n(
                    offset..end,
                    grapheme.symbol.chars().count(),
                ));
                offset = end;
            }
            ranges.push(offset..offset);
            let start_index = start.1 - column_first;
            if left == 0
                && start.1 > 0
                && start.1 < end.1
                && &self.text[ranges[start_index].clone()] == " "
                && &self.text[ranges[start_index - 1].clone()] != " "
                && &self.text[ranges[start_index + 1].clone()] != " "
            {
                // The editor can wrap a separator onto the next row. Hide it, not draft whitespace.
                let y = area.y + row;
                for x in area.x..area.right() - 1 {
                    buffer[(x, y)] = buffer[(x + 1, y)].clone();
                }
                let trailing = &mut buffer[(area.right() - 1, y)];
                trailing.reset();
                trailing.set_style(composer.style());
                if composer.cursor() == start {
                    buffer[(area.x, y)].set_style(composer.cursor_style());
                }
                start.1 += 1;
            }
            let mut cells = Vec::with_capacity(usize::from(area.width));
            let mut previous_range = None;
            let mut column = 0;
            // TextArea's column lookup counts scalar widths, which differ from
            // rendered grapheme widths for combining marks and joined emoji.
            for range in &ranges[start.1 - column_first..end.1 - column_first] {
                if previous_range == Some(range) {
                    continue;
                }
                previous_range = Some(range);
                for _ in 0..self.text[range.clone()].cell_width() {
                    if column >= usize::from(left) && cells.len() < usize::from(area.width) {
                        cells.push(range.clone());
                    }
                    column += 1;
                }
                if cells.len() == usize::from(area.width) {
                    break;
                }
            }
            let offset = ranges[end.1 - column_first].start;
            cells.resize(usize::from(area.width), offset..offset);
            self.rows.push(SelectionRow {
                x: area.x,
                y: area.y + row,
                region: Region::Composer,
                cells,
            });
        }
    }

    pub(crate) fn exclude(&mut self, area: Rect) {
        self.rows
            .retain(|row| row.y < area.y || row.y >= area.bottom());
    }

    fn append(&mut self, text: &str) -> usize {
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        let base = self.text.len();
        self.text.push_str(text);
        base
    }

    fn word(&self, position: &Range<usize>) -> Option<Range<usize>> {
        if position.is_empty() {
            return None;
        }
        self.text
            .split_word_bound_indices()
            .find_map(|(start, word)| {
                let end = start + word.len();
                (start <= position.start
                    && position.start < end
                    && !word.chars().all(char::is_whitespace))
                .then_some(start..end)
            })
    }

    fn paragraph_range(&self, position: &Range<usize>) -> Option<Range<usize>> {
        if position.is_empty() {
            return None;
        }
        let mut paragraph = 0..0;
        let mut offset = 0;
        for line in self.text.split_inclusive('\n') {
            if line.trim().is_empty() {
                if paragraph.contains(&position.start) {
                    return Some(paragraph);
                }
                let next = offset + line.len();
                paragraph = next..next;
            } else {
                paragraph.end = offset + line.trim_end_matches('\n').len();
            }
            offset += line.len();
        }
        paragraph.contains(&position.start).then_some(paragraph)
    }

    fn position(&self, x: u16, y: u16, region: Option<Region>) -> Option<(Region, Range<usize>)> {
        let row = if let Some(region) = region {
            self.rows
                .iter()
                .filter(|row| row.region == region)
                .min_by_key(|row| row.y.abs_diff(y))?
        } else {
            self.rows
                .iter()
                .find(|row| row.y == y && x >= row.x && usize::from(x - row.x) < row.cells.len())?
        };
        let column = usize::from(x.saturating_sub(row.x)).min(row.cells.len().saturating_sub(1));
        row.cells
            .get(column)
            .map(|range| (row.region, range.clone()))
    }
}

impl Drag {
    fn range(&self) -> Range<usize> {
        self.anchor.start.min(self.head.start)..self.anchor.end.max(self.head.end)
    }
}

struct WrappedSegment<'a> {
    symbol: &'a str,
    range: Range<usize>,
}

fn wrap_line(line: &Line<'_>, width: u16, mut visit: impl FnMut(&[WrappedSegment<'_>])) {
    if width == 0 {
        return;
    }
    let width = usize::from(width);
    let mut row = Vec::new();
    let mut word = Vec::new();
    let mut whitespace = VecDeque::<WrappedSegment<'_>>::new();
    let mut row_width = 0;
    let mut word_width = 0;
    let mut whitespace_width = 0;
    let mut non_whitespace_previous = false;
    let mut count = 0;
    let mut offset = 0;
    for span in &line.spans {
        let ascii = span.content.is_ascii();
        let mut remaining = span.content.as_ref();
        let mut graphemes = remaining.graphemes(true);
        while !remaining.is_empty() {
            let capacity = width
                .saturating_sub(row_width + word_width + whitespace_width)
                .max(1);
            let symbol = if ascii {
                let prefix = &remaining[..remaining.len().min(capacity)];
                let end = if prefix.starts_with(' ') {
                    prefix.find(|character| character != ' ')
                } else {
                    prefix.find(' ')
                }
                .unwrap_or(prefix.len());
                &prefix[..end]
            } else if let Some(symbol) = graphemes.next() {
                symbol
            } else {
                break;
            };
            remaining = &remaining[symbol.len()..];
            let range = offset..offset + symbol.len();
            offset = range.end;
            let symbol_width = if ascii {
                symbol.len()
            } else {
                usize::from(symbol.cell_width())
            };
            if symbol_width > width {
                continue;
            }
            let is_whitespace = if ascii {
                symbol.starts_with(' ')
            } else {
                symbol == "\u{200b}"
                    || (symbol != "\u{a0}" && symbol.chars().all(char::is_whitespace))
            };
            if (non_whitespace_previous && is_whitespace)
                || (row.is_empty() && word_width + whitespace_width + symbol_width > width)
            {
                row.extend(whitespace.drain(..));
                row.append(&mut word);
                row_width += whitespace_width + word_width;
                whitespace_width = 0;
                word_width = 0;
            }
            if row_width >= width
                || (symbol_width > 0
                    && row_width + whitespace_width + word_width + symbol_width > width)
            {
                debug_assert!(row_width <= width, "wrapped rows must fit the viewport");
                count += 1;
                visit(&row);
                row.clear();
                let mut remaining = width.saturating_sub(row_width);
                row_width = 0;
                while let Some(segment) = whitespace.front_mut() {
                    let length = usize::from(segment.symbol.cell_width());
                    if length > remaining {
                        if segment.symbol.is_ascii() {
                            segment.symbol = &segment.symbol[remaining..];
                            segment.range.start += remaining;
                            whitespace_width -= remaining;
                        }
                        break;
                    }
                    whitespace_width -= length;
                    remaining -= length;
                    whitespace.pop_front();
                }
                if is_whitespace && whitespace.is_empty() {
                    continue;
                }
            }
            // Zero-width symbols affect word breaks but need no cell or buffer entry.
            if symbol_width > 0 {
                let segment = WrappedSegment { symbol, range };
                if is_whitespace {
                    whitespace_width += symbol_width;
                    whitespace.push_back(segment);
                } else {
                    word_width += symbol_width;
                    word.push(segment);
                }
            }
            non_whitespace_previous = !is_whitespace;
        }
    }
    debug_assert!(
        row_width + whitespace_width + word_width <= width,
        "the final wrapped row must fit the viewport"
    );
    row.extend(whitespace);
    row.append(&mut word);
    if !row.is_empty() || count == 0 {
        visit(&row);
    }
}
