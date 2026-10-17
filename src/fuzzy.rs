//! fzf-style fuzzy matching.
//!
//! A query is split on whitespace into terms; every term must match the text
//! as a subsequence (order of terms does not matter). Each term is located
//! with a greedy forward scan and then tightened with a backward scan, and the
//! resulting span is scored with bonuses for word boundaries, path separators,
//! camelCase humps and consecutive characters.
//!
//! Matching is smart-case: case-insensitive unless the term contains an
//! uppercase character.

const SCORE_MATCH: i64 = 16;
const SCORE_GAP_START: i64 = -3;
const SCORE_GAP_EXTENSION: i64 = -1;
const BONUS_BOUNDARY: i64 = SCORE_MATCH / 2;
const BONUS_DELIMITER: i64 = BONUS_BOUNDARY + 1;
const BONUS_CAMEL: i64 = BONUS_BOUNDARY - 1;
const BONUS_CONSECUTIVE: i64 = -(SCORE_GAP_START + SCORE_GAP_EXTENSION);
const BONUS_FIRST_CHAR_MULTIPLIER: i64 = 2;
/// Extra score when a term matches entirely inside the file name, so `main`
/// prefers `src/main.rs` over `domain/x.rs`.
const BONUS_BASENAME: i64 = SCORE_MATCH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i64,
    /// Matched character (not byte) indices, sorted and unique.
    pub positions: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Lower,
    Upper,
    Number,
    Delimiter,
    NonWord,
}

fn class_of(c: char) -> Class {
    if c.is_lowercase() {
        Class::Lower
    } else if c.is_uppercase() {
        Class::Upper
    } else if c.is_numeric() {
        Class::Number
    } else if c.is_alphabetic() {
        // Letters without case (CJK, etc.) behave like lowercase letters.
        Class::Lower
    } else if c == '/' || c == '\\' {
        Class::Delimiter
    } else {
        Class::NonWord
    }
}

fn bonus_for(prev: Class, cur: Class) -> i64 {
    let is_word = matches!(cur, Class::Lower | Class::Upper | Class::Number);
    if !is_word {
        return BONUS_BOUNDARY;
    }
    match prev {
        Class::Delimiter => BONUS_DELIMITER,
        Class::NonWord => BONUS_BOUNDARY,
        Class::Lower if cur == Class::Upper => BONUS_CAMEL,
        p if p != Class::Number && cur == Class::Number => BONUS_CAMEL,
        _ => 0,
    }
}

pub(crate) fn fold(c: char, case_sensitive: bool) -> char {
    if case_sensitive {
        c
    } else if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Smart case: a query containing an uppercase letter is matched exactly.
pub fn is_case_sensitive(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TermKind {
    Fuzzy,
    /// `'foo`: substring.
    Exact,
    /// `^foo`: the path starts with it.
    Prefix,
    /// `foo$`: the path ends with it.
    Suffix,
    /// `^foo$`: the whole path.
    Equal,
}

#[derive(Debug, Clone)]
struct Term {
    chars: Vec<char>,
    case_sensitive: bool,
    kind: TermKind,
    /// `!foo`: the path must not contain it.
    inverse: bool,
}

impl Term {
    /// Parses fzf's extended syntax. Returns `None` for a bare operator
    /// (`!`, `'`, `^`, `$`) so half-typed queries do not hide everything.
    fn parse(token: &str) -> Option<Term> {
        let mut t = token;
        let inverse = t.starts_with('!');
        if inverse {
            t = &t[1..];
        }
        // An inverse term is exact unless anchored, as in fzf.
        let mut kind = if inverse { TermKind::Exact } else { TermKind::Fuzzy };
        if let Some(rest) = t.strip_prefix('\'') {
            t = rest;
            kind = TermKind::Exact;
        } else {
            let prefix = t.starts_with('^');
            if prefix {
                t = &t[1..];
            }
            let suffix = t.ends_with('$');
            if suffix {
                t = &t[..t.len() - 1];
            }
            kind = match (prefix, suffix) {
                (true, true) => TermKind::Equal,
                (true, false) => TermKind::Prefix,
                (false, true) => TermKind::Suffix,
                (false, false) => kind,
            };
        }
        if t.is_empty() {
            return None;
        }
        let case_sensitive = is_case_sensitive(t);
        Some(Term { chars: t.chars().map(|c| fold(c, case_sensitive)).collect(), case_sensitive, kind, inverse })
    }

    /// Where the term occurs in `text`, ignoring `inverse`.
    fn find(&self, text: &[char], basename_start: usize) -> Option<Match> {
        let (pat, cs) = (&self.chars, self.case_sensitive);
        let eq = |at: usize| text[at..at + pat.len()].iter().zip(pat).all(|(&c, &p)| fold(c, cs) == p);
        let span = |s: usize| {
            let mut m = score_span(text, s, s + pat.len(), pat, cs);
            if s >= basename_start {
                m.score += BONUS_BASENAME;
            }
            m
        };
        if pat.len() > text.len() {
            return None;
        }
        let last = text.len() - pat.len();
        match self.kind {
            TermKind::Fuzzy => {
                let full = match_term(text, 0, self)?;
                Some(if basename_start > 0 {
                    match match_term(text, basename_start, self) {
                        Some(mut base) => {
                            base.score += BONUS_BASENAME;
                            if base.score >= full.score { base } else { full }
                        }
                        None => full,
                    }
                } else {
                    // The whole text is a file name.
                    Match { score: full.score + BONUS_BASENAME, ..full }
                })
            }
            TermKind::Exact => {
                // Prefer an occurrence in the file name.
                let in_name = (basename_start.min(last + 1)..=last).find(|&i| eq(i));
                in_name.or_else(|| (0..=last).find(|&i| eq(i))).map(span)
            }
            TermKind::Prefix => eq(0).then(|| span(0)),
            TermKind::Suffix => eq(last).then(|| span(last)),
            TermKind::Equal => (last == 0 && eq(0)).then(|| span(0)),
        }
    }

    fn matches(&self, text: &[char], basename_start: usize) -> Option<Match> {
        let found = self.find(text, basename_start);
        if self.inverse {
            found.is_none().then(|| Match { score: 0, positions: Vec::new() })
        } else {
            found
        }
    }
}

/// A parsed query, reusable across many texts.
///
/// Whitespace separates terms that must all match. A term is fuzzy by
/// default; `'foo` is a substring, `^foo` a prefix, `foo$` a suffix,
/// `^foo$` the whole path and `!foo` excludes paths containing `foo`.
/// `a | b` matches either term.
#[derive(Debug, Clone)]
pub struct Pattern {
    /// Every group must match; a group matches if any of its terms does.
    groups: Vec<Vec<Term>>,
}

impl Pattern {
    pub fn new(query: &str) -> Self {
        let mut groups: Vec<Vec<Term>> = Vec::new();
        let mut or_next = false;
        for token in query.split_whitespace() {
            if token == "|" {
                or_next = !groups.is_empty();
                continue;
            }
            let Some(term) = Term::parse(token) else { continue };
            match groups.last_mut() {
                Some(group) if or_next => group.push(term),
                _ => groups.push(vec![term]),
            }
            or_next = false;
        }
        Pattern { groups }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Matches `text` against every group. An empty pattern matches anything
    /// with score 0.
    pub fn matches(&self, text: &str) -> Option<Match> {
        if self.groups.is_empty() {
            return Some(Match { score: 0, positions: Vec::new() });
        }
        let chars: Vec<char> = text.chars().collect();
        let basename_start = chars
            .iter()
            .rposition(|&c| c == '/' || c == '\\')
            .map_or(0, |i| i + 1);

        let mut score = 0;
        let mut positions = Vec::new();
        for group in &self.groups {
            let best = group
                .iter()
                .filter_map(|t| t.matches(&chars, basename_start))
                .max_by_key(|m| m.score)?;
            score += best.score;
            positions.extend(best.positions);
        }
        positions.sort_unstable();
        positions.dedup();
        Some(Match { score, positions })
    }
}

/// Whether every path matching `query + more` also matches `query`, so a
/// longer query only needs to re-check the previous matches. Exclusions and
/// alternatives can widen the results, so they disable this.
pub fn extension_narrows(query: &str) -> bool {
    !query.split_whitespace().any(|t| t == "|" || t.starts_with('!'))
}

/// Convenience wrapper for one-off matching.
pub fn fuzzy_match(text: &str, query: &str) -> Option<Match> {
    Pattern::new(query).matches(text)
}

/// Matches one term inside `text[from..]`; positions are absolute indices.
fn match_term(text: &[char], from: usize, term: &Term) -> Option<Match> {
    let pat = &term.chars;
    let cs = term.case_sensitive;
    if pat.is_empty() {
        return Some(Match { score: 0, positions: Vec::new() });
    }

    // Forward scan: find the first position where the whole term has matched.
    let mut pi = 0;
    let mut end = 0;
    for (i, &c) in text.iter().enumerate().skip(from) {
        if fold(c, cs) == pat[pi] {
            pi += 1;
            if pi == pat.len() {
                end = i + 1;
                break;
            }
        }
    }
    if pi < pat.len() {
        return None;
    }

    // Backward scan from the end to find the tightest start.
    let mut start = from;
    let mut pi = pat.len();
    for i in (from..end).rev() {
        if fold(text[i], cs) == pat[pi - 1] {
            pi -= 1;
            if pi == 0 {
                start = i;
                break;
            }
        }
    }

    Some(score_span(text, start, end, pat, cs))
}

fn score_span(text: &[char], start: usize, end: usize, pat: &[char], cs: bool) -> Match {
    let mut score = 0;
    let mut positions = Vec::with_capacity(pat.len());
    let mut pi = 0;
    let mut in_gap = false;
    let mut consecutive = 0;
    let mut first_bonus = 0;
    let mut prev_class = if start > 0 { class_of(text[start - 1]) } else { Class::Delimiter };

    for (i, &c) in text.iter().enumerate().take(end).skip(start) {
        let class = class_of(c);
        if pi < pat.len() && fold(c, cs) == pat[pi] {
            positions.push(i);
            score += SCORE_MATCH;
            let mut bonus = bonus_for(prev_class, class);
            if consecutive == 0 {
                first_bonus = bonus;
            } else {
                // A boundary inside a consecutive chunk starts a new chunk.
                if bonus >= BONUS_BOUNDARY && bonus > first_bonus {
                    first_bonus = bonus;
                }
                bonus = bonus.max(first_bonus).max(BONUS_CONSECUTIVE);
            }
            score += if pi == 0 { bonus * BONUS_FIRST_CHAR_MULTIPLIER } else { bonus };
            in_gap = false;
            consecutive += 1;
            pi += 1;
        } else {
            score += if in_gap { SCORE_GAP_EXTENSION } else { SCORE_GAP_START };
            in_gap = true;
            consecutive = 0;
            first_bonus = 0;
        }
        prev_class = class;
    }
    Match { score, positions }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(text: &str, q: &str) -> i64 {
        fuzzy_match(text, q).expect("should match").score
    }

    fn positions(text: &str, q: &str) -> Vec<usize> {
        fuzzy_match(text, q).expect("should match").positions
    }

    #[test]
    fn empty_query_matches_everything() {
        let m = fuzzy_match("anything.txt", "").unwrap();
        assert_eq!(m.score, 0);
        assert!(m.positions.is_empty());
        assert!(fuzzy_match("x", "   ").is_some());
        assert!(Pattern::new("  ").is_empty());
    }

    #[test]
    fn requires_subsequence_in_order() {
        assert!(fuzzy_match("a_b_c", "abc").is_some());
        assert!(fuzzy_match("acb", "abc").is_none());
        assert!(fuzzy_match("ab", "abc").is_none());
        assert!(fuzzy_match("", "a").is_none());
    }

    #[test]
    fn smart_case() {
        assert!(fuzzy_match("README.md", "readme").is_some());
        assert!(fuzzy_match("readme.md", "README").is_none());
        assert!(fuzzy_match("README.md", "README").is_some());
        // Uppercase only in one term makes only that term case-sensitive.
        assert!(fuzzy_match("src/Main.rs", "src Main").is_some());
        assert!(fuzzy_match("SRC/main.rs", "src Main").is_none());
    }

    #[test]
    fn unicode_case_folding() {
        assert!(fuzzy_match("Çalışma/Ödev.txt", "çalışma ödev").is_some());
        assert_eq!(positions("aÇbĞ", "çğ"), vec![1, 3]);
    }

    #[test]
    fn positions_are_char_indices() {
        assert_eq!(positions("foo/bar", "fb"), vec![0, 4]);
        assert_eq!(positions("ünï/x", "x"), vec![4]);
    }

    #[test]
    fn backward_scan_finds_tightest_span() {
        assert_eq!(positions("a____ab", "ab"), vec![5, 6]);
    }

    #[test]
    fn consecutive_beats_scattered() {
        assert!(score("abc.txt", "abc") > score("a_b_c.txt", "abc"));
        assert!(score("xabcx", "abc") > score("xaxbxcx", "abc"));
    }

    #[test]
    fn boundaries_beat_middle_of_word() {
        assert!(score("foo_bar", "fb") > score("fxxbxx", "fb"));
        assert!(score("src/lib", "l") > score("src/xlx", "l"));
    }

    #[test]
    fn camel_case_bonus() {
        assert_eq!(positions("fooBar", "fb"), vec![0, 3]);
        assert!(score("fooBar", "fb") > score("foobar", "fb"));
    }

    #[test]
    fn prefers_matches_in_file_name() {
        assert!(score("src/app.rs", "app") > score("app/foo/bar.txt", "app"));
        assert!(score("src/main.rs", "main") > score("src/domain/x.rs", "main"));
        // Greedy forward scan would match inside `domain`; the basename
        // pass must pick the file name instead.
        assert_eq!(positions("src/domain/main.rs", "main"), vec![11, 12, 13, 14]);
    }

    #[test]
    fn matches_across_directory_and_file_name() {
        assert_eq!(positions("src/ui.rs", "sui"), vec![0, 4, 5]);
    }

    #[test]
    fn multiple_terms_in_any_order() {
        assert!(fuzzy_match("bar/foo.txt", "foo bar").is_some());
        assert!(fuzzy_match("bar/foo.txt", "bar foo").is_some());
        assert!(fuzzy_match("bar/foo.txt", "foo baz").is_none());
        let m = fuzzy_match("bar/foo.txt", "foo bar").unwrap();
        assert_eq!(m.positions, vec![0, 1, 2, 4, 5, 6]);
    }

    #[test]
    fn overlapping_terms_dedup_positions() {
        let m = fuzzy_match("abc", "ab b").unwrap();
        assert_eq!(m.positions, vec![0, 1]);
    }

    #[test]
    fn windows_separators_count_as_delimiters() {
        assert!(score("src\\main.rs", "main") > score("srcxmain.rs", "main"));
        assert_eq!(positions("src\\domain\\main.rs", "main"), vec![11, 12, 13, 14]);
    }

    #[test]
    fn exact_term() {
        assert!(fuzzy_match("abc", "'ac").is_none());
        assert_eq!(positions("x/abc", "'bc"), vec![3, 4]);
        // Prefers the occurrence inside the file name.
        assert_eq!(positions("lib/x/lib.rs", "'lib"), vec![6, 7, 8]);
    }

    #[test]
    fn prefix_suffix_and_equal() {
        assert!(fuzzy_match("src/main.rs", "^src").is_some());
        assert!(fuzzy_match("lib/src/x", "^src").is_none());
        assert_eq!(positions("src/main.rs", ".rs$"), vec![8, 9, 10]);
        assert!(fuzzy_match("main.rsx", ".rs$").is_none());
        assert!(fuzzy_match("Cargo.toml", "^cargo.toml$").is_some());
        assert!(fuzzy_match("Cargo.toml.bak", "^cargo.toml$").is_none());
    }

    #[test]
    fn inverse_terms_exclude() {
        assert!(fuzzy_match("src/main.rs", "main !test").is_some());
        assert!(fuzzy_match("tests/main.rs", "main !test").is_none());
        assert!(fuzzy_match("src/a.rs", "!.rs$").is_none());
        assert!(fuzzy_match("src/a.md", "!.rs$").is_some());
        assert!(fuzzy_match("src/a.md", "!^src").is_none());
        // Inverse terms are exact, not fuzzy: `t...e...s...t` is not `test`.
        assert!(fuzzy_match("the_best.rs", "!test").is_some());
        assert_eq!(positions("src/main.rs", "main !test"), vec![4, 5, 6, 7]);
    }

    #[test]
    fn or_groups() {
        assert!(fuzzy_match("a.rs", ".rs$ | .md$").is_some());
        assert!(fuzzy_match("a.md", ".rs$ | .md$").is_some());
        assert!(fuzzy_match("a.txt", ".rs$ | .md$").is_none());
        // `|` binds two terms; other terms are still ANDed.
        assert!(fuzzy_match("src/a.md", "^src .rs$ | .md$").is_some());
        assert!(fuzzy_match("doc/a.md", "^src .rs$ | .md$").is_none());
        // A leading or dangling `|` is ignored.
        assert!(fuzzy_match("abc", "| abc |").is_some());
    }

    #[test]
    fn bare_operators_are_ignored_while_typing() {
        for q in ["!", "'", "^", "$", "^$", "!^"] {
            assert!(Pattern::new(q).is_empty(), "{q:?}");
        }
        assert!(fuzzy_match("anything", "abc !").is_none(), "other terms still apply");
    }

    #[test]
    fn exact_terms_are_smart_case() {
        assert!(fuzzy_match("README.md", "'readme").is_some());
        assert!(fuzzy_match("readme.md", "'README").is_none());
    }

    #[test]
    fn extension_narrowing_rules() {
        assert!(extension_narrows("src main"));
        assert!(extension_narrows("^src .rs$ 'x"));
        assert!(!extension_narrows("src !test"));
        assert!(!extension_narrows("a | b"));
    }
}
