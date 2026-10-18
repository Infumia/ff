//! Draws `App` with ratatui. Rows are built cell by cell (pure, testable) so
//! matched characters can be highlighted and long paths cut on the left, then
//! rendered into the areas ratatui lays out.
//!
//! Layout, top to bottom: result list, status bar, prompt, key help. On wide
//! terminals the list area is split: results left, preview of the selected
//! file right.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

use crate::app::{App, Hit, Mode};
use crate::content::ContentMatch;
use crate::matcher::Matcher;
use crate::preview::{Body, Preview};
use crate::tree::{Kind, TreeRow};

const PROMPT: &str = "> ";
const HELP: &str = "Tab mode  ^R regex  ^L tree  ^T preview  ^S mark  ^O open  ^Y copy  Enter select  Esc quit";
/// Rows used by prompt, status bar and help line.
const CHROME_ROWS: u16 = 3;
/// Below this width the preview pane is hidden.
const MIN_SPLIT_WIDTH: u16 = 61;

const NORMAL: Style = Style::new();
const PROMPT_STYLE: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const INFO: Style = Style::new().fg(Color::Yellow);
const DIM: Style = Style::new().fg(Color::DarkGray);
const PATH: Style = Style::new().fg(Color::Magenta);
const MATCH: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
const SELECTED: Style = Style::new().fg(Color::White).bg(Color::DarkGray);
const SELECTED_MATCH: Style = Style::new().fg(Color::Green).bg(Color::DarkGray).add_modifier(Modifier::BOLD);
const ACTIVE_TAB: Style = Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD);
const BADGE: Style = Style::new().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD);
const ERROR: Style = Style::new().fg(Color::Red);
const DIR: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);

type Cell = (char, Style);

/// Where each part of the screen goes.
struct Areas {
    list: Rect,
    /// The preview pane, its separator included, when shown.
    preview: Option<Rect>,
    status: Option<Rect>,
    prompt: Option<Rect>,
    help: Option<Rect>,
}

/// When the terminal is too short for all three bottom rows, the status bar
/// goes first, then the help; the prompt stays.
fn areas(app: &App, area: Rect) -> Areas {
    let chrome = area.height.min(CHROME_ROWS);
    let [list, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(chrome)]).areas(area);
    let row = |i: u16| Rect { y: bottom.y + i, height: 1, ..bottom };
    let (status, prompt, help) = match chrome {
        3 => (Some(row(0)), Some(row(1)), Some(row(2))),
        2 => (None, Some(row(0)), Some(row(1))),
        1 => (None, Some(row(0)), None),
        _ => (None, None, None),
    };
    let (list, preview) = if app.show_preview() && list.width >= MIN_SPLIT_WIDTH {
        let [list, preview] =
            Layout::horizontal([Constraint::Length(list.width / 2), Constraint::Fill(1)]).areas(list);
        (list, Some(preview))
    } else {
        (list, None)
    };
    Areas { list, preview, status, prompt, help }
}

/// The preview pane's border on its left, a column of space, then the file.
fn preview_block() -> Block<'static> {
    Block::new().borders(Borders::LEFT).border_style(DIM).padding(Padding::left(1))
}

/// Rows available for the result list on a terminal `height` rows tall.
pub fn list_height(height: u16) -> usize {
    height.saturating_sub(CHROME_ROWS) as usize
}

/// Which part of the screen a mouse event at (`col`, `row`) is over.
pub fn hit_test(app: &App, width: u16, height: u16, col: u16, row: u16) -> Hit {
    let areas = areas(app, Rect::new(0, 0, width, height));
    if row >= areas.list.bottom() {
        return Hit::Other;
    }
    match areas.preview {
        Some(preview) if col > preview.x => Hit::Preview,
        Some(preview) if col == preview.x => Hit::Other,
        _ => Hit::List(row as usize),
    }
}

/// How many file lines the preview pane can show (below its header), or
/// `None` when the pane is hidden.
pub fn preview_rows(app: &App, width: u16, height: u16) -> Option<usize> {
    let preview = areas(app, Rect::new(0, 0, width, height)).preview?;
    let rows = preview_block().inner(preview).height.saturating_sub(1) as usize;
    (rows > 0).then_some(rows)
}

/// Draws the whole screen. Also updates the app's scroll position to fit the
/// list height.
pub fn render(app: &mut App, preview: Option<&Preview>, frame: &mut Frame) {
    let areas = areas(app, frame.area());
    app.adjust_scroll(areas.list.height as usize);
    let app = &*app;

    let width = areas.list.width as usize;
    let start = app.offset();
    let rows: Vec<Line> = (0..areas.list.height as usize)
        .map(|row| {
            let i = start + row;
            let selected = i == app.selected();
            let cells = if app.tree_view() {
                app.tree().rows.get(i).map(|r| tree_row(app, r, width))
            } else {
                match app.mode() {
                    Mode::Files => app.file_matches().get(i).map(|m| {
                        file_row(&app.files()[m.idx], &m.positions, selected, app.is_marked(i), width)
                    }),
                    Mode::Content => {
                        app.content_matches().get(i).map(|m| content_row(m, selected, app.is_marked(i), width))
                    }
                }
            };
            to_spans(&cells.unwrap_or_default())
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), areas.list);

    if let Some(area) = areas.preview {
        let block = preview_block();
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines: Vec<Line> = (0..inner.height as usize)
            .map(|row| to_spans(&preview_row(preview, row, inner.width as usize, app.matcher())))
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
    if let Some(area) = areas.status {
        frame.render_widget(status_line(app, area.width as usize), area);
    }
    if let Some(area) = areas.prompt {
        let (line, cursor) = prompt_line(app, area.width as usize);
        frame.render_widget(line, area);
        frame.set_cursor_position((area.x + cursor as u16, area.y));
    }
    if let Some(area) = areas.help {
        frame.render_widget(to_spans(&fit_right(text_cells(HELP, DIM), area.width as usize)), area);
    }
}

fn prompt_line(app: &App, width: usize) -> (Line<'static>, usize) {
    let query: Vec<char> = app.query().chars().collect();
    let prompt_len = PROMPT.chars().count();
    let avail = width.saturating_sub(prompt_len).max(1);
    // Scroll the query horizontally so the cursor stays visible.
    let start = (app.cursor() + 1).saturating_sub(avail);
    let visible: String = query.iter().skip(start).take(avail).collect();
    let before_cursor: String = query[start..app.cursor()].iter().collect();
    let line = Line::from(vec![Span::styled(PROMPT, PROMPT_STYLE), Span::styled(visible, NORMAL)]);
    (line, prompt_len + str_width(&before_cursor))
}

fn status_line(app: &App, width: usize) -> Line<'static> {
    let (files_style, content_style) = match app.mode() {
        Mode::Files => (ACTIVE_TAB, DIM),
        Mode::Content => (DIM, ACTIVE_TAB),
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
        cells.push((' ', NORMAL));
        cells.extend(text_cells(" regex ", BADGE));
    }
    if app.marked_count() > 0 {
        cells.extend(text_cells(&format!("  {} marked", app.marked_count()), BADGE));
    }
    match (app.query_error(), app.flash()) {
        (Some(e), _) => cells.extend(text_cells(&format!("  invalid regex: {e}"), ERROR)),
        (None, Some(flash)) => cells.extend(text_cells(&format!("  {flash}"), INFO)),
        (None, None) => cells.extend(text_cells(&info, INFO)),
    }
    to_spans(&fit_right(cells, width))
}

/// Row `row` of the preview pane: a header with the path, then numbered
/// file lines. In content mode the focus line is highlighted and every
/// occurrence of the query is marked.
fn preview_row(preview: Option<&Preview>, row: usize, width: usize, matcher: Option<&Matcher>) -> Vec<Cell> {
    let Some(p) = preview else { return Vec::new() };
    let note = |text: &str| fit_right(text_cells(text, DIM), width);

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
        return fit_left(text_cells(&header, PATH), width);
    }
    let r = row - 1;
    match &p.body {
        Body::Binary if r == 0 => note("(binary file)"),
        Body::Unreadable(e) if r == 0 => note(&format!("(cannot read: {e})")),
        Body::Text { first_line: 1, lines } if lines.is_empty() && r == 0 => note("(empty file)"),
        Body::Text { first_line, lines } => {
            let Some(line) = lines.get(r) else { return Vec::new() };
            let number = first_line + r;
            let gutter = (first_line + lines.len() - 1).to_string().len();
            let focused = p.focus == Some(number);
            let (base, hl) = row_styles(focused);
            let ranges = matcher.map(|m| m.find_all(line)).unwrap_or_default();

            let mut cells = text_cells(&format!("{number:>gutter$} "), if focused { base } else { DIM });
            cells.extend(line.chars().enumerate().map(|(i, c)| {
                let hit = ranges.iter().any(|&(s, e)| (s..e).contains(&i));
                (c, if hit { hl } else { base })
            }));
            let mut fitted = fit_right(cells, width);
            if focused {
                pad(&mut fitted, width, SELECTED);
            }
            fitted
        }
        _ => Vec::new(),
    }
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
    let prefix_style = if selected { SELECTED } else { PATH };
    let prefix = fit_left(text_cells(&prefix, prefix_style), (avail / 2).max(1));
    let text_avail = avail.saturating_sub(cells_width(&prefix));
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
    cells.extend(text_cells(&row.guide, if selected { base } else { DIM }));
    match (row.kind, row.result) {
        (Kind::Dir, _) => cells.extend(text_cells(&row.label, DIR)),
        (Kind::File, Some(i)) => {
            let positions = &app.file_matches()[i].positions;
            cells.extend(row.label.chars().enumerate().map(|(j, c)| {
                let hit = positions.binary_search(&(row.name_start + j)).is_ok();
                (c, if hit { hl } else { base })
            }));
        }
        (Kind::File, None) => cells.extend(text_cells(&row.label, PATH)),
        (Kind::Line, Some(i)) => {
            let m = &app.content_matches()[i];
            cells.extend(text_cells(&format!("{}: ", m.line), if selected { base } else { DIM }));
            let avail = width.saturating_sub(cells_width(&cells));
            cells.extend(match_text(m, base, hl, avail));
        }
        (Kind::Line, None) => {}
    }
    finish_row(fit_right(cells, width), selected, width)
}

fn row_styles(selected: bool) -> (Style, Style) {
    if selected {
        (SELECTED, SELECTED_MATCH)
    } else {
        (NORMAL, MATCH)
    }
}

/// Two columns: `>` for the selected row, `•` for marked rows.
fn marker(selected: bool, marked: bool) -> Vec<Cell> {
    let style = if selected { SELECTED } else { NORMAL };
    let mark = if marked { ('\u{2022}', if selected { SELECTED } else { BADGE }) } else { (' ', style) };
    vec![(if selected { '>' } else { ' ' }, style), mark]
}

/// Pads the selected row so its background spans the full width.
fn finish_row(mut cells: Vec<Cell>, selected: bool, width: usize) -> Vec<Cell> {
    if selected {
        pad(&mut cells, width, SELECTED);
    }
    cells
}

/// Terminal columns taken by `c`, as ratatui counts them: 0 for control and
/// combining characters, 2 for East Asian wide characters and most emoji.
fn char_width(c: char) -> usize {
    if c.is_control() {
        0
    } else {
        c.width().unwrap_or(0)
    }
}

fn str_width(s: &str) -> usize {
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

fn text_cells(text: &str, style: Style) -> Vec<Cell> {
    text.chars().map(|c| (c, style)).collect()
}

/// Keeps the first `width` columns, marking a cut with `…`.
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
fn to_spans(cells: &[Cell]) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();
    for &(c, style) in cells {
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push(c),
            _ => spans.push(Span::styled(c.to_string(), style)),
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBatch;
    use ratatui::backend::{Backend, TestBackend};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use std::sync::Arc;
    use unicode_width::UnicodeWidthStr;

    const SEPARATOR: char = '│';

    /// A drawn screen: its rows as text and as styled cells, and the cursor.
    struct Screen {
        rows: Vec<Vec<(String, Style)>>,
        cursor: (u16, u16),
    }

    impl Screen {
        fn text(&self, y: usize) -> String {
            self.rows[y].iter().map(|(s, _)| s.as_str()).collect()
        }

        fn trimmed(&self, y: usize) -> String {
            self.text(y).trim_end().to_string()
        }

        /// The characters of row `y` drawn in `style`.
        fn styled(&self, y: usize, style: Style) -> String {
            self.rows[y].iter().filter(|(_, s)| *s == style).map(|(s, _)| s.as_str()).collect()
        }

        /// The part of row `y` right of the separator.
        fn pane(&self, y: usize) -> String {
            self.text(y).split_once(SEPARATOR).map(|(_, p)| p.trim_end().to_string()).unwrap_or_default()
        }
    }

    fn draw(app: &mut App, preview: Option<&Preview>, width: u16, height: u16) -> Screen {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(app, preview, frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| {
                let mut row = Vec::new();
                let mut x = 0;
                // A wide character covers the cell after it: skip that one.
                while x < width {
                    let cell = &buffer[(x, y)];
                    let style = Style::new().fg(cell.fg).bg(cell.bg).add_modifier(cell.modifier);
                    row.push((cell.symbol().to_string(), style));
                    x += cell.symbol().width().max(1) as u16;
                }
                row
            })
            .collect();
        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        Screen { rows, cursor: (cursor.x, cursor.y) }
    }

    /// `style` as the buffer reports it: unset colors read back as `Reset`.
    fn drawn(style: Style) -> Style {
        Style::new()
            .fg(style.fg.unwrap_or(Color::Reset))
            .bg(style.bg.unwrap_or(Color::Reset))
            .add_modifier(style.add_modifier)
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
        let s = draw(&mut a, None, 40, 10);
        assert_eq!(s.trimmed(8), ">");
        assert!(s.text(7).contains("2/2 files"));
        assert!(s.text(9).starts_with("Tab mode"));
    }

    #[test]
    fn long_paths_fit_every_width() {
        let long = "x/".repeat(100) + "file.txt";
        let mut a = app(&[&long, "short"], "file");
        a.finish_walk();
        for w in [12u16, 20, 60] {
            let s = draw(&mut a, None, w, 8);
            assert!(s.trimmed(0).ends_with("file.txt"), "w={w}: {:?}", s.text(0));
            assert!(s.text(0).contains('…'));
        }
    }

    #[test]
    fn prompt_sits_above_help_at_the_bottom() {
        let mut a = app(&["a.txt", "b.txt"], "txt");
        let s = draw(&mut a, None, 40, 6);
        assert!(s.text(0).starts_with("> ") && s.text(0).contains(".txt"), "results first");
        assert!(s.text(3).contains("files"), "status above prompt");
        assert_eq!(s.trimmed(4), "> txt");
        assert!(s.text(5).starts_with("Tab mode"));
        assert_eq!(s.cursor, (5, 4));
    }

    #[test]
    fn short_terminal_keeps_prompt() {
        let mut a = app(&["a"], "q");
        let s = draw(&mut a, None, 40, 2);
        assert_eq!(s.trimmed(0), "> q");
        assert!(s.text(1).starts_with("Tab mode"));
        assert_eq!(s.cursor, (3, 0));
        let s = draw(&mut a, None, 40, 1);
        assert_eq!(s.trimmed(0), "> q");
        assert_eq!(s.cursor, (3, 0));
        draw(&mut a, None, 40, 0);
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let mut a = app(&["a"], "a");
        for (w, h) in [(0, 0), (1, 1), (3, 2), (2, 3)] {
            draw(&mut a, None, w, h);
        }
    }

    #[test]
    fn highlights_matched_characters() {
        let mut a = app(&["foo/bar.rs"], "fbr");
        let s = draw(&mut a, None, 40, 6);
        assert_eq!(s.trimmed(0), "> foo/bar.rs");
        assert_eq!(s.styled(0, drawn(SELECTED_MATCH)), "fbr");
    }

    #[test]
    fn selected_row_is_marked_and_padded() {
        let mut a = app(&["a.txt", "b.txt"], "");
        press(&mut a, KeyCode::Down);
        let s = draw(&mut a, None, 30, 6);
        assert_eq!(s.trimmed(0), "  a.txt");
        assert!(s.text(1).starts_with("> b.txt"));
        assert_eq!(s.rows[1].last().unwrap().1, drawn(SELECTED), "the background spans the row");
    }

    #[test]
    fn long_paths_keep_file_name_visible() {
        let long = "very/long/directory/structure/that/goes/on/target_file.rs";
        let mut a = app(&[long], "");
        let s = draw(&mut a, None, 25, 5);
        assert!(s.trimmed(0).ends_with("target_file.rs"), "{}", s.text(0));
        assert!(s.text(0).contains('…'));
    }

    #[test]
    fn list_scrolls_to_selection() {
        let names: Vec<String> = (0..30).map(|i| format!("f{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs, "");
        for _ in 0..12 {
            press(&mut a, KeyCode::Down);
        }
        let s = draw(&mut a, None, 20, 8); // 5 list rows
        let rows: Vec<String> = (0..5).map(|y| s.trimmed(y)).collect();
        assert_eq!(rows, vec!["  f08", "  f09", "  f10", "  f11", "> f12"]);
    }

    #[test]
    fn prompt_cursor_position() {
        let mut a = app(&[], "héllo");
        press(&mut a, KeyCode::Left);
        let s = draw(&mut a, None, 40, 5);
        assert_eq!(s.trimmed(3), "> héllo");
        assert_eq!(s.cursor, (6, 3));
    }

    #[test]
    fn long_query_scrolls_to_cursor() {
        let q = "abcdefghijklmnopqrstuvwxyz";
        let mut a = app(&[], q);
        let s = draw(&mut a, None, 11, 5); // query area 9
        assert_eq!(s.trimmed(3), "> stuvwxyz");
        assert_eq!(s.cursor, (10, 3));
    }

    #[test]
    fn status_shows_mode_and_counts() {
        let mut a = app(&["a.rs", "b.md"], "rs");
        let s = draw(&mut a, None, 80, 5);
        assert_eq!(s.styled(2, drawn(ACTIVE_TAB)), " Files ");
        assert!(s.text(2).contains("1/2 files (scanning...)"));

        press(&mut a, KeyCode::Tab);
        a.finish_walk();
        let s = draw(&mut a, None, 80, 5);
        assert_eq!(s.styled(2, drawn(ACTIVE_TAB)), " Content ");
        assert!(s.text(2).contains("type to search inside 1 files matching 'rs'"));
        press(&mut a, KeyCode::Char('x'));
        let s = draw(&mut a, None, 80, 5);
        assert!(s.text(2).contains("0 matching lines in 1 files matching 'rs'"));
    }

    #[test]
    fn content_mode_with_blank_query_shows_hint() {
        let mut a = App::new(Mode::Content, "");
        a.finish_walk();
        let s = draw(&mut a, None, 80, 5);
        assert!(s.text(2).contains("type to search inside 0 files"));
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
        let s = draw(&mut a, None, 60, 5);
        assert_eq!(s.trimmed(0), "> src/main.rs:42: let needle = 1;");
        assert_eq!(s.styled(0, drawn(SELECTED_MATCH)), "needle");
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
        let s = draw(&mut a, None, 40, 5);
        assert_eq!(s.styled(0, drawn(SELECTED_MATCH)), "needle");
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

    #[test]
    fn wide_terminal_shows_preview_pane() {
        let mut a = app(&["src/a.rs", "b.rs"], "");
        let p = text_preview("src/a.rs", None, 1, &["fn main() {", "}"]);
        let s = draw(&mut a, Some(&p), 80, 8);
        // 40 list columns, the separator, a space, then the file.
        let row: Vec<char> = s.text(0).chars().collect();
        assert_eq!(row[40], SEPARATOR);
        assert!(s.text(0).starts_with("> src/a.rs"));
        assert_eq!(s.pane(0), " src/a.rs");
        assert_eq!(s.pane(1), " 1 fn main() {");
        assert_eq!(s.pane(2), " 2 }");
        assert_eq!(s.pane(3), "");
        // Every list row has the separator, even past the last result.
        for y in 0..5 {
            assert!(s.text(y).contains(SEPARATOR));
        }
        for y in 5..8 {
            assert!(!s.text(y).contains(SEPARATOR), "bottom rows are full width");
        }
    }

    #[test]
    fn narrow_terminal_or_ctrl_t_hides_preview() {
        let mut a = app(&["a.rs"], "");
        let p = text_preview("a.rs", None, 1, &["x"]);
        let s = draw(&mut a, Some(&p), 50, 6);
        assert!((0..6).all(|y| !s.text(y).contains(SEPARATOR)));
        assert_eq!(preview_rows(&a, 50, 6), None);

        assert_eq!(preview_rows(&a, 80, 10), Some(6));
        a.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        let s = draw(&mut a, Some(&p), 80, 6);
        assert!((0..6).all(|y| !s.text(y).contains(SEPARATOR)));
        assert_eq!(preview_rows(&a, 80, 10), None);
    }

    #[test]
    fn preview_rows_needs_room_for_a_line() {
        let a = app(&["a.rs"], "");
        assert_eq!(preview_rows(&a, 80, 5), Some(1));
        assert_eq!(preview_rows(&a, 80, 4), None);
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
        let s = draw(&mut a, Some(&p), 80, 8);
        assert_eq!(s.pane(0), " f.rs:3  [2-4]");
        assert_eq!(s.pane(1), " 2 ax");
        assert_eq!(s.styled(1, drawn(MATCH)), "x");
        // Focused line: highlighted background, both occurrences marked.
        assert!(s.pane(2).starts_with(" 3 x and x"));
        assert_eq!(s.styled(2, drawn(SELECTED_MATCH)), "xx");
        assert_eq!(s.rows[2].last().unwrap().1, drawn(SELECTED), "focus line padded to pane width");
        assert_eq!(s.styled(3, drawn(MATCH)), "");
    }

    #[test]
    fn preview_notes_for_special_files() {
        let mut a = app(&["a"], "");
        let mut p = text_preview("a", None, 1, &[]);
        assert_eq!(draw(&mut a, Some(&p), 80, 6).pane(1), " (empty file)");
        p.body = Body::Binary;
        assert_eq!(draw(&mut a, Some(&p), 80, 6).pane(1), " (binary file)");
        p.body = Body::Unreadable("denied".into());
        assert_eq!(draw(&mut a, Some(&p), 80, 6).pane(1), " (cannot read: denied)");
    }

    #[test]
    fn gutter_aligns_line_numbers() {
        let mut a = app(&["a"], "");
        let p = text_preview("a", None, 9, &["nine", "ten"]);
        let s = draw(&mut a, Some(&p), 80, 7);
        assert_eq!(s.pane(1), "  9 nine");
        assert_eq!(s.pane(2), " 10 ten");
    }

    #[test]
    fn long_preview_paths_keep_their_end() {
        let mut a = app(&["a"], "");
        let p = text_preview(&("d/".repeat(100) + "end.rs"), None, 1, &["x"]);
        let s = draw(&mut a, Some(&p), 80, 6);
        assert!(s.pane(0).starts_with(" …") && s.pane(0).ends_with("end.rs"), "{}", s.pane(0));
    }

    #[test]
    fn tree_view_rows() {
        let mut a = app(&["src/main.rs", "src/ui.rs", "README.md"], "");
        a.set_tree_view(true);
        press(&mut a, KeyCode::Down);
        let s = draw(&mut a, None, 40, 8);
        let rows: Vec<String> = (0..4).map(|y| s.trimmed(y)).collect();
        assert_eq!(rows, vec!["  src/", "  \u{251c}\u{2500} main.rs", "> \u{2514}\u{2500} ui.rs", "  README.md"]);
        assert_eq!(s.styled(0, drawn(DIR)), "src/");
    }

    #[test]
    fn tree_view_highlights_file_name_matches() {
        let mut a = app(&["src/main.rs"], "main");
        a.set_tree_view(true);
        let s = draw(&mut a, None, 40, 6);
        assert_eq!(s.styled(1, drawn(SELECTED_MATCH)), "main");
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
        let s = draw(&mut a, None, 40, 7);
        let rows: Vec<String> = (0..3).map(|y| s.trimmed(y)).collect();
        assert_eq!(rows, vec!["  src/", "  \u{2514}\u{2500} a.rs", ">    \u{2514}\u{2500} 7: let x = 1;"]);
        assert_eq!(s.styled(2, drawn(SELECTED_MATCH)), "x");
    }

    #[test]
    fn regex_badge_and_error_in_status() {
        let mut a = app(&["a"], "a(");
        let s = draw(&mut a, None, 80, 5);
        assert_eq!(s.styled(2, drawn(BADGE)), "");
        a.set_regex(true);
        let s = draw(&mut a, None, 80, 5);
        assert_eq!(s.styled(2, drawn(BADGE)), " regex ");
        assert!(s.styled(2, drawn(ERROR)).contains("invalid regex: unclosed group"));
    }

    #[test]
    fn marked_rows_and_count() {
        let mut a = app(&["a", "b", "c"], "");
        a.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let s = draw(&mut a, None, 40, 7);
        assert_eq!(s.trimmed(0), " \u{2022}a");
        assert_eq!(s.trimmed(1), "> b");
        assert!(s.text(4).contains("1 marked"));
    }

    #[test]
    fn flash_message_in_status() {
        let mut a = app(&["a"], "");
        a.set_flash("copied: a");
        let s = draw(&mut a, None, 60, 5);
        assert!(s.text(2).contains("copied: a"));
        assert!(!s.text(2).contains("1/1 files"));
    }

    #[test]
    fn char_widths() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('ş'), 1);
        assert_eq!(char_width('漢'), 2);
        assert_eq!(char_width('한'), 2);
        assert_eq!(char_width('\u{1F600}'), 2);
        assert_eq!(char_width('\u{0301}'), 0, "combining accent");
        assert_eq!(char_width('\t'), 0, "ratatui does not draw control characters");
        assert_eq!(str_width("e\u{0301}漢"), 3);
    }

    #[test]
    fn fit_respects_wide_chars() {
        let cells = text_cells("漢字漢字", NORMAL);
        let s = |c: Vec<Cell>| c.into_iter().map(|c| c.0).collect::<String>();
        assert_eq!(s(fit_right(cells.clone(), 5)), "漢字\u{2026}");
        assert_eq!(s(fit_left(cells.clone(), 5)), "\u{2026}漢字");
        assert_eq!(s(fit_right(cells.clone(), 8)), "漢字漢字");
        assert_eq!(s(fit_right(cells, 2)), "\u{2026}");
    }

    #[test]
    fn wide_paths_keep_the_separator_in_place() {
        let mut a = app(&["文档/非常长的文件名称测试用例.txt", "😀/x.txt"], "");
        let p = text_preview("文档/x", None, 1, &["漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字漢字"]);
        let s = draw(&mut a, Some(&p), 80, 6);
        assert!(s.text(0).split(SEPARATOR).next().unwrap().trim_end().ends_with("例.txt"));
        let col = |y: usize| str_width(s.text(y).split(SEPARATOR).next().unwrap());
        assert_eq!(col(0), 40);
        assert_eq!(col(1), 40);
        assert_eq!(col(2), 40);
    }

    #[test]
    fn prompt_cursor_counts_wide_chars() {
        let mut a = app(&[], "漢a");
        let s = draw(&mut a, None, 40, 4);
        assert_eq!(s.cursor.0, 2 + 3);
    }

    #[test]
    fn hit_test_regions() {
        let a = app(&["a"], "");
        // 80 wide: list 0..40, separator at 40, preview after; 6 high: 3 list rows.
        assert_eq!(hit_test(&a, 80, 6, 5, 1), Hit::List(1));
        assert_eq!(hit_test(&a, 80, 6, 40, 1), Hit::Other);
        assert_eq!(hit_test(&a, 80, 6, 60, 0), Hit::Preview);
        assert_eq!(hit_test(&a, 80, 6, 5, 4), Hit::Other, "prompt row");
        assert_eq!(hit_test(&a, 40, 6, 30, 2), Hit::List(2), "no preview when narrow");
    }

    #[test]
    fn scrolled_preview_shows_line_range() {
        let mut a = app(&["a"], "");
        let p = text_preview("a", None, 4, &["four", "five"]);
        assert_eq!(draw(&mut a, Some(&p), 80, 6).pane(0), " a  [4-5]");
    }

    #[test]
    fn fit_helpers() {
        let cells = text_cells("abcdef", NORMAL);
        let s = |c: Vec<Cell>| c.into_iter().map(|c| c.0).collect::<String>();
        assert_eq!(s(fit_right(cells.clone(), 4)), "abc…");
        assert_eq!(s(fit_left(cells.clone(), 4)), "…def");
        assert_eq!(s(fit_left(cells.clone(), 10)), "abcdef");
        assert_eq!(s(fit_right(cells, 0)), "");
    }

    #[test]
    fn spans_merge_same_style() {
        let cells = vec![('a', NORMAL), ('b', NORMAL), ('c', MATCH)];
        assert_eq!(to_spans(&cells), Line::from(vec![Span::styled("ab", NORMAL), Span::styled("c", MATCH)]));
    }

    #[test]
    fn cursor_sits_in_the_prompt() {
        let mut a = app(&["hello.txt"], "he");
        let s = draw(&mut a, None, 30, 4);
        assert_eq!(s.cursor, (4, 2));
    }
}
