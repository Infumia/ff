//! Line matchers for content search: a literal string or a regex.
//!
//! Both are smart-case (case-insensitive unless the query has an uppercase
//! letter) and report matches as char ranges, which is what the UI needs for
//! highlighting.

use regex::{Regex, RegexBuilder};

use crate::content::Needle;

#[derive(Debug, Clone)]
pub enum Matcher {
    Literal(Needle),
    Regex(Regex),
}

impl Matcher {
    /// `Ok(None)` for a blank query, `Err` with a one-line message for an
    /// invalid regex.
    pub fn new(query: &str, regex: bool) -> Result<Option<Matcher>, String> {
        if query.trim().is_empty() {
            return Ok(None);
        }
        if regex {
            build_regex(query).map(|r| Some(Matcher::Regex(r)))
        } else {
            Ok(Needle::new(query).map(Matcher::Literal))
        }
    }

    /// The first match in `line` as a char range. A regex may match an empty
    /// range (e.g. `x*`), which still counts as a matching line.
    pub fn find(&self, line: &str) -> Option<(usize, usize)> {
        match self {
            Matcher::Literal(n) => n.find(line),
            Matcher::Regex(r) => r.find(line).map(|m| char_range(line, m.start(), m.end())),
        }
    }

    /// All non-empty, non-overlapping matches in `line` as char ranges.
    pub fn find_all(&self, line: &str) -> Vec<(usize, usize)> {
        match self {
            Matcher::Literal(n) => n.find_all(line),
            Matcher::Regex(r) => r
                .find_iter(line)
                .filter(|m| !m.is_empty())
                .map(|m| char_range(line, m.start(), m.end()))
                .collect(),
        }
    }

    /// Cheap whole-file check before splitting into lines. May return true
    /// for files without a matching line, never false for one with.
    pub fn may_match(&self, text: &str) -> bool {
        match self {
            // The regex is multi-line, so `^`/`$` also work on the whole text.
            Matcher::Regex(r) => r.is_match(text),
            Matcher::Literal(_) => true,
        }
    }
}

/// Smart case for regexes: an uppercase letter makes the pattern
/// case-sensitive, except when it is part of an escape like `\W` or `\S`.
pub fn regex_is_case_sensitive(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c.is_uppercase() {
            return true;
        }
    }
    false
}

/// Compiles a smart-case regex where `^` and `$` match at line boundaries
/// (also before `\r\n`). Errors are reduced to their last line, e.g.
/// "unclosed group".
pub fn build_regex(pattern: &str) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(!regex_is_case_sensitive(pattern))
        .multi_line(true)
        .crlf(true)
        .build()
        .map_err(|e| short_error(&e.to_string()))
}

fn short_error(message: &str) -> String {
    let last = message.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(message);
    last.trim().trim_start_matches("error:").trim().to_string()
}

/// Converts a byte range of `text` to a char range.
pub fn char_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    if text.is_ascii() {
        return (start, end);
    }
    let s = text[..start].chars().count();
    (s, s + text[start..end].chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(q: &str) -> Matcher {
        Matcher::new(q, true).unwrap().unwrap()
    }

    fn lit(q: &str) -> Matcher {
        Matcher::new(q, false).unwrap().unwrap()
    }

    #[test]
    fn blank_query_gives_no_matcher() {
        assert!(Matcher::new("", true).unwrap().is_none());
        assert!(Matcher::new("  ", false).unwrap().is_none());
    }

    #[test]
    fn invalid_regex_reports_short_error() {
        let err = Matcher::new("foo(", true).unwrap_err();
        assert!(err.contains("unclosed group"), "{err}");
        assert!(!err.contains('\n'));
        // The same text is fine as a literal.
        assert!(Matcher::new("foo(", false).unwrap().is_some());
    }

    #[test]
    fn regex_finds_char_ranges() {
        assert_eq!(re(r"fn \w+").find("pub fn main() {"), Some((4, 11)));
        assert_eq!(re(r"\d+").find("çğ 123"), Some((3, 6)));
        assert_eq!(re("x").find("abc"), None);
    }

    #[test]
    fn regex_anchors_work_per_line() {
        assert!(re("^use ").find("use std::fs;").is_some());
        assert!(re("^use ").find("  use x").is_none());
        assert!(re(";$").find("let a = 1;").is_some());
    }

    #[test]
    fn regex_smart_case() {
        assert!(re("hello").find("HELLO").is_some());
        assert!(re("Hello").find("hello").is_none());
        // Escapes like \W or \S do not make the pattern case-sensitive.
        assert!(re(r"a\Sc").find("AbC").is_some());
        assert!(regex_is_case_sensitive(r"\\A"), "escaped backslash then uppercase");
        assert!(!regex_is_case_sensitive(r"\W\S\D"));
        assert!(regex_is_case_sensitive("Foo"));
    }

    #[test]
    fn find_all_skips_empty_matches() {
        assert_eq!(re("a*").find_all("baab"), vec![(1, 3)]);
        assert_eq!(re("a*").find("b"), Some((0, 0)), "empty match still matches the line");
        assert_eq!(re("o").find_all("foo boo"), vec![(1, 2), (2, 3), (5, 6), (6, 7)]);
        assert_eq!(lit("ab").find_all("ab ab"), vec![(0, 2), (3, 5)]);
    }

    #[test]
    fn may_match_prefilters_whole_text() {
        let m = re("^fn main");
        assert!(m.may_match("use x;\nfn main() {}\n"));
        assert!(m.may_match("use x;\r\nfn main() {}\r\n"));
        assert!(!m.may_match("use x;\n"));
        assert!(re("1;$").may_match("let a = 1;\r\nnext"));
        assert!(lit("anything").may_match(""));
    }

    #[test]
    fn literal_matcher_delegates_to_needle() {
        assert_eq!(lit("a.c").find("abc a.c"), Some((4, 7)), "dots are literal");
        assert_eq!(re("a.c").find("abc a.c"), Some((0, 3)), "dots are wildcards");
    }

    #[test]
    fn char_range_conversion() {
        assert_eq!(char_range("abc", 1, 2), (1, 2));
        let s = "ğüx";
        let b = s.find('x').unwrap();
        assert_eq!(char_range(s, b, b + 1), (2, 3));
    }
}
