//! Turns `App` state into styled lines (pure, testable) and draws them.
//!
//! Layout, top to bottom: result list, status bar, prompt, key help. On wide
//! terminals the list area is split: results left, preview of the selected
//! file right.

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use crossterm::queue;

use crate::app::{App, Hit, Mode};
use crate::content::ContentMatch;
use crate::matcher::Matcher;
use crate::preview::{Body, Preview};
use crate::tree::{Kind, TreeRow};

const PROMPT: &str = "> ";
const HELP: &str = "Tab mode  ^R regex  ^L tree  ^T preview  ^S mark  ^O open  ^Y copy  Enter select  Esc quit";
/// Rows used by prompt, status bar and help line.
const CHROME_ROWS: usize = 3;
/// Below this usable width the preview pane is hidden.
const MIN_SPLIT_WIDTH: usize = 60;
const SEPARATOR: char = '│';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Normal,
    Prompt,
    Info,
    Dim,
    Path,
    Match,
    Selected,
    SelectedMatch,
    Tab,
    ActiveTab,
    Badge,
    Error,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

pub type Line = Vec<Span>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub lines: Vec<Line>,
    /// Terminal cursor position (column, row) inside the prompt.
    pub cursor: (u16, u16),
}

type Cell = (char, Style);

/// Never write into the last column: some terminals wrap or scroll there.
fn usable_width(width: u16) -> usize {
    (width as usize).saturating_sub(1).max(1)
}

/// Rows available for the result list on a terminal `height` rows tall.
pub fn list_height(height: u16) -> usize {
    (height as usize).saturating_sub(CHROME_ROWS)
}

/// Which part of the screen a mouse event at (`col`, `row`) is over.
pub fn hit_test(app: &App, width: u16, height: u16, col: u16, row: u16) -> Hit {
    let (col, row) = (col as usize, row as usize);
    if row >= list_height(height) {
        return Hit::Other;
    }
    match split_widths(app, usable_width(width)) {
        Some((list, _)) if col > list => Hit::Preview,
        Some((list, _)) if col == list => Hit::Other,
        _ => Hit::List(row),
    }
}

/// `(list width, preview width)` when the preview pane is shown.
fn split_widths(app: &App, width: usize) -> Option<(usize, usize)> {
    if !app.show_preview() || width < MIN_SPLIT_WIDTH {
        return None;
    }
    let list = width / 2;
    Some((list, width - list - 1))
}

/// How many file lines the preview pane can show (below its header), or
/// `None` when the pane is hidden.
pub fn preview_rows(app: &App, width: u16, height: u16) -> Option<usize> {
    split_widths(app, usable_width(width))?;
    let rows = list_height(height).saturating_sub(1);
    (rows > 0).then_some(rows)
}

/// Builds the screen for a terminal of `width` x `height`. Also updates the
/// app's scroll position to fit the list height.
pub fn layout(app: &mut App, preview: Option<&Preview>, width: u16, height: u16) -> Frame {
    let width = usable_width(width);
    let list_height = list_height(height);
    let height = height as usize;
    app.adjust_scroll(list_height);

    let mut lines = Vec::with_capacity(height);
    let split = split_widths(app, width);
    let list_width = split.map_or(width, |(list, _)| list);
    let start = app.offset();
    for row in 0..list_height {
        let i = start + row;
        let selected = i == app.selected();
        let mut cells = if app.tree_view() {
            app.tree().rows.get(i).map(|r| tree_row(app, r, list_width))
        } else {
            match app.mode() {
                Mode::Files => app.file_matches().get(i).map(|m| {
                    file_row(&app.files()[m.idx], &m.positions, selected, app.is_marked(i), list_width)
                }),
                Mode::Content => {
                    app.content_matches().get(i).map(|m| content_row(m, selected, app.is_marked(i), list_width))
                }
            }
        }
        .unwrap_or_default();
        if let Some((list_width, preview_width)) = split {
            pad(&mut cells, list_width, Style::Normal);
            cells.push((SEPARATOR, Style::Dim));
            cells.extend(preview_row(preview, row, preview_width, app.matcher()));
        }
        lines.push(to_spans(&cells));
    }

    // Bottom rows: status, prompt, key help. When the terminal is too short
    // for all three, the status bar goes first, then the help; the prompt stays.
    let (prompt, cursor_col) = prompt_line(app, width);
    let help = to_spans(&fit_right(text_cells(HELP, Style::Dim), width));
    let mut chrome = vec![status_line(app, width), prompt, help];
    let keep = height.min(CHROME_ROWS);
    if keep < CHROME_ROWS {
        chrome.remove(0);
    }
    chrome.truncate(keep);
    let prompt_row = lines.len() + usize::from(keep == CHROME_ROWS);
    lines.extend(chrome);
    Frame { lines, cursor: (cursor_col as u16, prompt_row as u16) }
}

fn prompt_line(app: &App, width: usize) -> (Line, usize) {
    let query: Vec<char> = app.query().chars().collect();
    let prompt_len = PROMPT.chars().count();
    let avail = width.saturating_sub(prompt_len).max(1);
    // Scroll the query horizontally so the cursor stays visible.
    let start = (app.cursor() + 1).saturating_sub(avail);
    let visible: String = query.iter().skip(start).take(avail).collect();
    let before_cursor: String = query[start..app.cursor()].iter().collect();
    let line = vec![span(PROMPT, Style::Prompt), span(&visible, Style::Normal)];
    (line, prompt_len + str_width(&before_cursor))
}

fn status_line(app: &App, width: usize) -> Line {
    let (files_style, content_style) = match app.mode() {
        Mode::Files => (Style::ActiveTab, Style::Tab),
        Mode::Content => (Style::Tab, Style::ActiveTab),
    };
    let total = app.files().len();
    let scanning = if app.is_walking() { " (scanning...)" } else { "" };
    let info = match app.mode() {
        Mode::Files => format!("  {}/{} files{}", app.file_matches().len(), total, scanning),
        Mode::Content => {
            let targets = app.content_target_count();
            let scope = app.content_scope().map(|q| format!(" matching '{q}'")).unwrap_or_default();
            if app.content_search().is_none() {
                format!("  type to search inside {targets} files{scope}{scanning}")
            } else {
                let more = if app.content_truncated() { "+" } else { "" };
                let found = app.content_matches().len();
                format!("  {found}{more} matching lines in {targets} files{scope}{scanning}")
            }
        }
    };
    let mut cells = text_cells(" Files ", files_style);
    cells.extend(text_cells(" Content ", content_style));
    if app.regex() {
        cells.push((' ', Style::Normal));
        cells.extend(text_cells(" regex ", Style::Badge));
    }
    if app.marked_count() > 0 {
        cells.extend(text_cells(&format!("  {} marked", app.marked_count()), Style::Badge));
    }
    match (app.query_error(), app.flash()) {
        (Some(e), _) => cells.extend(text_cells(&format!("  invalid regex: {e}"), Style::Error)),
        (None, Some(flash)) => cells.extend(text_cells(&format!("  {flash}"), Style::Info)),
        (None, None) => cells.extend(text_cells(&info, Style::Info)),
    }
    to_spans(&fit_right(cells, width))
}

/// Row `row` of the preview pane: a header with the path, then numbered
/// file lines. In content mode the focus line is highlighted and every
/// occurrence of the query is marked.
fn preview_row(preview: Option<&Preview>, row: usize, width: usize, matcher: Option<&Matcher>) -> Vec<Cell> {
    let Some(p) = preview else { return Vec::new() };
    let mut cells = vec![(' ', Style::Normal)];
    let width = width.saturating_sub(1);
    let note = |text: &str| fit_right(text_cells(text, Style::Dim), width);

    if row == 0 {
        let mut header = match p.focus {
            Some(line) => format!("{}:{}", p.path, line),
            None => p.path.to_string(),
        };
        if let Body::Text { first_line, lines } = &p.body {
            if *first_line > 1 && !lines.is_empty() {
                header.push_str(&format!("  [{}-{}]", first_line, first_line + lines.len() - 1));
            }
        }
        cells.extend(fit_left(text_cells(&header, Style::Path), width));
        return cells;
    }
    let r = row - 1;
    match &p.body {
        Body::Binary if r == 0 => cells.extend(note("(binary file)")),
        Body::Unreadable(e) if r == 0 => cells.extend(note(&format!("(cannot read: {e})"))),
        Body::Text { first_line: 1, lines } if lines.is_empty() && r == 0 => cells.extend(note("(empty file)")),
        Body::Text { first_line, lines } => {
            let Some(line) = lines.get(r) else { return cells };
            let number = first_line + r;
            let gutter = (first_line + lines.len() - 1).to_string().len();
            let focused = p.focus == Some(number);
            let (base, hl) = row_styles(focused);
            let ranges = matcher.map(|m| m.find_all(line)).unwrap_or_default();

            let mut line_cells = text_cells(&format!("{number:>gutter$} "), if focused { base } else { Style::Dim });
            line_cells.extend(line.chars().enumerate().map(|(i, c)| {
                let hit = ranges.iter().any(|&(s, e)| (s..e).contains(&i));
                (c, if hit { hl } else { base })
            }));
            let mut fitted = fit_right(line_cells, width);
            if focused {
                pad(&mut fitted, width, Style::Selected);
            }
            cells.extend(fitted);
        }
        _ => {}
    }
    cells
}

fn file_row(path: &str, positions: &[usize], selected: bool, marked: bool, width: usize) -> Vec<Cell> {
    let (base, hl) = row_styles(selected);
    let mut cells = marker(selected, marked);
    let mut next = positions.iter().peekable();
    let path_cells: Vec<Cell> = path
        .chars()
        .enumerate()
        .map(|(i, c)| {
            let hit = next.peek() == Some(&&i);
            if hit {
                next.next();
            }
            (c, if hit { hl } else { base })
        })
        .collect();
    // Keep the end of long paths visible: the file name matters most.
    cells.extend(fit_left(path_cells, width.saturating_sub(cells_width(&cells))));
    finish_row(cells, selected, width)
}

fn content_row(m: &ContentMatch, selected: bool, marked: bool, width: usize) -> Vec<Cell> {
    let (base, hl) = row_styles(selected);
    let mut cells = marker(selected, marked);
    let avail = width.saturating_sub(cells.len());

    let prefix = format!("{}:{}: ", m.path, m.line);
    let prefix_style = if selected { Style::Selected } else { Style::Path };
    let prefix = fit_left(text_cells(&prefix, prefix_style), (avail / 2).max(1));
    let text_avail = avail.saturating_sub(prefix.len());
    cells.extend(prefix);
    cells.extend(match_text(m, base, hl, text_avail));
    finish_row(cells, selected, width)
}

/// The matched line's text in `width` cells, scrolled so the match is visible.
fn match_text(m: &ContentMatch, base: Style, hl: Style, width: usize) -> Vec<Cell> {
    let (start, end) = m.range;
    let mut text: Vec<Cell> = m
        .text
        .chars()
        .enumerate()
        .map(|(i, c)| (c, if (start..end).contains(&i) { hl } else { base }))
        .collect();
    if end > width && start > 0 {
        let skip = start.saturating_sub(4).min(text.len());
        text.drain(..skip);
        if let Some(first) = text.first_mut() {
            first.0 = '…';
        }
    }
    fit_right(text, width)
}

/// One row of the tree view: guides, then a directory, a file (highlighted
/// in file mode) or a matching line (content mode).
fn tree_row(app: &App, row: &TreeRow, width: usize) -> Vec<Cell> {
    let selected = row.result.is_some() && row.result == Some(app.selected());
    let marked = row.result.is_some_and(|i| app.is_marked(i));
    let (base, hl) = row_styles(selected);
    let mut cells = marker(selected, marked);
    cells.extend(text_cells(&row.guide, if selected { base } else { Style::Dim }));
    match (row.kind, row.result) {
        (Kind::Dir, _) => cells.extend(text_cells(&row.label, Style::Dir)),
        (Kind::File, Some(i)) => {
            let positions = &app.file_matches()[i].positions;
            cells.extend(row.label.chars().enumerate().map(|(j, c)| {
                let hit = positions.binary_search(&(row.name_start + j)).is_ok();
                (c, if hit { hl } else { base })
            }));
        }
        (Kind::File, None) => cells.extend(text_cells(&row.label, Style::Path)),
        (Kind::Line, Some(i)) => {
            let m = &app.content_matches()[i];
            cells.extend(text_cells(&format!("{}: ", m.line), if selected { base } else { Style::Dim }));
            let avail = width.saturating_sub(cells_width(&cells));
            cells.extend(match_text(m, base, hl, avail));
        }
        (Kind::Line, None) => {}
    }
    finish_row(fit_right(cells, width), selected, width)
}

fn row_styles(selected: bool) -> (Style, Style) {
    if selected {
        (Style::Selected, Style::SelectedMatch)
    } else {
        (Style::Normal, Style::Match)
    }
}

/// Two columns: `>` for the selected row, `•` for marked rows.
fn marker(selected: bool, marked: bool) -> Vec<Cell> {
    let style = if selected { Style::Selected } else { Style::Normal };
    let mark = if marked { ('\u{2022}', if selected { Style::Selected } else { Style::Badge }) } else { (' ', style) };
    vec![(if selected { '>' } else { ' ' }, style), mark]
}

/// Pads the selected row so its background spans the full width.
fn finish_row(mut cells: Vec<Cell>, selected: bool, width: usize) -> Vec<Cell> {
    if selected {
        pad(&mut cells, width, Style::Selected);
    }
    cells
}

/// Terminal columns taken by `c`: 0 for combining marks and zero-width
/// characters, 2 for East Asian wide characters and most emoji.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    let zero = matches!(cp,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A | 0x064B..=0x065F
        | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF
        | 0x200B..=0x200F | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0xE0100..=0xE01EF);
    if zero {
        return 0;
    }
    let wide = matches!(cp,
        0x1100..=0x115F | 0x231A..=0x231B | 0x2329..=0x232A | 0x23E9..=0x23EC | 0x23F0 | 0x23F3
        | 0x25FD..=0x25FE | 0x2614..=0x2615 | 0x2648..=0x2653 | 0x267F | 0x2693 | 0x26A1
        | 0x26AA..=0x26AB | 0x26BD..=0x26BE | 0x26C4..=0x26C5 | 0x26CE | 0x26D4 | 0x26EA
        | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD | 0x2705 | 0x270A..=0x270B | 0x2728 | 0x274C
        | 0x274E | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF | 0x2B1B..=0x2B1C
        | 0x2B50 | 0x2B55 | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF | 0xA960..=0xA97F | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F004 | 0x1F0CF | 0x1F18E
        | 0x1F191..=0x1F19A | 0x1F200..=0x1F251 | 0x1F300..=0x1F64F | 0x1F680..=0x1F6FF
        | 0x1F7E0..=0x1F7EB | 0x1F90C..=0x1F9FF | 0x1FA70..=0x1FAFF | 0x20000..=0x3FFFD);
    if wide {
        2
    } else {
        1
    }
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(|c| char_width(c.0)).sum()
}

/// Appends spaces until `cells` is `width` columns wide.
fn pad(cells: &mut Vec<Cell>, width: usize, style: Style) {
    let have = cells_width(cells);
    cells.extend(std::iter::repeat_n((' ', style), width.saturating_sub(have)));
}

fn span(text: &str, style: Style) -> Span {
    Span { text: text.to_string(), style }
}

fn text_cells(text: &str, style: Style) -> Vec<Cell> {
    text.chars().map(|c| (c, style)).collect()
}

/// Keeps the first `width` cells, marking a cut with `…`.
fn fit_right(cells: Vec<Cell>, width: usize) -> Vec<Cell> {
    if cells_width(&cells) <= width {
        return cells;
    }
    let mut out = Vec::new();
    let mut used = 0;
    for cell in cells {
        let w = char_width(cell.0);
        if used + w + 1 > width {
            if width > 0 {
                out.push(('\u{2026}', cell.1));
            }
            break;
        }
        used += w;
        out.push(cell);
    }
    out
}

/// Keeps the last `width` columns, marking a cut with `…`.
fn fit_left(cells: Vec<Cell>, width: usize) -> Vec<Cell> {
    if cells_width(&cells) <= width {
        return cells;
    }
    let mut out = Vec::new();
    let mut used = 0;
    for cell in cells.into_iter().rev() {
        let w = char_width(cell.0);
        if used + w + 1 > width {
            if width > 0 {
                out.push(('\u{2026}', cell.1));
            }
            break;
        }
        used += w;
        out.push(cell);
    }
    out.reverse();
    out
}

/// Merges runs of equally styled cells into spans.
fn to_spans(cells: &[Cell]) -> Line {
    let mut spans: Line = Vec::new();
    for &(c, style) in cells {
        match spans.last_mut() {
            Some(last) if last.style == style => last.text.push(c),
            _ => spans.push(Span { text: c.to_string(), style }),
        }
    }
    spans
}

fn apply_style(out: &mut impl Write, style: Style) -> io::Result<()> {
    queue!(out, SetAttribute(Attribute::Reset))?;
    match style {
        Style::Normal => {}
        Style::Prompt => queue!(out, SetForegroundColor(Color::Cyan), SetAttribute(Attribute::Bold))?,
        Style::Info => queue!(out, SetForegroundColor(Color::Yellow))?,
        Style::Dim | Style::Tab => queue!(out, SetForegroundColor(Color::DarkGrey))?,
        Style::Dir => queue!(out, SetForegroundColor(Color::Blue), SetAttribute(Attribute::Bold))?,
        Style::Error => queue!(out, SetForegroundColor(Color::Red))?,
        Style::Badge => queue!(
            out,
            SetBackgroundColor(Color::Yellow),
            SetForegroundColor(Color::Black),
            SetAttribute(Attribute::Bold)
        )?,
        Style::Path => queue!(out, SetForegroundColor(Color::Magenta))?,
        Style::Match => queue!(out, SetForegroundColor(Color::Green), SetAttribute(Attribute::Bold))?,
        Style::Selected => queue!(out, SetBackgroundColor(Color::DarkGrey), SetForegroundColor(Color::White))?,
        Style::SelectedMatch => queue!(
            out,
            SetBackgroundColor(Color::DarkGrey),
            SetForegroundColor(Color::Green),
            SetAttribute(Attribute::Bold)
        )?,
        Style::ActiveTab => queue!(
            out,
            SetBackgroundColor(Color::Cyan),
            SetForegroundColor(Color::Black),
            SetAttribute(Attribute::Bold)
        )?,
    }
    Ok(())
}

/// Writes a full frame, redrawing every row.
pub fn draw(out: &mut impl Write, frame: &Frame) -> io::Result<()> {
    queue!(out, BeginSynchronizedUpdate, Hide)?;
    for (y, line) in frame.lines.iter().enumerate() {
        queue!(out, MoveTo(0, y as u16))?;
        for s in line {
            apply_style(out, s.style)?;
            queue!(out, Print(&s.text))?;
        }
        queue!(out, SetAttribute(Attribute::Reset), Clear(ClearType::UntilNewLine))?;
    }
    queue!(out, MoveTo(frame.cursor.0, frame.cursor.1), Show, EndSynchronizedUpdate)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBatch;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Arc;

    fn text(line: &Line) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    fn styled(line: &Line, style: Style) -> String {
        line.iter().filter(|s| s.style == style).map(|s| s.text.as_str()).collect()
    }

    fn app(names: &[&str], query: &str) -> App {
        let mut app = App::new(Mode::Files, query);
        app.add_files(names.iter().map(|&n| Arc::from(n)).collect());
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn frame_fills_terminal_height() {
        let mut a = app(&["a", "b"], "");
        let f = layout(&mut a, None, 40, 10);
        assert_eq!(f.lines.len(), 10);
        assert_eq!(text(&f.lines[8]), "> ");
        assert!(text(&f.lines[7]).contains("2/2 files"));
        assert!(text(&f.lines[9]).starts_with("Tab mode"));
    }

    #[test]
    fn lines_never_exceed_width() {
        let long = "x/".repeat(100) + "file.txt";
        let mut a = app(&[&long, "short"], "file");
        a.finish_walk();
        for w in [5u16, 20, 80] {
            let f = layout(&mut a, None, w, 8);
            for line in &f.lines {
                assert!(text(line).chars().count() < w as usize, "w={w}: {:?}", text(line));
            }
        }
    }

    #[test]
    fn prompt_sits_above_help_at_the_bottom() {
        let mut a = app(&["a.txt", "b.txt"], "txt");
        let f = layout(&mut a, None, 40, 6);
        let rows: Vec<String> = f.lines.iter().map(|l| text(l).trim_end().to_string()).collect();
        assert!(rows[0].starts_with("> ") && rows[0].contains(".txt"), "results first: {rows:?}");
        assert!(rows[3].contains("files"), "status above prompt: {rows:?}");
        assert_eq!(rows[4], "> txt");
        assert!(rows[5].starts_with("Tab mode"));
        assert_eq!(f.cursor, (5, 4));
    }

    #[test]
    fn short_terminal_keeps_prompt() {
        let mut a = app(&["a"], "q");
        let rows = |f: &Frame| f.lines.iter().map(text).collect::<Vec<_>>();
        let f = layout(&mut a, None, 40, 2);
        assert_eq!(rows(&f)[0], "> q");
        assert!(rows(&f)[1].starts_with("Tab mode"));
        assert_eq!(f.cursor, (3, 0));
        let f = layout(&mut a, None, 40, 1);
        assert_eq!(rows(&f), vec!["> q"]);
        assert_eq!(f.cursor, (3, 0));
        assert!(layout(&mut a, None, 40, 0).lines.is_empty());
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let mut a = app(&["a"], "a");
        for (w, h) in [(0, 0), (1, 1), (3, 2), (2, 3)] {
            let f = layout(&mut a, None, w, h);
            assert!(f.lines.len() <= h as usize);
        }
    }

    #[test]
    fn highlights_matched_characters() {
        let mut a = app(&["foo/bar.rs"], "fbr");
        let f = layout(&mut a, None, 40, 6);
        let row = &f.lines[0];
        assert_eq!(text(row).trim_end(), "> foo/bar.rs");
        assert_eq!(styled(row, Style::SelectedMatch), "fbr");
    }

    #[test]
    fn selected_row_is_marked_and_padded() {
        let mut a = app(&["a.txt", "b.txt"], "");
        press(&mut a, KeyCode::Down);
        let f = layout(&mut a, None, 30, 6);
        assert_eq!(text(&f.lines[0]), "  a.txt");
        assert!(text(&f.lines[1]).starts_with("> b.txt"));
        assert_eq!(text(&f.lines[1]).chars().count(), 29);
    }

    #[test]
    fn long_paths_keep_file_name_visible() {
        let long = "very/long/directory/structure/that/goes/on/target_file.rs";
        let mut a = app(&[long], "");
        let f = layout(&mut a, None, 25, 5);
        let row = text(&f.lines[0]);
        assert!(row.trim_end().ends_with("target_file.rs"), "{row}");
        assert!(row.contains('…'));
    }

    #[test]
    fn list_scrolls_to_selection() {
        let names: Vec<String> = (0..30).map(|i| format!("f{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs, "");
        for _ in 0..12 {
            press(&mut a, KeyCode::Down);
        }
        let f = layout(&mut a, None, 20, 8); // 5 list rows
        let rows: Vec<String> = f.lines[0..5].iter().map(|l| text(l).trim_end().to_string()).collect();
        assert_eq!(rows, vec!["  f08", "  f09", "  f10", "  f11", "> f12"]);
    }

    #[test]
    fn prompt_cursor_position() {
        let mut a = app(&[], "héllo");
        press(&mut a, KeyCode::Left);
        let f = layout(&mut a, None, 40, 5);
        assert_eq!(text(&f.lines[3]), "> héllo");
        assert_eq!(f.cursor, (6, 3));
    }

    #[test]
    fn long_query_scrolls_to_cursor() {
        let q = "abcdefghijklmnopqrstuvwxyz";
        let mut a = app(&[], q);
        let f = layout(&mut a, None, 11, 5); // usable width 10, query area 8
        assert_eq!(text(&f.lines[3]), "> tuvwxyz");
        assert_eq!(f.cursor, (9, 3));
        assert!((f.cursor.0 as usize) < 10);
    }

    #[test]
    fn status_shows_mode_and_counts() {
        let mut a = app(&["a.rs", "b.md"], "rs");
        let f = layout(&mut a, None, 80, 5);
        assert_eq!(styled(&f.lines[2], Style::ActiveTab), " Files ");
        assert!(text(&f.lines[2]).contains("1/2 files (scanning...)"));

        press(&mut a, KeyCode::Tab);
        a.finish_walk();
        let f = layout(&mut a, None, 80, 5);
        assert_eq!(styled(&f.lines[2], Style::ActiveTab), " Content ");
        assert!(text(&f.lines[2]).contains("type to search inside 1 files matching 'rs'"));
        press(&mut a, KeyCode::Char('x'));
        let f = layout(&mut a, None, 80, 5);
        assert!(text(&f.lines[2]).contains("0 matching lines in 1 files matching 'rs'"));
    }

    #[test]
    fn content_mode_with_blank_query_shows_hint() {
        let mut a = App::new(Mode::Content, "");
        a.finish_walk();
        let f = layout(&mut a, None, 80, 5);
        assert!(text(&f.lines[2]).contains("type to search inside 0 files"));
    }

    fn content_app(m: ContentMatch) -> App {
        let mut a = App::new(Mode::Content, "x");
        let g = a.generation();
        a.add_content(ContentBatch { done: false, generation: g, matches: vec![m] });
        a
    }

    #[test]
    fn content_row_shows_location_and_highlight() {
        let mut a = content_app(ContentMatch {
            file_idx: 0,
            path: Arc::from("src/main.rs"),
            line: 42,
            text: "let needle = 1;".into(),
            range: (4, 10),
        });
        let f = layout(&mut a, None, 60, 5);
        let row = &f.lines[0];
        assert_eq!(text(row).trim_end(), "> src/main.rs:42: let needle = 1;");
        assert_eq!(styled(row, Style::SelectedMatch), "needle");
    }

    #[test]
    fn content_row_scrolls_long_line_to_match() {
        let line = format!("{}needle", "a".repeat(200));
        let mut a = content_app(ContentMatch {
            file_idx: 0,
            path: Arc::from("f.rs"),
            line: 1,
            text: line,
            range: (200, 206),
        });
        let f = layout(&mut a, None, 40, 5);
        let row = &f.lines[0];
        assert_eq!(styled(row, Style::SelectedMatch), "needle");
        assert!(text(row).chars().count() <= 39);
    }

    fn text_preview(path: &str, focus: Option<usize>, first_line: usize, lines: &[&str]) -> Preview {
        Preview {
            path: Arc::from(path),
            focus,
            start: first_line,
            height: lines.len(),
            body: Body::Text { first_line, lines: lines.iter().map(|l| l.to_string()).collect() },
        }
    }

    /// The part of a row right of the separator.
    fn pane(line: &Line) -> String {
        text(line).split_once(SEPARATOR).map(|(_, p)| p.to_string()).unwrap_or_default()
    }

    #[test]
    fn wide_terminal_shows_preview_pane() {
        let mut a = app(&["src/a.rs", "b.rs"], "");
        let p = text_preview("src/a.rs", None, 1, &["fn main() {", "}"]);
        let f = layout(&mut a, Some(&p), 81, 8);
        // Usable width 80: 40 list columns, separator, 39 preview columns.
        let row: Vec<char> = text(&f.lines[0]).chars().collect();
        assert_eq!(row[40], SEPARATOR);
        assert!(text(&f.lines[0]).starts_with("> src/a.rs"));
        assert_eq!(pane(&f.lines[0]), " src/a.rs");
        assert_eq!(pane(&f.lines[1]), " 1 fn main() {");
        assert_eq!(pane(&f.lines[2]), " 2 }");
        assert_eq!(pane(&f.lines[3]).trim(), "");
        // Every list row has the separator, even past the last result.
        for line in &f.lines[0..5] {
            assert!(text(line).contains(SEPARATOR));
        }
        for line in &f.lines[5..8] {
            assert!(!text(line).contains(SEPARATOR), "bottom rows are full width");
        }
    }

    #[test]
    fn narrow_terminal_or_ctrl_t_hides_preview() {
        let mut a = app(&["a.rs"], "");
        let p = text_preview("a.rs", None, 1, &["x"]);
        let f = layout(&mut a, Some(&p), 50, 6);
        assert!(f.lines.iter().all(|l| !text(l).contains(SEPARATOR)));
        assert_eq!(preview_rows(&a, 50, 6), None);

        assert_eq!(preview_rows(&a, 81, 10), Some(6));
        a.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        let f = layout(&mut a, Some(&p), 81, 6);
        assert!(f.lines.iter().all(|l| !text(l).contains(SEPARATOR)));
        assert_eq!(preview_rows(&a, 81, 10), None);
    }

    #[test]
    fn preview_rows_needs_room_for_a_line() {
        let a = app(&["a.rs"], "");
        assert_eq!(preview_rows(&a, 81, 5), Some(1));
        assert_eq!(preview_rows(&a, 81, 4), None);
    }

    #[test]
    fn content_preview_highlights_focus_line_and_query() {
        let mut a = content_app(ContentMatch {
            file_idx: 0,
            path: Arc::from("f.rs"),
            line: 3,
            text: "x and x".into(),
            range: (0, 1),
        });
        let p = text_preview("f.rs", Some(3), 2, &["ax", "x and x", "none"]);
        let f = layout(&mut a, Some(&p), 81, 8);
        assert_eq!(pane(&f.lines[0]), " f.rs:3  [2-4]");
        assert_eq!(pane(&f.lines[1]), " 2 ax");
        assert_eq!(styled(&f.lines[1], Style::Match), "x");
        // Focused line: highlighted background, both occurrences marked.
        assert!(pane(&f.lines[2]).starts_with(" 3 x and x"));
        assert_eq!(styled(&f.lines[2], Style::SelectedMatch), "xx");
        assert_eq!(pane(&f.lines[2]).chars().count(), 39, "focus line padded to pane width");
        assert_eq!(styled(&f.lines[3], Style::Match), "");
    }

    #[test]
    fn preview_notes_for_special_files() {
        let mut a = app(&["a"], "");
        let mut p = text_preview("a", None, 1, &[]);
        assert_eq!(pane(&layout(&mut a, Some(&p), 81, 6).lines[1]), " (empty file)");
        p.body = Body::Binary;
        assert_eq!(pane(&layout(&mut a, Some(&p), 81, 6).lines[1]), " (binary file)");
        p.body = Body::Unreadable("denied".into());
        assert_eq!(pane(&layout(&mut a, Some(&p), 81, 6).lines[1]), " (cannot read: denied)");
    }

    #[test]
    fn gutter_aligns_line_numbers() {
        let mut a = app(&["a"], "");
        let p = text_preview("a", None, 9, &["nine", "ten"]);
        let f = layout(&mut a, Some(&p), 81, 7);
        assert_eq!(pane(&f.lines[1]), "  9 nine");
        assert_eq!(pane(&f.lines[2]), " 10 ten");
    }

    #[test]
    fn preview_lines_never_exceed_width() {
        let long = "y".repeat(300);
        let mut a = app(&["a"], "");
        let p = text_preview(&"d/".repeat(100), Some(2), 1, &[&long, &long]);
        for w in [61u16, 81, 120] {
            let f = layout(&mut a, Some(&p), w, 6);
            for line in &f.lines {
                assert!(text(line).chars().count() < w as usize, "w={w}");
            }
        }
    }

    #[test]
    fn tree_view_rows() {
        let mut a = app(&["src/main.rs", "src/ui.rs", "README.md"], "");
        a.set_tree_view(true);
        press(&mut a, KeyCode::Down);
        let f = layout(&mut a, None, 40, 8);
        let rows: Vec<String> = f.lines[..4].iter().map(|l| text(l).trim_end().to_string()).collect();
        assert_eq!(rows, vec!["  src/", "  \u{251c}\u{2500} main.rs", "> \u{2514}\u{2500} ui.rs", "  README.md"]);
        assert_eq!(styled(&f.lines[0], Style::Dir), "src/");
    }

    #[test]
    fn tree_view_highlights_file_name_matches() {
        let mut a = app(&["src/main.rs"], "main");
        a.set_tree_view(true);
        let f = layout(&mut a, None, 40, 6);
        assert_eq!(styled(&f.lines[1], Style::SelectedMatch), "main");
    }

    #[test]
    fn tree_view_in_content_mode_shows_lines_under_files() {
        let mut a = content_app(ContentMatch {
            file_idx: 0,
            path: Arc::from("src/a.rs"),
            line: 7,
            text: "let x = 1;".into(),
            range: (4, 5),
        });
        a.set_tree_view(true);
        let f = layout(&mut a, None, 40, 7);
        let rows: Vec<String> = f.lines[..3].iter().map(|l| text(l).trim_end().to_string()).collect();
        assert_eq!(rows, vec!["  src/", "  \u{2514}\u{2500} a.rs", ">    \u{2514}\u{2500} 7: let x = 1;"]);
        assert_eq!(styled(&f.lines[2], Style::SelectedMatch), "x");
    }

    #[test]
    fn regex_badge_and_error_in_status() {
        let mut a = app(&["a"], "a(");
        let f = layout(&mut a, None, 80, 5);
        assert_eq!(styled(&f.lines[2], Style::Badge), "");
        a.set_regex(true);
        let f = layout(&mut a, None, 80, 5);
        assert_eq!(styled(&f.lines[2], Style::Badge), " regex ");
        assert!(styled(&f.lines[2], Style::Error).contains("invalid regex: unclosed group"));
    }

    #[test]
    fn marked_rows_and_count() {
        let mut a = app(&["a", "b", "c"], "");
        a.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let f = layout(&mut a, None, 40, 7);
        assert_eq!(text(&f.lines[0]).trim_end(), " \u{2022}a");
        assert_eq!(text(&f.lines[1]).trim_end(), "> b");
        assert!(text(&f.lines[4]).contains("1 marked"));
    }

    #[test]
    fn flash_message_in_status() {
        let mut a = app(&["a"], "");
        a.set_flash("copied: a");
        let f = layout(&mut a, None, 60, 5);
        assert!(text(&f.lines[2]).contains("copied: a"));
        assert!(!text(&f.lines[2]).contains("1/1 files"));
    }

    #[test]
    fn char_widths() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('ş'), 1);
        assert_eq!(char_width('漢'), 2);
        assert_eq!(char_width('한'), 2);
        assert_eq!(char_width('\u{1F600}'), 2);
        assert_eq!(char_width('\u{0301}'), 0, "combining accent");
        assert_eq!(str_width("e\u{0301}漢"), 3);
    }

    #[test]
    fn fit_respects_wide_chars() {
        let cells = text_cells("漢字漢字", Style::Normal);
        let s = |c: Vec<Cell>| c.into_iter().map(|c| c.0).collect::<String>();
        assert_eq!(s(fit_right(cells.clone(), 5)), "漢字\u{2026}");
        assert_eq!(s(fit_left(cells.clone(), 5)), "\u{2026}漢字");
        assert_eq!(s(fit_right(cells.clone(), 8)), "漢字漢字");
        assert_eq!(s(fit_right(cells, 2)), "\u{2026}");
    }

    #[test]
    fn wide_paths_never_exceed_width() {
        let mut a = app(&["文档/非常长的文件名称测试用例.txt", "😀/x.txt"], "");
        let p = text_preview("文档/x", None, 1, &["漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字"]);
        for w in [20u16, 61, 81] {
            let f = layout(&mut a, Some(&p), w, 6);
            for line in &f.lines {
                assert!(str_width(&text(line)) < w as usize, "w={w}: {:?}", text(line));
            }
        }
        // The separator stays in the same column on every row.
        let f = layout(&mut a, Some(&p), 81, 6);
        let col = |l: &Line| str_width(text(l).split(SEPARATOR).next().unwrap());
        assert_eq!(col(&f.lines[0]), col(&f.lines[1]));
    }

    #[test]
    fn prompt_cursor_counts_wide_chars() {
        let mut a = app(&[], "漢a");
        let f = layout(&mut a, None, 40, 4);
        assert_eq!(f.cursor.0, 2 + 3);
    }

    #[test]
    fn hit_test_regions() {
        let a = app(&["a"], "");
        // 81 wide: list 0..40, separator at 40, preview after; 6 high: 3 list rows.
        assert_eq!(hit_test(&a, 81, 6, 5, 1), Hit::List(1));
        assert_eq!(hit_test(&a, 81, 6, 40, 1), Hit::Other);
        assert_eq!(hit_test(&a, 81, 6, 60, 0), Hit::Preview);
        assert_eq!(hit_test(&a, 81, 6, 5, 4), Hit::Other, "prompt row");
        assert_eq!(hit_test(&a, 40, 6, 30, 2), Hit::List(2), "no preview when narrow");
    }

    #[test]
    fn scrolled_preview_shows_line_range() {
        let mut a = app(&["a"], "");
        let p = text_preview("a", None, 4, &["four", "five"]);
        let f = layout(&mut a, Some(&p), 81, 6);
        assert_eq!(pane(&f.lines[0]), " a  [4-5]");
    }

    #[test]
    fn fit_helpers() {
        let cells = text_cells("abcdef", Style::Normal);
        let s = |c: Vec<Cell>| c.into_iter().map(|c| c.0).collect::<String>();
        assert_eq!(s(fit_right(cells.clone(), 4)), "abc…");
        assert_eq!(s(fit_left(cells.clone(), 4)), "…def");
        assert_eq!(s(fit_left(cells.clone(), 10)), "abcdef");
        assert_eq!(s(fit_right(cells, 0)), "");
    }

    #[test]
    fn spans_merge_same_style() {
        let cells = vec![('a', Style::Normal), ('b', Style::Normal), ('c', Style::Match)];
        assert_eq!(to_spans(&cells), vec![span("ab", Style::Normal), span("c", Style::Match)]);
    }

    #[test]
    fn draw_writes_text_and_escape_codes() {
        let mut a = app(&["hello.txt"], "");
        let f = layout(&mut a, None, 30, 4);
        let mut buf = Vec::new();
        draw(&mut buf, &f).unwrap();
        let out = String::from_utf8_lossy(&buf);
        assert!(out.contains("hello.txt"));
        assert!(out.contains("\x1b["));
    }
}
