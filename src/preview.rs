//! Loading a window of a file for the preview pane.
//!
//! Only the lines that are shown are kept, and reading stops right after
//! them, so previewing a line near the top of a huge file stays cheap.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;

use crate::content::{is_binary, MAX_FILE_SIZE};

const TAB_WIDTH: usize = 4;
/// Longer lines are cut; the pane is never this wide.
const MAX_LINE_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// `lines[0]` is line number `first_line` (1-based).
    Text { first_line: usize, lines: Vec<String> },
    Binary,
    Unreadable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub path: Arc<str>,
    /// The line to center on and highlight (content mode).
    pub focus: Option<usize>,
    /// The first line that was asked for (see `Body::Text` for the actual one).
    pub start: usize,
    /// Number of text rows this preview was loaded for.
    pub height: usize,
    pub body: Body,
}

impl Preview {
    /// Whether this preview already shows what was asked for.
    pub fn is_for(&self, path: &str, focus: Option<usize>, start: usize, height: usize) -> bool {
        self.path.as_ref() == path && self.focus == focus && self.start == start && self.height == height
    }
}

/// First line of the window: the focus line sits about a third of the way
/// down so there is some context above it.
pub fn window_start(focus: Option<usize>, height: usize) -> usize {
    focus.map_or(1, |line| line.saturating_sub(height / 3).max(1))
}

/// Loads `height` lines from line `start` (see `read_window`).
pub fn load(path: &Arc<str>, focus: Option<usize>, start: usize, height: usize) -> Preview {
    let body = match File::open(path.as_ref()) {
        Ok(file) => read_window(file, start, height),
        Err(e) => Body::Unreadable(e.to_string()),
    };
    Preview { path: path.clone(), focus, start, height, body }
}

/// Reads `height` lines starting at line `start` (1-based). Near the end of
/// the file the window moves up so it stays full.
pub fn read_window(reader: impl Read, start: usize, height: usize) -> Body {
    let mut reader = BufReader::with_capacity(8 * 1024, reader.take(MAX_FILE_SIZE));
    match reader.fill_buf() {
        Ok(head) if is_binary(head) => return Body::Binary,
        Ok(_) => {}
        Err(e) => return Body::Unreadable(e.to_string()),
    }

    if height == 0 {
        return Body::Text { first_line: start, lines: Vec::new() };
    }
    // The last `height` raw lines read so far.
    let mut window: VecDeque<Vec<u8>> = VecDeque::with_capacity(height + 1);
    let mut line_no = 0;
    let last_wanted = start.max(1) - 1 + height;
    while line_no < last_wanted {
        let mut buf = Vec::new();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return Body::Unreadable(e.to_string()),
        }
        line_no += 1;
        window.push_back(buf);
        if window.len() > height {
            window.pop_front();
        }
    }
    let first_line = (line_no + 1 - window.len()).max(1);
    Body::Text { first_line, lines: window.iter().map(|l| display_line(l)).collect() }
}

/// Decodes a raw line for display: strips the line ending, expands tabs and
/// replaces other control characters.
fn display_line(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches(['\n', '\r']);
    let mut out = String::new();
    let mut col = 0;
    for c in text.chars() {
        if col >= MAX_LINE_CHARS {
            break;
        }
        if c == '\t' {
            let spaces = TAB_WIDTH - col % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', spaces));
            col += spaces;
        } else {
            out.push(if c.is_control() { ' ' } else { c });
            col += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    fn text(body: Body) -> (usize, Vec<String>) {
        match body {
            Body::Text { first_line, lines } => (first_line, lines),
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn reads_from_top_without_focus() {
        let (first, lines) = text(read_window(numbered(100).as_bytes(), 1, 3));
        assert_eq!(first, 1);
        assert_eq!(lines, vec!["line 1", "line 2", "line 3"]);
    }

    #[test]
    fn window_puts_focus_a_third_down() {
        assert_eq!(window_start(None, 30), 1);
        assert_eq!(window_start(Some(5), 30), 1);
        assert_eq!(window_start(Some(50), 30), 40);
        assert_eq!(window_start(Some(50), 0), 50);
        let (first, lines) = text(read_window(numbered(100).as_bytes(), window_start(Some(50), 9), 9));
        assert_eq!(first, 47);
        assert_eq!(lines[3], "line 50");
    }

    #[test]
    fn short_file_gives_fewer_lines() {
        let (_, lines) = text(read_window("a\nb".as_bytes(), 1, 10));
        assert_eq!(lines, vec!["a", "b"]);
        let (_, lines) = text(read_window("".as_bytes(), 1, 10));
        assert!(lines.is_empty());
    }

    #[test]
    fn window_past_end_shows_last_lines() {
        let (first, lines) = text(read_window(numbered(5).as_bytes(), 10, 3));
        assert_eq!(first, 3);
        assert_eq!(lines, vec!["line 3", "line 4", "line 5"]);
        let (first, lines) = text(read_window(numbered(5).as_bytes(), 4, 3));
        assert_eq!((first, lines.len()), (3, 3));
    }

    #[test]
    fn zero_height_window() {
        assert_eq!(text(read_window(numbered(5).as_bytes(), 2, 0)), (2, vec![]));
    }

    #[test]
    fn crlf_tabs_and_control_chars() {
        let (_, lines) = text(read_window("a\tb\r\n\tx\x07y\r\n".as_bytes(), 1, 5));
        assert_eq!(lines, vec!["a   b", "    x y"]);
    }

    #[test]
    fn long_lines_are_cut() {
        let line = "x".repeat(10_000);
        let (_, lines) = text(read_window(line.as_bytes(), 1, 1));
        assert_eq!(lines[0].chars().count(), MAX_LINE_CHARS);
    }

    #[test]
    fn invalid_utf8_is_shown_lossily() {
        let (_, lines) = text(read_window(&b"caf\xe9\n"[..], 1, 1));
        assert_eq!(lines, vec!["caf\u{fffd}"]);
    }

    #[test]
    fn binary_is_detected() {
        assert_eq!(read_window(&b"\x7fELF\0\0\0"[..], 1, 5), Body::Binary);
    }

    #[test]
    fn stops_reading_after_window() {
        // Hands out one line per read and panics if the third is requested.
        struct OneLinePerRead(usize);
        impl Read for OneLinePerRead {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                assert!(self.0 < 2, "read past the window");
                let chunk: &[u8] = [b"a\n".as_slice(), b"b\n"][self.0];
                self.0 += 1;
                buf[..chunk.len()].copy_from_slice(chunk);
                Ok(chunk.len())
            }
        }
        let (_, lines) = text(read_window(OneLinePerRead(0), 1, 2));
        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn load_from_disk() {
        let t = TempDir::new("preview");
        let p = t.write("f.txt", numbered(20));
        let path: Arc<str> = Arc::from(p.to_string_lossy().as_ref());
        let preview = load(&path, Some(10), window_start(Some(10), 6), 6);
        assert_eq!(preview.focus, Some(10));
        assert!(preview.is_for(&path, Some(10), 8, 6));
        assert!(!preview.is_for(&path, Some(11), 8, 6));
        assert!(!preview.is_for(&path, Some(10), 9, 6));
        assert!(!preview.is_for(&path, Some(10), 8, 7));
        assert_eq!(text(preview.body), (8, (8..=13).map(|i| format!("line {i}")).collect()));
    }

    #[test]
    fn missing_file_is_unreadable() {
        let t = TempDir::new("preview-missing");
        let path: Arc<str> = Arc::from(t.path().join("nope").to_string_lossy().as_ref());
        assert!(matches!(load(&path, None, 1, 5).body, Body::Unreadable(_)));
    }
}
