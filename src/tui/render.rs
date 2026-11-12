use super::Entry;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL: Duration = Duration::from_millis(80);

pub(super) fn spinner_frame(elapsed: Duration) -> usize {
    (elapsed.as_millis() / SPINNER_INTERVAL.as_millis()) as usize % SPINNER_FRAMES.len()
}

pub(super) fn spinner_glyph(frame: usize) -> &'static str {
    SPINNER_FRAMES[frame]
}

pub(super) fn assistant_entry(source: String, width: usize) -> Entry {
    let rendered = render_markdown(&source, width);
    Entry::Assistant {
        source,
        rendered,
        width,
    }
}

pub(super) fn render_markdown(markdown: &str, width: usize) -> Vec<Line<'static>> {
    let lines = render_markdown_with(markdown, width, false);
    if lines
        .iter()
        .any(|line| box_line(line) && line.width() > width)
    {
        return render_markdown_with(markdown, width, true);
    }
    lines
}

pub(super) fn render_markdown_with(
    markdown: &str,
    width: usize,
    force: bool,
) -> Vec<Line<'static>> {
    let markdown = mobile_tables(markdown, width, force);
    let palette = super::theme::palette();
    super::theme::markdown(&markdown)
        .lines
        .into_iter()
        .map(|line| {
            let mut spans = line
                .spans
                .into_iter()
                .map(|span| {
                    let style = if span.style.fg.is_some() {
                        span.style
                    } else if span.style.add_modifier.contains(Modifier::BOLD) {
                        span.style.fg(palette.strong)
                    } else if span.style.add_modifier.contains(Modifier::ITALIC) {
                        span.style.fg(palette.secondary)
                    } else {
                        span.style
                    };
                    Span::styled(span.content.into_owned(), style)
                })
                .collect::<Vec<_>>();
            if !spans.is_empty() {
                spans.insert(0, Span::raw("  "));
            }
            Line::from(spans).style(line.style)
        })
        .collect()
}

pub(super) fn box_line(line: &Line<'static>) -> bool {
    line.spans.iter().any(|span| {
        span.content
            .chars()
            .any(|character| matches!(character, '┌' | '│' | '├' | '└'))
    })
}

pub(super) fn split_quote(line: &str) -> (&str, &str) {
    let mut end = 0;
    let mut seen = false;
    for (index, byte) in line.bytes().enumerate() {
        match byte {
            b'>' => {
                seen = true;
                end = index + 1;
            }
            b' ' => {}
            _ => break,
        }
    }
    if seen {
        (&line[..end], &line[end..])
    } else {
        ("", line)
    }
}

pub(super) fn mobile_tables(markdown: &str, width: usize, force: bool) -> String {
    let lines = markdown.lines().collect::<Vec<_>>();
    let mut out = String::with_capacity(markdown.len());
    let mut index = 0;
    let mut fenced = false;
    while index < lines.len() {
        let fence = lines[index].trim_start();
        if fence.starts_with("```") || fence.starts_with("~~~") {
            fenced = !fenced;
            out.push_str(lines[index]);
            out.push('\n');
            index += 1;
            continue;
        }
        if fenced {
            out.push_str(lines[index]);
            out.push('\n');
            index += 1;
            continue;
        }
        let (prefix, rest) = split_quote(lines[index]);
        let header = table_cells(rest);
        let delimiter = lines
            .get(index + 1)
            .copied()
            .map(split_quote)
            .filter(|(line_prefix, _)| *line_prefix == prefix)
            .and_then(|(_, line_rest)| table_cells(line_rest))
            .filter(|cells| is_table_delimiter(cells));
        match (header, delimiter) {
            (Some(header), Some(delimiter)) if header.len() == delimiter.len() => {
                let mut end = index + 2;
                let mut rows = Vec::new();
                while let Some(line) = lines.get(end).copied() {
                    let (row_prefix, row_rest) = split_quote(line);
                    if row_prefix != prefix {
                        break;
                    }
                    match table_cells(row_rest) {
                        Some(cells) => {
                            rows.push(cells);
                            end += 1;
                        }
                        None => break,
                    }
                }
                let block = lines[index..end].join("\n");
                if rows.is_empty() || (!force && table_fits(&block, width)) {
                    out.push_str(&block);
                } else {
                    out.push_str(&stacked_table(prefix, &header, &rows));
                }
                out.push('\n');
                index = end;
            }
            _ => {
                out.push_str(lines[index]);
                out.push('\n');
                index += 1;
            }
        }
    }
    if !markdown.is_empty() && !markdown.ends_with('\n') {
        out.pop();
    }
    out
}

pub(super) fn table_cells(line: &str) -> Option<Vec<String>> {
    if !line.contains('|') {
        return None;
    }
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut characters = trimmed.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' && characters.peek() == Some(&'|') {
            cell.push('|');
            characters.next();
        } else if character == '|' {
            cells.push(cell.trim().to_string());
            cell.clear();
        } else {
            cell.push(character);
        }
    }
    cells.push(cell.trim().to_string());
    Some(cells)
}

pub(super) fn is_table_delimiter(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let dashes = cell.trim_matches(':');
            !dashes.is_empty() && dashes.chars().all(|character| character == '-')
        })
}

pub(super) fn table_fits(block: &str, width: usize) -> bool {
    let intrinsic = tui_markdown::from_str(block)
        .lines
        .iter()
        .map(|line| line.width())
        .max()
        .unwrap_or(0);
    intrinsic + 2 <= width
}

pub(super) fn stacked_table(prefix: &str, header: &[String], rows: &[Vec<String>]) -> String {
    let lead = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix} ")
    };
    let separator = if prefix.is_empty() {
        "\n\n".to_string()
    } else {
        format!("\n{prefix}\n")
    };
    let mut records = Vec::new();
    for row in rows {
        let mut fields = Vec::new();
        for (column, name) in header.iter().enumerate() {
            let cell = row.get(column).map(String::as_str).unwrap_or("");
            let break_line = if column + 1 < header.len() { "  " } else { "" };
            fields.push(format!("{lead}**{name}:** {cell}{break_line}"));
        }
        records.push(fields.join("\n"));
    }
    records.join(&separator)
}

pub(super) fn quote_marker(line: &Line<'static>) -> Option<String> {
    let content = line.spans.get(1)?.content.as_ref();
    if !content.contains('>')
        || !content
            .chars()
            .all(|character| character == '>' || character == ' ')
    {
        return None;
    }
    let mut marker = content.trim_end().to_string();
    if line
        .spans
        .get(2)
        .is_some_and(|span| span.content.starts_with(' '))
    {
        marker.push(' ');
    }
    Some(marker)
}

pub(super) fn wrap_markdown_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let characters = line
        .spans
        .iter()
        .flat_map(|span| {
            span.content
                .chars()
                .map(move |character| (character, span.style))
        })
        .collect::<Vec<_>>();
    let total = characters
        .iter()
        .map(|(character, _)| char_columns(*character))
        .sum::<usize>();
    if total <= width || width == 0 {
        return vec![line.clone()];
    }
    let marker_width = line.spans.get(1).map_or(0, |span| {
        let marker = span.content.as_ref();
        let trimmed = marker.trim_start();
        let ordered = trimmed.split_once(". ").is_some_and(|(number, rest)| {
            rest.is_empty() && number.chars().all(|c| c.is_ascii_digit())
        });
        if marker.ends_with("- ") || marker.ends_with("] ") || ordered {
            columns(marker)
        } else {
            0
        }
    });
    let mut continuation = match quote_marker(line) {
        Some(marker) => format!("  {marker}"),
        None => " ".repeat(2 + marker_width),
    };
    let limit = width.saturating_sub(1);
    if columns(&continuation) > limit {
        continuation = take_columns(&continuation, limit);
    }
    let mut lines = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < characters.len() {
        let prefix = if first { "" } else { continuation.as_str() };
        let capacity = width.saturating_sub(columns(prefix)).max(1);
        let mut end = start;
        let mut used = 0;
        while end < characters.len() {
            let size = char_columns(characters[end].0);
            if end > start && used + size > capacity {
                break;
            }
            used += size;
            end += 1;
        }
        if end < characters.len()
            && let Some(offset) = characters[start..end]
                .iter()
                .rposition(|(character, _)| character.is_whitespace())
            && offset > 0
        {
            end = start + offset;
        }
        let mut spans = Vec::new();
        if !prefix.is_empty() {
            spans.push(Span::raw(prefix.to_string()));
        }
        for (character, style) in &characters[start..end] {
            if spans.last().is_some_and(|span| span.style == *style) {
                spans.last_mut().unwrap().content.to_mut().push(*character);
            } else {
                spans.push(Span::styled(character.to_string(), *style));
            }
        }
        lines.push(Line::from(spans).style(line.style));
        start = end;
        while start < characters.len() && characters[start].0.is_whitespace() {
            start += 1;
        }
        first = false;
    }
    lines
}

/// Display columns a character occupies; zero for control characters.
pub(super) fn char_columns(character: char) -> usize {
    character.width().unwrap_or(0)
}

fn covered(at: usize, width: usize, first: usize, last: usize) -> bool {
    at + width.max(1) > first && at <= last
}

fn marked(style: Style, selected: bool) -> Style {
    if selected {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    }
}

/// Copy of `line` with the cells `first..=last` reversed.
pub(super) fn select(line: &Line<'static>, first: usize, last: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut at = 0;
    for span in &line.spans {
        let mut run = String::new();
        let mut selected = false;
        for character in span.content.chars() {
            let width = char_columns(character);
            let covers = covered(at, width, first, last);
            if !run.is_empty() && covers != selected {
                spans.push(Span::styled(
                    std::mem::take(&mut run),
                    marked(span.style, selected),
                ));
            }
            selected = covers;
            run.push(character);
            at += width;
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, marked(span.style, selected)));
        }
    }
    Line::from(spans).style(line.style)
}

/// Text of `line` under the cells `first..=last`.
pub(super) fn selected_text(line: &Line<'static>, first: usize, last: usize) -> String {
    let mut text = String::new();
    let mut at = 0;
    for span in &line.spans {
        for character in span.content.chars() {
            let width = char_columns(character);
            if covered(at, width, first, last) {
                text.push(character);
            }
            at += width;
        }
    }
    text
}

/// Drop the two-column gutter every transcript line starts with.
pub(super) fn strip_marker(text: &str) -> &str {
    for marker in ["┃ ", "● ", "├ ", "└ "] {
        if let Some(rest) = text.strip_prefix(marker) {
            return rest;
        }
    }
    text.strip_prefix("  ").unwrap_or(text)
}

/// Row and column of `cursor` under the wrap performed by [`wrap_input_line`].
pub(super) fn cursor_row_col(input: &str, cursor: usize, width: usize) -> (usize, usize) {
    let mut row = 0;
    let mut col = 0;
    for character in input[..cursor].chars() {
        if character == '\n' {
            row += 1;
            col = 0;
            continue;
        }
        let size = char_columns(character);
        if col + size > width && col > 0 {
            row += 1;
            col = 0;
        }
        col += size;
    }
    if let Some(character) = input[cursor..].chars().next()
        && character != '\n'
        && col + char_columns(character) > width
        && col > 0
    {
        row += 1;
        col = 0;
    }
    (row, col)
}

/// First composer row to draw so `cursor_row` stays inside `height` rows.
pub(super) fn input_scroll(cursor_row: usize, lines: usize, height: usize) -> usize {
    let cursor_row = cursor_row.min(lines.saturating_sub(1));
    (cursor_row + 1)
        .saturating_sub(height)
        .min(lines.saturating_sub(height))
}

/// Display columns a string occupies on screen (wide and zero-width aware).
pub(super) fn columns(text: &str) -> usize {
    text.chars().map(char_columns).sum()
}

/// Keep at most `width` display columns of `text`.
pub(super) fn take_columns(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let size = char_columns(character);
        if used + size > width {
            break;
        }
        result.push(character);
        used += size;
    }
    result
}

/// Shorten `text` to at most `width` display columns, ending with an ellipsis.
pub(super) fn truncate(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_string();
    }
    let mut result = take_columns(text, width.saturating_sub(1));
    result.push('…');
    result
}

/// Shorten `text` to at most `width` display columns, keeping the end.
pub(super) fn truncate_tail(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_string();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for character in text.chars().rev() {
        let size = char_columns(character);
        if used + size > width.saturating_sub(1) {
            break;
        }
        kept.push(character);
        used += size;
    }
    kept.reverse();
    let mut result = String::from("…");
    result.extend(kept);
    result
}

/// Label of a tool call, cut down to `width` columns.
pub(super) fn fit_label(label: &crate::app::ToolLabel, width: usize) -> String {
    if !label.keep_tail {
        return truncate(&label.text, width);
    }
    let Some((verb, rest)) = label.text.split_once(' ') else {
        return truncate(&label.text, width);
    };
    if columns(&label.text) <= width {
        return label.text.clone();
    }
    let head = columns(verb) + 1;
    if head >= width {
        return truncate_tail(&label.text, width);
    }
    format!("{verb} {}", truncate_tail(rest, width - head))
}

pub(super) fn elapsed(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

pub(super) fn format_tokens(tokens: usize) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else if tokens < 10_000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("{}k", tokens / 1000)
    }
}

pub(super) fn age(updated: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let elapsed = (now - updated).max(0) as u64;
    if elapsed < 60_000 {
        "now".into()
    } else if elapsed < 3_600_000 {
        format!("{}m", elapsed / 60_000)
    } else if elapsed < 86_400_000 {
        format!("{}h", elapsed / 3_600_000)
    } else {
        format!("{}d", elapsed / 86_400_000)
    }
}

pub(super) fn byte_at_column(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map_or(end, |(index, _)| start + index)
}

pub(super) fn wrap_input_line(line: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for character in line.chars() {
        let size = char_columns(character);
        if used + size > width && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(character);
        used += size;
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Terminals quote or backslash-escape a path dropped onto them; undo that so
/// the path can be read.
pub(super) fn dropped_image_path(text: &str) -> Option<String> {
    let text = text.trim();
    let unquoted = text
        .strip_prefix('\'')
        .and_then(|text| text.strip_suffix('\''))
        .or_else(|| {
            text.strip_prefix('"')
                .and_then(|text| text.strip_suffix('"'))
        })
        .unwrap_or(text)
        .replace("\\ ", " ");
    crate::image::is_image_path(&unquoted).then_some(unquoted)
}
