//! Path-only mentions. Discovery reads directory entries and ignore rules, never file bodies.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use ignore::{Walk, WalkBuilder};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, BorderType, Clear, Paragraph},
};
use ratatui_textarea::{CursorMove, TextArea};

use crate::selection::byte_offset;
use crate::theme::Theme;

const PATHS_MAX: usize = 100_000;
const INDEX_BYTES_MAX: usize = 8 * 1024 * 1024;
const PATH_BYTES_MAX: usize = 4096;
const MATCHES_MAX: usize = 32;
const VISIBLE_ROWS: usize = 6;

pub(crate) struct FileCompletion {
    root: PathBuf,
    walk: Option<Walk>,
    paths: Vec<String>,
    index_bytes: usize,
    visited: usize,
    limited: bool,
    unavailable: bool,
    mention: Option<Mention>,
    dismissed: Option<Mention>,
    matches: Vec<((usize, usize), usize)>,
    selected: usize,
    dirty: bool,
}

impl FileCompletion {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            walk: None,
            paths: Vec::new(),
            index_bytes: 0,
            visited: 0,
            limited: false,
            unavailable: false,
            mention: None,
            dismissed: None,
            matches: Vec::new(),
            selected: 0,
            dirty: false,
        }
    }

    pub(crate) fn update(&mut self, composer: &TextArea<'_>) -> bool {
        let mention = Mention::at_cursor(composer);
        if mention.is_some() && mention == self.dismissed {
            return false;
        }
        self.dismissed = None;
        if mention == self.mention {
            return false;
        }
        if mention.is_some() && self.mention.is_none() {
            self.paths.clear();
            self.index_bytes = 0;
            self.visited = 0;
            self.limited = false;
            self.unavailable = false;
            self.walk = Some(
                WalkBuilder::new(&self.root)
                    .require_git(false)
                    .max_depth(Some(32))
                    .build(),
            );
        }
        if mention.is_none() {
            self.walk = None;
        }
        self.mention = mention;
        self.matches.clear();
        self.selected = 0;
        self.dirty = true;
        true
    }

    pub(crate) fn tick(&mut self, composer: &TextArea<'_>) -> bool {
        let mut changed = self.update(composer);
        let slice = Instant::now();
        for _ in 0..2048 {
            let Some(walk) = &mut self.walk else { break };
            changed = true;
            if self.paths.len() >= PATHS_MAX
                || self.index_bytes >= INDEX_BYTES_MAX
                || self.visited >= PATHS_MAX * 2
            {
                self.limited = true;
                self.walk = None;
                break;
            }
            let Some(entry) = walk.next() else {
                self.walk = None;
                break;
            };
            self.visited += 1;
            match entry {
                Ok(entry) => {
                    self.unavailable |= entry.error().is_some();
                    let kind = entry.file_type();
                    self.limited |= entry.depth() == 32 && kind.is_some_and(|kind| kind.is_dir());
                    if entry.depth() > 0
                        && kind.is_some_and(|kind| {
                            kind.is_file() || kind.is_dir() || kind.is_symlink()
                        })
                        && let Ok(relative) = entry.path().strip_prefix(&self.root)
                        && let Some(path) = relative.to_str()
                        && path.len() <= PATH_BYTES_MAX
                        && !path.chars().any(char::is_control)
                    {
                        let length =
                            path.len() + usize::from(kind.is_some_and(|kind| kind.is_dir()));
                        if self.index_bytes + length > INDEX_BYTES_MAX {
                            self.limited = true;
                            self.walk = None;
                            break;
                        }
                        let mut path = path.to_owned();
                        if kind.is_some_and(|kind| kind.is_dir()) {
                            path.push('/');
                        }
                        self.index_bytes += path.len();
                        self.paths.push(path);
                        self.dirty = true;
                    }
                }
                Err(_) => self.unavailable = true,
            }
            if slice.elapsed() >= Duration::from_millis(2) {
                break;
            }
        }
        self.search();
        changed
    }

    pub(crate) fn input(
        &mut self,
        key: KeyEvent,
        composer: &mut TextArea<'_>,
        budget: usize,
    ) -> bool {
        if self.mention.is_none()
            || !key.modifiers.is_empty()
            || !matches!(
                key.code,
                KeyCode::Esc | KeyCode::Up | KeyCode::Down | KeyCode::Enter | KeyCode::Tab
            )
        {
            return false;
        }
        self.search();
        match key.code {
            KeyCode::Esc => {
                self.dismissed = self.mention.take();
                self.walk = None;
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.matches.len().saturating_sub(1))
            }
            KeyCode::Enter | KeyCode::Tab => {
                let Some(mention) = &self.mention else {
                    return false;
                };
                let Some((_, index)) = self.matches.get(self.selected) else {
                    return true;
                };
                let path = &self.paths[*index];
                let drill = key.code == KeyCode::Tab && path.ends_with('/');
                let quoted = path
                    .chars()
                    .any(|character| character.is_whitespace() || matches!(character, '"' | '\\'));
                let mut replacement = String::from("@");
                if quoted {
                    replacement.push('"');
                }
                for character in path.chars() {
                    if quoted && matches!(character, '"' | '\\') {
                        replacement.push('\\');
                    }
                    replacement.push(character);
                }
                if quoted && !drill {
                    replacement.push('"');
                }
                let line = &composer.lines()[mention.row];
                let separator = line
                    .chars()
                    .nth(mention.end)
                    .is_some_and(char::is_whitespace);
                if !drill && !separator {
                    replacement.push(' ');
                }
                let bytes = composer.lines().iter().map(String::len).sum::<usize>()
                    + composer.lines().len()
                    - 1;
                let removed = line
                    .chars()
                    .skip(mention.start)
                    .take(mention.end - mention.start)
                    .map(char::len_utf8)
                    .sum::<usize>();
                if bytes - removed + replacement.len() > budget {
                    return true;
                }
                composer.move_cursor(CursorMove::Jump(mention.row as u16, mention.start as u16));
                composer.delete_str(mention.end - mention.start);
                composer.insert_str(replacement);
                if !drill && separator {
                    composer.move_cursor(CursorMove::Forward);
                }
                self.update(composer);
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn height(&self) -> u16 {
        if self.mention.is_none() {
            return 0;
        }
        self.matches.len().clamp(1, VISIBLE_ROWS) as u16 + 2
    }

    pub(crate) fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: Theme) {
        if self.mention.is_none() || area.is_empty() {
            return;
        }
        let palette = theme.palette();
        self.search();
        frame.render_widget(Clear, area);
        frame.render_widget(Block::new().style(theme.style()), area);
        let title = if self.walk.is_some() {
            " Files · searching "
        } else {
            " Files "
        };
        let notice = if self.limited {
            " File list limited "
        } else if self.unavailable {
            " Some paths unavailable "
        } else if area.width >= 56 {
            " ↑↓ select · Enter/Tab insert · Esc close "
        } else {
            " Esc close "
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(palette.border))
            .title(Line::styled(title, Style::new().fg(palette.muted)))
            .title_bottom(Line::styled(notice, Style::new().fg(palette.muted)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if self.matches.is_empty() {
            let text = if self.walk.is_some() {
                " Searching files…"
            } else {
                " No matching paths"
            };
            frame.render_widget(
                Paragraph::new(text).style(Style::new().fg(palette.muted)),
                inner,
            );
            return;
        }
        let visible = usize::from(inner.height);
        let start = self.selected.saturating_sub(visible.saturating_sub(1));
        for (row, (_, index)) in self.matches.iter().skip(start).take(visible).enumerate() {
            let selected = start + row == self.selected;
            let style = if selected {
                theme.selection_style()
            } else {
                Style::default()
            };
            let marker = if selected { "› " } else { "  " };
            frame.render_widget(
                Paragraph::new(format!("{marker}{}", self.paths[*index])).style(style),
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            );
        }
    }

    fn search(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(mention) = &self.mention else { return };
        let previous = self.matches.get(self.selected).map(|(_, index)| *index);
        self.matches.clear();
        let query = mention.query.strip_prefix("./").unwrap_or(&mention.query);
        let (directory, query) = query
            .rfind('/')
            .map_or(("", query), |index| (&query[..=index], &query[index + 1..]));
        for (index, path) in self.paths.iter().enumerate() {
            let Some(relative) = path
                .strip_prefix(directory)
                .filter(|relative| !relative.is_empty())
            else {
                continue;
            };
            let Some(score) = match_score(relative, query) else {
                continue;
            };
            let position = self
                .matches
                .binary_search_by(|(other_score, other)| {
                    (other_score, &self.paths[*other]).cmp(&(&score, path))
                })
                .unwrap_or_else(|position| position);
            if position < MATCHES_MAX {
                self.matches.insert(position, (score, index));
                self.matches.truncate(MATCHES_MAX);
            }
        }
        if self.selected > 0 {
            self.selected = self
                .matches
                .iter()
                .position(|(_, index)| Some(*index) == previous)
                .unwrap_or(0);
        }
    }
}

#[derive(PartialEq, Eq)]
struct Mention {
    row: usize,
    start: usize,
    end: usize,
    query: String,
}

impl Mention {
    fn at_cursor(composer: &TextArea<'_>) -> Option<Self> {
        let cursor = composer.cursor();
        let line = &composer.lines()[cursor.0];
        let offset = byte_offset(line, cursor.1);
        let before = &line[..offset];
        let (start, _) = before
            .rmatch_indices('@')
            .take_while(|(index, _)| before.len() - index - 1 <= PATH_BYTES_MAX)
            .find(|(index, _)| {
                before[..*index]
                    .chars()
                    .next_back()
                    .is_none_or(char::is_whitespace)
            })?;
        let token = &line[start + 1..];
        let typed = &before[start + 1..];
        let mut query = String::new();
        let mut escaped = false;
        let quoted = typed.starts_with('"');
        for character in typed.chars().skip(usize::from(quoted)) {
            if escaped {
                query.push(character);
                escaped = false;
            } else if quoted && character == '\\' {
                escaped = true;
            } else if (quoted && character == '"') || (!quoted && character.is_whitespace()) {
                return None;
            } else {
                query.push(character);
            }
        }
        let mut end = start + 1;
        let mut escaped = false;
        for (index, character) in token.char_indices() {
            if quoted && index == 0 {
                end += character.len_utf8();
                continue;
            }
            if !escaped && ((!quoted && character.is_whitespace()) || (quoted && character == '"'))
            {
                if quoted {
                    end += character.len_utf8();
                }
                break;
            }
            end += character.len_utf8();
            escaped = quoted && character == '\\' && !escaped;
        }
        Some(Self {
            row: cursor.0,
            start: line[..start].chars().count(),
            end: line[..end].chars().count(),
            query,
        })
    }
}

fn match_score(path: &str, query: &str) -> Option<(usize, usize)> {
    let mut characters = path.char_indices();
    let mut first = 0;
    let mut end = 0;
    for (matched, wanted) in query.chars().enumerate() {
        let (index, character) =
            characters.find(|(_, character)| character.eq_ignore_ascii_case(&wanted))?;
        if matched == 0 {
            first = index;
        }
        end = index + character.len_utf8();
    }
    Some((end.saturating_sub(first + query.len()), first))
}
