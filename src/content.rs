//! Searching inside files.
//!
//! Each line is checked with a `Matcher`: a literal, smart-case substring
//! (like `grep -F`) or a regex. Binary files (a NUL byte in the first 8 KiB)
//! and very large files are skipped.

use std::fs;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use crate::fuzzy::{fold, is_case_sensitive};
use crate::matcher::Matcher;

pub const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;
const BINARY_SNIFF_LEN: usize = 8 * 1024;
/// Long lines (minified files) are cut to a window around the match.
const MAX_LINE_CHARS: usize = 400;
const CONTEXT_BEFORE_MATCH: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentMatch {
    /// Index of the file in the walk order, used for stable sorting.
    pub file_idx: usize,
    pub path: Arc<str>,
    /// 1-based line number.
    pub line: usize,
    /// Display text: leading whitespace trimmed, control chars replaced.
    pub text: String,
    /// Char range of the match inside `text`.
    pub range: (usize, usize),
}

/// A literal search string with smart-case semantics.
#[derive(Debug, Clone)]
pub struct Needle {
    chars: Vec<char>,
    bytes: Vec<u8>,
    case_sensitive: bool,
    ascii: bool,
}

impl Needle {
    /// Returns `None` for an empty or whitespace-only query.
    pub fn new(query: &str) -> Option<Self> {
        if query.trim().is_empty() {
            return None;
        }
        let case_sensitive = is_case_sensitive(query);
        let chars: Vec<char> = query.chars().map(|c| fold(c, case_sensitive)).collect();
        let folded: String = chars.iter().collect();
        Some(Needle {
            ascii: folded.is_ascii(),
            bytes: folded.into_bytes(),
            chars,
            case_sensitive,
        })
    }

    /// Finds the first occurrence in `line`, returning a char range.
    pub fn find(&self, line: &str) -> Option<(usize, usize)> {
        let n = self.chars.len();
        if self.ascii && line.is_ascii() {
            // Fast path: byte indices equal char indices.
            let hay = line.as_bytes();
            if hay.len() < n {
                return None;
            }
            return hay
                .windows(n)
                .position(|w| {
                    if self.case_sensitive {
                        w == self.bytes.as_slice()
                    } else {
                        w.eq_ignore_ascii_case(&self.bytes)
                    }
                })
                .map(|i| (i, i + n));
        }
        let hay: Vec<char> = line.chars().map(|c| fold(c, self.case_sensitive)).collect();
        if hay.len() < n {
            return None;
        }
        hay.windows(n).position(|w| w == self.chars.as_slice()).map(|i| (i, i + n))
    }
}

impl Needle {
    /// All non-overlapping occurrences in `line`, as char ranges.
    pub fn find_all(&self, line: &str) -> Vec<(usize, usize)> {
        let n = self.chars.len();
        let hay: Vec<char> = line.chars().map(|c| fold(c, self.case_sensitive)).collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i + n <= hay.len() {
            if hay[i..i + n] == self.chars[..] {
                out.push((i, i + n));
                i += n;
            } else {
                i += 1;
            }
        }
        out
    }
}

pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_SNIFF_LEN)].contains(&0)
}

/// Searches `data` line by line, appending at most `limit` matches in total
/// to `out`.
pub fn search_bytes(
    file_idx: usize,
    path: &Arc<str>,
    data: &[u8],
    matcher: &Matcher,
    limit: usize,
    out: &mut Vec<ContentMatch>,
) {
    if is_binary(data) {
        return;
    }
    let text = String::from_utf8_lossy(data);
    if !matcher.may_match(&text) {
        return;
    }
    for (i, line) in text.lines().enumerate() {
        if out.len() >= limit {
            return;
        }
        if let Some(range) = matcher.find(line) {
            let (text, range) = display_line(line, range);
            out.push(ContentMatch { file_idx, path: path.clone(), line: i + 1, text, range });
        }
    }
}

/// Trims indentation, replaces control characters and windows long lines so
/// the match stays visible. Ranges are char indices.
fn display_line(line: &str, range: (usize, usize)) -> (String, (usize, usize)) {
    let chars: Vec<char> = line.chars().collect();
    // Indentation is always trimmed; a match that starts inside it (e.g.
    // regex `^\s*fn`) is clamped to the first visible char.
    let lead = chars.iter().take_while(|c| c.is_whitespace()).count();
    let mut chars = &chars[lead..];
    let (mut start, mut end) = (range.0.max(lead) - lead, range.1.max(lead) - lead);

    if chars.len() > MAX_LINE_CHARS {
        let from = start.saturating_sub(CONTEXT_BEFORE_MATCH);
        let to = (from + MAX_LINE_CHARS).min(chars.len());
        chars = &chars[from..to];
        start -= from;
        end = (end - from).min(chars.len());
    }
    let text = chars.iter().map(|&c| if c.is_control() { ' ' } else { c }).collect();
    (text, (start, end))
}

/// Reads and searches one file. Unreadable, binary and huge files yield nothing.
pub fn search_file(file_idx: usize, path: &Arc<str>, matcher: &Matcher, limit: usize) -> Vec<ContentMatch> {
    let mut out = Vec::new();
    match fs::metadata(path.as_ref()) {
        Ok(meta) if meta.is_file() && meta.len() <= MAX_FILE_SIZE => {}
        _ => return out,
    }
    if let Ok(data) = fs::read(path.as_ref()) {
        search_bytes(file_idx, path, &data, matcher, limit, &mut out);
    }
    out
}

/// Searches files sequentially, in order. Used by the non-interactive mode.
pub fn search_files(files: &[Arc<str>], matcher: &Matcher, limit: usize) -> Vec<ContentMatch> {
    let mut out = Vec::new();
    for (i, path) in files.iter().enumerate() {
        if out.len() >= limit {
            break;
        }
        out.extend(search_file(i, path, matcher, limit - out.len()));
    }
    out
}

/// Matches from one file, tagged with the search generation that produced
/// them. Each `search` call also ends with one batch with `done` set.
#[derive(Debug)]
pub struct ContentBatch {
    pub done: bool,
    pub generation: u64,
    pub matches: Vec<ContentMatch>,
}

/// Runs content searches on worker threads. Starting a search with a new
/// generation makes workers of older generations stop at the next file.
pub struct ContentSearcher {
    current: Arc<AtomicU64>,
    tx: Sender<ContentBatch>,
    workers: usize,
}

impl ContentSearcher {
    pub fn new() -> (Self, Receiver<ContentBatch>) {
        let (tx, rx) = mpsc::channel();
        let workers = thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
        (ContentSearcher { current: Arc::new(AtomicU64::new(0)), tx, workers }, rx)
    }

    /// Cancels every search whose generation differs from `generation`.
    pub fn set_generation(&self, generation: u64) {
        self.current.store(generation, Ordering::SeqCst);
    }

    /// Searches `files` (walk index, path) in the background. Results are
    /// sent per file; at most roughly `limit` matches are produced.
    pub fn search(&self, generation: u64, matcher: Arc<Matcher>, files: Vec<(usize, Arc<str>)>, limit: usize) {
        self.set_generation(generation);
        self.spawn_workers(generation, matcher, files, limit);
    }

    fn spawn_workers(&self, generation: u64, matcher: Arc<Matcher>, files: Vec<(usize, Arc<str>)>, limit: usize) {
        let done = ContentBatch { done: true, generation, matches: Vec::new() };
        if files.is_empty() {
            let _ = self.tx.send(done);
            return;
        }
        let files = Arc::new(files);
        let next = Arc::new(AtomicUsize::new(0));
        let found = Arc::new(AtomicUsize::new(0));
        let workers = self.workers.min(files.len());
        let running = Arc::new(AtomicUsize::new(workers));

        for _ in 0..workers {
            let (files, matcher, next, found) = (files.clone(), matcher.clone(), next.clone(), found.clone());
            let (current, tx, running) = (self.current.clone(), self.tx.clone(), running.clone());
            thread::spawn(move || {
                loop {
                    if current.load(Ordering::Relaxed) != generation || found.load(Ordering::Relaxed) >= limit {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((idx, path)) = files.get(i) else { break };
                    let matches = search_file(*idx, path, &matcher, limit);
                    if !matches.is_empty() {
                        found.fetch_add(matches.len(), Ordering::Relaxed);
                        if tx.send(ContentBatch { done: false, generation, matches }).is_err() {
                            break;
                        }
                    }
                }
                // The last worker out reports that this search is over.
                if running.fetch_sub(1, Ordering::AcqRel) == 1 {
                    let _ = tx.send(ContentBatch { done: true, generation, matches: Vec::new() });
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    fn needle(q: &str) -> Needle {
        Needle::new(q).unwrap()
    }

    fn lit(q: &str) -> Matcher {
        Matcher::Literal(needle(q))
    }

    fn lit_arc(q: &str) -> Arc<Matcher> {
        Arc::new(lit(q))
    }

    fn indexed(base: usize, files: Vec<Arc<str>>) -> Vec<(usize, Arc<str>)> {
        files.into_iter().enumerate().map(|(i, f)| (base + i, f)).collect()
    }

    fn search_str(text: &str, q: &str) -> Vec<ContentMatch> {
        let mut out = Vec::new();
        search_bytes(0, &Arc::from("f.txt"), text.as_bytes(), &lit(q), usize::MAX, &mut out);
        out
    }

    #[test]
    fn regex_search_reports_lines() {
        let m = Matcher::new(r"^\s*fn \w+", true).unwrap().unwrap();
        let mut out = Vec::new();
        let src = "use x;\nfn main() {}\r\n    fn helper() {}\nlet fn_ptr = 1;\n";
        search_bytes(0, &Arc::from("f.rs"), src.as_bytes(), &m, usize::MAX, &mut out);
        let got: Vec<_> = out.iter().map(|m| (m.line, m.text.as_str(), m.range)).collect();
        assert_eq!(got, vec![(2, "fn main() {}", (0, 7)), (3, "fn helper() {}", (0, 9))]);
    }

    #[test]
    fn regex_prefilter_skips_files_without_match() {
        let m = Matcher::new("^zzz", true).unwrap().unwrap();
        let mut out = Vec::new();
        search_bytes(0, &Arc::from("f"), b"a zzz\nb\n", &m, usize::MAX, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn empty_needle_is_rejected() {
        assert!(Needle::new("").is_none());
        assert!(Needle::new("   ").is_none());
    }

    #[test]
    fn find_is_smart_case() {
        assert_eq!(needle("hello").find("say Hello world"), Some((4, 9)));
        assert_eq!(needle("Hello").find("say hello world"), None);
        assert_eq!(needle("Hello").find("say Hello world"), Some((4, 9)));
    }

    #[test]
    fn find_handles_unicode_with_char_ranges() {
        assert_eq!(needle("şğ").find("aŞĞb"), Some((1, 3)));
        assert_eq!(needle("b").find("ççb"), Some((2, 3)));
        assert_eq!(needle("ç").find("abc"), None);
    }

    #[test]
    fn find_keeps_spaces_in_query() {
        assert_eq!(needle("fn main").find("pub fn main() {"), Some((4, 11)));
        assert_eq!(needle("fn main").find("fnmain"), None);
    }

    #[test]
    fn find_all_occurrences() {
        assert_eq!(needle("ab").find_all("ab_AB_xab"), vec![(0, 2), (3, 5), (7, 9)]);
        assert_eq!(needle("aa").find_all("aaaa"), vec![(0, 2), (2, 4)], "non-overlapping");
        assert_eq!(needle("ğ").find_all("aĞbğ"), vec![(1, 2), (3, 4)]);
        assert!(needle("x").find_all("abc").is_empty());
        assert!(needle("abc").find_all("ab").is_empty());
    }

    #[test]
    fn needle_longer_than_line() {
        assert_eq!(needle("abcdef").find("abc"), None);
        assert_eq!(needle("ğğğğ").find("ğ"), None);
    }

    #[test]
    fn reports_line_numbers_and_trims_indentation() {
        let m = search_str("first\n    let foo = 1;\r\nfoo again\n", "foo");
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].line, m[0].text.as_str(), m[0].range), (2, "let foo = 1;", (4, 7)));
        assert_eq!((m[1].line, m[1].text.as_str(), m[1].range), (3, "foo again", (0, 3)));
    }

    #[test]
    fn tabs_become_spaces() {
        let m = search_str("\tx\ty", "y");
        assert_eq!(m[0].text, "x y");
        assert_eq!(m[0].range, (2, 3));
    }

    #[test]
    fn long_lines_are_windowed_around_match() {
        let line = format!("{}NEEDLE{}", "a".repeat(5000), "b".repeat(5000));
        let m = search_str(&line, "NEEDLE");
        let text: Vec<char> = m[0].text.chars().collect();
        assert!(text.len() <= MAX_LINE_CHARS);
        let (s, e) = m[0].range;
        assert_eq!(text[s..e].iter().collect::<String>(), "NEEDLE");
    }

    #[test]
    fn match_inside_leading_whitespace_is_clamped() {
        // Indentation is trimmed even when the match starts inside it.
        let m = search_str("    x", "  x");
        assert_eq!(m[0].text, "x");
        assert_eq!(m[0].range, (0, 1));
    }

    #[test]
    fn binary_data_is_skipped() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary(b"plain text"));
        let mut out = Vec::new();
        search_bytes(0, &Arc::from("bin"), b"foo\0foo", &lit("foo"), 10, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn invalid_utf8_is_searched_lossily() {
        let m = search_str_bytes(b"caf\xe9 foo\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].range, (5, 8));
    }

    fn search_str_bytes(data: &[u8]) -> Vec<ContentMatch> {
        let mut out = Vec::new();
        search_bytes(0, &Arc::from("f"), data, &lit("foo"), usize::MAX, &mut out);
        out
    }

    #[test]
    fn limit_is_respected() {
        let mut out = Vec::new();
        search_bytes(0, &Arc::from("f"), "x\nx\nx\nx\n".as_bytes(), &lit("x"), 2, &mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn search_files_in_order_with_indices() {
        let t = TempDir::new("content");
        let a = t.write("a.txt", "hello\nnothing\nhello again");
        let b = t.write("b.txt", "nope");
        let c = t.write("c.txt", "say hello");
        let files: Vec<Arc<str>> =
            [a, b, c].iter().map(|p| Arc::from(p.to_string_lossy().as_ref())).collect();

        let m = search_files(&files, &lit("hello"), usize::MAX);
        let got: Vec<_> = m.iter().map(|m| (m.file_idx, m.line)).collect();
        assert_eq!(got, vec![(0, 1), (0, 3), (2, 1)]);

        let limited = search_files(&files, &lit("hello"), 2);
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn missing_file_yields_nothing() {
        let t = TempDir::new("content-missing");
        let path: Arc<str> = Arc::from(t.path().join("nope.txt").to_string_lossy().as_ref());
        assert!(search_file(0, &path, &lit("x"), 10).is_empty());
    }

    #[test]
    fn background_searcher_finds_all_matches() {
        let t = TempDir::new("searcher");
        let files: Vec<Arc<str>> = (0..50)
            .map(|i| {
                let body = if i % 5 == 0 { "needle here" } else { "hay" };
                let p = t.write(&format!("f{i:02}.txt"), body);
                Arc::from(p.to_string_lossy().as_ref())
            })
            .collect();

        let (searcher, rx) = ContentSearcher::new();
        searcher.search(7, lit_arc("needle"), indexed(100, files), usize::MAX);
        drop(searcher); // Channel closes once every worker is done.

        let batches: Vec<ContentBatch> = rx.iter().collect();
        assert!(batches.iter().all(|b| b.generation == 7));
        assert_eq!(batches.iter().filter(|b| b.done).count(), 1, "exactly one done marker");
        assert!(batches.last().unwrap().done, "done comes last");
        let mut idx: Vec<usize> = batches.into_iter().flat_map(|b| b.matches).map(|m| m.file_idx).collect();
        idx.sort_unstable();
        assert_eq!(idx, vec![100, 105, 110, 115, 120, 125, 130, 135, 140, 145]);
    }

    #[test]
    fn searcher_with_no_files_sends_nothing() {
        let (searcher, rx) = ContentSearcher::new();
        searcher.search(1, lit_arc("x"), Vec::new(), 10);
        drop(searcher);
        let batches: Vec<ContentBatch> = rx.iter().collect();
        assert_eq!(batches.len(), 1);
        assert!(batches[0].done && batches[0].matches.is_empty());
    }

    #[test]
    fn stale_generation_stops_workers() {
        let t = TempDir::new("searcher-stale");
        let files: Vec<Arc<str>> = (0..20)
            .map(|i| Arc::from(t.write(&format!("f{i}.txt"), "x").to_string_lossy().as_ref()))
            .collect();
        let (searcher, rx) = ContentSearcher::new();
        // A newer search (generation 2) is current, so workers spawned for
        // generation 1 must quit before reading anything.
        searcher.set_generation(2);
        searcher.spawn_workers(1, lit_arc("x"), indexed(0, files.clone()), usize::MAX);
        drop(searcher);
        assert!(rx.iter().all(|b| b.done && b.matches.is_empty()));

        // Sanity check: the same files do match when the generation is current.
        let (searcher, rx) = ContentSearcher::new();
        searcher.search(3, lit_arc("x"), indexed(0, files), usize::MAX);
        drop(searcher);
        assert_eq!(rx.iter().filter(|b| !b.done).count(), 20);
    }

    #[test]
    fn result_limit_stops_search_early() {
        let t = TempDir::new("searcher-limit");
        let files: Vec<Arc<str>> = (0..200)
            .map(|i| Arc::from(t.write(&format!("f{i:03}.txt"), "x\nx").to_string_lossy().as_ref()))
            .collect();
        let (searcher, rx) = ContentSearcher::new();
        searcher.search(1, lit_arc("x"), indexed(0, files), 10);
        drop(searcher);
        let total: usize = rx.iter().map(|b| b.matches.len()).sum();
        // Each worker may finish the file it is on, so allow some overshoot.
        assert!((10..=10 + 2 * 8).contains(&total), "total = {total}");
    }
}
