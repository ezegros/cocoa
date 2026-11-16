//! Bounded normalized matching with indivisible original-source units.

use std::{collections::HashSet, ops::Range};

use cocoa_core::capability::{CapabilityError, Effects};
use unicode_normalization::{
    UnicodeNormalization,
    char::{canonical_combining_class, compose},
};

use crate::{CodingContext, TextEdit, tool};

const TEXT_BYTES_MAX: usize = 8 * 1024 * 1024;
const SOURCE_UNITS_MAX: usize = 1024 * 1024;
const UNIT_BYTES_MAX: usize = 4096;
const CHECK_INTERVAL: usize = 1024;

pub(crate) fn apply(
    content: &str,
    edits: &[TextEdit],
    file_bytes_max: usize,
    context: &CodingContext,
) -> Result<String, CapabilityError> {
    context.check()?;
    if content.len() > TEXT_BYTES_MAX {
        return Err(capacity());
    }
    let normalized = Normalized::new(content, context)?;
    let ending = newline_style(content, context)?;
    let mut anchors = HashSet::with_capacity(edits.len());
    let mut matches = Vec::with_capacity(edits.len());
    let mut removed = 0usize;
    let mut added = 0usize;
    for edit in edits {
        context.check()?;
        let literal = unique_match(content, &edit.old_text, context)?;
        let anchor = Normalized::new(&edit.old_text, context)?.text;
        if anchor.is_empty() {
            return Err(tool(
                "invalid_arguments",
                "old_text must not be empty after normalization",
                Effects::None,
            ));
        }
        let found = unique_match(&normalized.text, &anchor, context)?.ok_or_else(|| {
            tool(
                "match_missing",
                "Normalized match does not occur",
                Effects::None,
            )
        })?;
        if !anchors.insert(anchor) {
            return Err(tool(
                "invalid_arguments",
                "edits must not contain duplicate normalized anchors",
                Effects::None,
            ));
        }
        let mapped = normalized.original_range(found)?;
        let range = if let Some(mut literal) = literal {
            // CRLF is one newline unit even when a literal starts at its LF
            // or ends at its CR. Newline equivalence consumes the whole unit.
            if content.as_bytes().get(literal.start) == Some(&b'\n')
                && literal.start > 0
                && content.as_bytes()[literal.start - 1] == b'\r'
            {
                literal.start -= 1;
            }
            if content.as_bytes().get(literal.end) == Some(&b'\n')
                && content.as_bytes()[literal.end - 1] == b'\r'
            {
                literal.end += 1;
            }
            if literal.start > mapped.start
                || literal.end < mapped.end
                || !normalized.is_original_boundary(literal.start)
                || !normalized.is_original_boundary(literal.end)
            {
                return Err(boundary());
            }
            literal
        } else {
            mapped
        };
        let replacement = replacement_newlines(&edit.new_text, ending, context)?;
        removed = removed
            .checked_add(range.len())
            .ok_or(CapabilityError::Budget)?;
        added = added
            .checked_add(replacement.len())
            .ok_or(CapabilityError::Budget)?;
        matches.push(MatchedEdit { range, replacement });
    }
    matches.sort_unstable_by_key(|matched| matched.range.start);
    for pair in matches.windows(2) {
        if pair[0].range.end > pair[1].range.start {
            return Err(tool(
                "match_overlap",
                "edits overlap in the original observation",
                Effects::None,
            ));
        }
    }
    let length = content
        .len()
        .checked_sub(removed)
        .and_then(|length| length.checked_add(added))
        .ok_or(CapabilityError::Budget)?;
    if length > file_bytes_max {
        return Err(tool("capacity", "File byte limit exceeded", Effects::None));
    }
    context.check()?;
    let mut updated = String::with_capacity(length);
    let mut offset = 0;
    for matched in matches {
        append_checked(&mut updated, &content[offset..matched.range.start], context)?;
        append_checked(&mut updated, &matched.replacement, context)?;
        offset = matched.range.end;
    }
    append_checked(&mut updated, &content[offset..], context)?;
    Ok(updated)
}

struct MatchedEdit {
    range: Range<usize>,
    replacement: String,
}

struct Normalized {
    text: String,
    units: Vec<SourceUnit>,
    line_content_end: usize,
}

struct SourceUnit {
    normalized_start: u32,
    normalized_end: u32,
    original_start: u32,
    original_end: u32,
}

impl Normalized {
    fn new(original: &str, context: &CodingContext) -> Result<Self, CapabilityError> {
        if original.len() > TEXT_BYTES_MAX {
            return Err(capacity());
        }
        let mut result = Self {
            text: String::with_capacity(original.len()),
            units: Vec::with_capacity(original.len().min(SOURCE_UNITS_MAX)),
            line_content_end: 0,
        };
        let mut characters = original.char_indices().peekable();
        let mut start = 0;
        let mut steps = 0;
        while let Some((index, character)) = characters.next() {
            if steps % CHECK_INTERVAL == 0 {
                context.check()?;
            }
            steps += 1;
            if matches!(character, '\r' | '\n') {
                result.finish_unit(original, start..index)?;
                let mut end = index + 1;
                if character == '\r' && characters.peek().is_some_and(|(_, next)| *next == '\n') {
                    characters.next();
                    end += 1;
                }
                result.trim_line(context)?;
                result.push_unit("\n".chars(), index..end)?;
                result.line_content_end = result.text.len();
                start = end;
                continue;
            }
            let first = character.nfkd().next().ok_or_else(boundary)?;
            if index > start && canonical_combining_class(first) == 0 {
                let unit: String = original[start..index].nfkc().collect();
                // A starter ends reordering, but Hangul and other class-zero
                // compositions can still join it to the preceding starter.
                let joins = unit.chars().next_back().is_some_and(|last| {
                    canonical_combining_class(last) == 0 && compose(last, first).is_some()
                });
                if !joins {
                    result.push_unit(unit.chars(), start..index)?;
                    start = index;
                }
            }
            if index + character.len_utf8() - start > UNIT_BYTES_MAX {
                return Err(capacity());
            }
        }
        result.finish_unit(original, start..original.len())?;
        result.trim_line(context)?;
        context.check()?;
        Ok(result)
    }

    fn original_range(&self, range: Range<usize>) -> Result<Range<usize>, CapabilityError> {
        let first = self
            .units
            .partition_point(|unit| unit.normalized_end as usize <= range.start);
        let end = self
            .units
            .partition_point(|unit| (unit.normalized_start as usize) < range.end);
        let first = self.units.get(first).ok_or_else(boundary)?;
        let last = end
            .checked_sub(1)
            .and_then(|index| self.units.get(index))
            .ok_or_else(boundary)?;
        if first.normalized_start as usize != range.start
            || last.normalized_end as usize != range.end
        {
            return Err(boundary());
        }
        Ok(first.original_start as usize..last.original_end as usize)
    }

    fn finish_unit(&mut self, original: &str, range: Range<usize>) -> Result<(), CapabilityError> {
        if !range.is_empty() {
            self.push_unit(original[range.clone()].nfkc(), range)?;
        }
        Ok(())
    }

    fn is_original_boundary(&self, offset: usize) -> bool {
        let index = self
            .units
            .partition_point(|unit| unit.original_end as usize <= offset);
        self.units
            .get(index)
            .is_none_or(|unit| unit.original_start as usize >= offset)
    }

    fn push_unit(
        &mut self,
        normalized: impl Iterator<Item = char>,
        original: Range<usize>,
    ) -> Result<(), CapabilityError> {
        let start = self.text.len();
        for character in normalized {
            let character = pi_character(character);
            if self.text.len() + character.len_utf8() > TEXT_BYTES_MAX {
                return Err(capacity());
            }
            self.text.push(character);
            if !character.is_whitespace() {
                self.line_content_end = self.text.len();
            }
        }
        if self.text.len() > start {
            if self.units.len() == SOURCE_UNITS_MAX {
                return Err(capacity());
            }
            self.units.push(SourceUnit {
                normalized_start: start as u32,
                normalized_end: self.text.len() as u32,
                original_start: original.start as u32,
                original_end: original.end as u32,
            });
        }
        Ok(())
    }

    fn trim_line(&mut self, context: &CodingContext) -> Result<(), CapabilityError> {
        self.text.truncate(self.line_content_end);
        let mut removed = 0;
        while self
            .units
            .last()
            .is_some_and(|unit| unit.normalized_start as usize >= self.text.len())
        {
            if removed % CHECK_INTERVAL == 0 {
                context.check()?;
            }
            self.units.pop();
            removed += 1;
        }
        if let Some(last) = self.units.last_mut() {
            last.normalized_end = self.text.len() as u32;
        }
        Ok(())
    }
}

fn unique_match(
    content: &str,
    anchor: &str,
    context: &CodingContext,
) -> Result<Option<Range<usize>>, CapabilityError> {
    if anchor.is_empty() {
        return Err(tool(
            "invalid_arguments",
            "old_text must be nonempty",
            Effects::None,
        ));
    }
    let anchor = anchor.as_bytes();
    let mut prefixes = vec![0; anchor.len()];
    let mut matched = 0;
    for index in 1..anchor.len() {
        if index % CHECK_INTERVAL == 0 {
            context.check()?;
        }
        while matched > 0 && anchor[index] != anchor[matched] {
            matched = prefixes[matched - 1];
        }
        if anchor[index] == anchor[matched] {
            matched += 1;
        }
        prefixes[index] = matched;
    }
    let mut found = None;
    matched = 0;
    for (index, byte) in content.bytes().enumerate() {
        if index % CHECK_INTERVAL == 0 {
            context.check()?;
        }
        while matched > 0 && byte != anchor[matched] {
            matched = prefixes[matched - 1];
        }
        if byte == anchor[matched] {
            matched += 1;
        }
        if matched == anchor.len() {
            if found.is_some() {
                return Err(tool(
                    "match_ambiguous",
                    "Match occurs more than once after normalization or literally",
                    Effects::None,
                ));
            }
            found = Some(index + 1 - anchor.len()..index + 1);
            matched = prefixes[matched - 1];
        }
    }
    context.check()?;
    Ok(found)
}

fn pi_character(character: char) -> char {
    match character {
        '\u{2018}'..='\u{201b}' => '\'',
        '\u{201c}'..='\u{201f}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
        character => character,
    }
}

fn newline_style(content: &str, context: &CodingContext) -> Result<&'static str, CapabilityError> {
    for (index, byte) in content.bytes().enumerate() {
        if index % CHECK_INTERVAL == 0 {
            context.check()?;
        }
        match byte {
            b'\r' if content.as_bytes().get(index + 1) == Some(&b'\n') => return Ok("\r\n"),
            b'\r' => return Ok("\r"),
            b'\n' => return Ok("\n"),
            _ => {}
        }
    }
    Ok("\n")
}

fn replacement_newlines(
    content: &str,
    ending: &str,
    context: &CodingContext,
) -> Result<String, CapabilityError> {
    let mut result = String::with_capacity(content.len());
    let mut characters = content.chars().peekable();
    let mut steps = 0;
    while let Some(character) = characters.next() {
        if steps % CHECK_INTERVAL == 0 {
            context.check()?;
        }
        steps += 1;
        if matches!(character, '\r' | '\n') {
            if character == '\r' && characters.peek() == Some(&'\n') {
                characters.next();
            }
            result.push_str(ending);
        } else {
            result.push(character);
        }
    }
    Ok(result)
}

fn append_checked(
    destination: &mut String,
    mut source: &str,
    context: &CodingContext,
) -> Result<(), CapabilityError> {
    while !source.is_empty() {
        context.check()?;
        let end = source.floor_char_boundary(UNIT_BYTES_MAX.min(source.len()));
        destination.push_str(&source[..end]);
        source = &source[end..];
    }
    Ok(())
}

fn boundary() -> CapabilityError {
    tool(
        "match_boundary",
        "Match must cover complete normalized source units",
        Effects::None,
    )
}

fn capacity() -> CapabilityError {
    tool(
        "capacity",
        "Normalization limit exceeded: 8 MiB input/output, 1048576 source units, 4096 bytes per unit",
        Effects::None,
    )
}
