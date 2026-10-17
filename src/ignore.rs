//! A small `.gitignore` implementation.
//!
//! Supported: comments, blank lines, `!` negation, trailing `/` for
//! directories only, leading or inner `/` anchoring a pattern to the
//! `.gitignore`'s directory, and the globs `*`, `?`, `[abc]`, `[a-z]`,
//! `[!a]`, `**` and `\` escapes. As in git, the last matching rule wins and
//! rules from deeper `.gitignore` files come after (override) outer ones.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Char(char),
    /// `?`
    Any,
    /// `*`: anything except `/`.
    Star,
    /// `**`: anything, including `/`.
    DoubleStar,
    /// `**/`: zero or more whole directories.
    DirStar,
    Class { negated: bool, ranges: Vec<(char, char)> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    tokens: Vec<Token>,
    negated: bool,
    dir_only: bool,
    /// Matched against the path relative to the base directory; otherwise
    /// against the file name only.
    anchored: bool,
}

/// The rules of one `.gitignore` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gitignore {
    base: PathBuf,
    rules: Vec<Rule>,
}

impl Gitignore {
    /// Parses `text` as the `.gitignore` of directory `base`.
    pub fn parse(base: &Path, text: &str) -> Self {
        let rules = text.lines().filter_map(parse_rule).collect();
        Gitignore { base: base.to_path_buf(), rules }
    }

    /// Reads `dir/.gitignore`, if there is one with rules.
    pub fn load(dir: &Path) -> Option<Self> {
        let text = fs::read_to_string(dir.join(".gitignore")).ok()?;
        let ignore = Self::parse(dir, &text);
        (!ignore.rules.is_empty()).then_some(ignore)
    }

    /// `Some(true)` if ignored, `Some(false)` if re-included by a `!` rule,
    /// `None` if no rule matches.
    fn verdict(&self, path: &Path, is_dir: bool) -> Option<bool> {
        let rel = if self.base == Path::new(".") { path } else { path.strip_prefix(&self.base).ok()? };
        let rel: Vec<char> = rel.to_string_lossy().replace('\\', "/").chars().collect();
        let name_start = rel.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1);
        self.rules.iter().rev().find_map(|rule| {
            if rule.dir_only && !is_dir {
                return None;
            }
            let text = if rule.anchored { &rel[..] } else { &rel[name_start..] };
            glob_match(&rule.tokens, text).then_some(!rule.negated)
        })
    }
}

/// Whether `path` is ignored by the stack of `.gitignore`s, outermost first.
pub fn is_ignored(stack: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    stack.iter().rev().find_map(|g| g.verdict(path, is_dir)).unwrap_or(false)
}

fn parse_rule(line: &str) -> Option<Rule> {
    // Trailing spaces are ignored unless escaped.
    let mut line = line.trim_end_matches(['\r', '\n']);
    while line.ends_with(' ') && !line.ends_with("\\ ") {
        line = &line[..line.len() - 1];
    }
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (negated, line) = match line.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, line.strip_prefix("\\!").map_or(line, |_| &line[1..])),
    };
    let line = if line.starts_with("\\#") { &line[1..] } else { line };
    let (dir_only, line) = match line.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let anchored = line.contains('/');
    let line = line.strip_prefix('/').unwrap_or(line);
    if line.is_empty() {
        return None;
    }
    Some(Rule { tokens: tokenize(line), negated, dir_only, anchored })
}

fn tokenize(pattern: &str) -> Vec<Token> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                tokens.push(Token::Char(chars[i + 1]));
                i += 1;
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                let at_start = i == 0 || chars[i - 1] == '/';
                if at_start && chars.get(i + 2) == Some(&'/') {
                    tokens.push(Token::DirStar);
                    i += 2;
                } else {
                    tokens.push(Token::DoubleStar);
                    i += 1;
                }
            }
            '*' => tokens.push(Token::Star),
            '?' => tokens.push(Token::Any),
            '[' => match parse_class(&chars[i + 1..]) {
                Some((token, used)) => {
                    tokens.push(token);
                    i += used;
                }
                None => tokens.push(Token::Char('[')),
            },
            c => tokens.push(Token::Char(c)),
        }
        i += 1;
    }
    tokens
}

/// Parses the inside of `[...]`; returns the token and how many chars after
/// `[` it consumed (including `]`).
fn parse_class(chars: &[char]) -> Option<(Token, usize)> {
    let mut i = 0;
    let negated = matches!(chars.first(), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    while i < chars.len() {
        let c = chars[i];
        if c == ']' && !first {
            return Some((Token::Class { negated, ranges }, i + 1));
        }
        first = false;
        if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] != ']' {
            ranges.push((c, chars[i + 2]));
            i += 3;
        } else {
            ranges.push((c, c));
            i += 1;
        }
    }
    None
}

/// Glob matching by dynamic programming over (token, char) positions.
fn glob_match(tokens: &[Token], text: &[char]) -> bool {
    // dp[t] = can tokens[ti..] match text[t..]; computed from the last token back.
    let n = text.len();
    let mut next = vec![false; n + 1];
    next[n] = true;
    for token in tokens.iter().rev() {
        let mut cur = vec![false; n + 1];
        for t in (0..=n).rev() {
            cur[t] = match token {
                Token::Char(c) => t < n && text[t] == *c && next[t + 1],
                Token::Any => t < n && text[t] != '/' && next[t + 1],
                Token::Class { negated, ranges } => {
                    t < n
                        && text[t] != '/'
                        && ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&text[t])) != *negated
                        && next[t + 1]
                }
                Token::Star => next[t] || (t < n && text[t] != '/' && cur[t + 1]),
                Token::DoubleStar => next[t] || (t < n && cur[t + 1]),
                // Zero dirs, or skip through the next `/` and try again.
                Token::DirStar => {
                    next[t] || text[t..].iter().position(|&c| c == '/').is_some_and(|p| cur[t + p + 1])
                }
            };
        }
        next = cur;
    }
    next[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignored(rules: &str, path: &str, is_dir: bool) -> bool {
        let g = Gitignore::parse(Path::new("."), rules);
        is_ignored(&[g], Path::new(path), is_dir)
    }

    fn glob(pattern: &str, text: &str) -> bool {
        glob_match(&tokenize(pattern), &text.chars().collect::<Vec<_>>())
    }

    #[test]
    fn globs() {
        assert!(glob("*.log", "a.log"));
        assert!(!glob("*.log", "a.log.txt"));
        assert!(!glob("*.log", "dir/a.log"), "* does not cross /");
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "a/c"));
        assert!(glob("[ab]x", "bx"));
        assert!(glob("[a-c]x", "cx"));
        assert!(!glob("[!a-c]x", "cx"));
        assert!(glob("[!a-c]x", "dx"));
        assert!(glob(r"\*x", "*x"));
        assert!(!glob(r"\*x", "ax"));
        assert!(glob("[unclosed", "[unclosed"));
    }

    #[test]
    fn double_star() {
        assert!(glob("**/foo", "foo"));
        assert!(glob("**/foo", "a/b/foo"));
        assert!(!glob("**/foo", "afoo"));
        assert!(glob("a/**/b", "a/b"));
        assert!(glob("a/**/b", "a/x/y/b"));
        assert!(glob("a/**", "a/x/y"));
        assert!(!glob("a/**", "b/x"));
    }

    #[test]
    fn name_patterns_match_at_any_depth() {
        assert!(ignored("*.log", "a.log", false));
        assert!(ignored("*.log", "deep/dir/a.log", false));
        assert!(ignored("build", "x/build", true));
        assert!(!ignored("*.log", "a.txt", false));
    }

    #[test]
    fn anchored_patterns() {
        assert!(ignored("/build", "build", true));
        assert!(!ignored("/build", "src/build", true));
        assert!(ignored("doc/*.txt", "doc/a.txt", false));
        assert!(!ignored("doc/*.txt", "x/doc/a.txt", false));
    }

    #[test]
    fn directory_only_patterns() {
        assert!(ignored("out/", "out", true));
        assert!(!ignored("out/", "out", false));
    }

    #[test]
    fn negation_and_order() {
        let rules = "*.log\n!keep.log\n";
        assert!(ignored(rules, "a.log", false));
        assert!(!ignored(rules, "keep.log", false));
        assert!(ignored("!keep.log\n*.log", "keep.log", false), "last rule wins");
    }

    #[test]
    fn comments_blank_lines_and_escapes() {
        let rules = "# comment\n\n   \n\\#file\n\\!bang\ntrailing   \nescaped\\ \n";
        assert!(ignored(rules, "#file", false));
        assert!(ignored(rules, "!bang", false));
        assert!(ignored(rules, "trailing", false));
        assert!(ignored(rules, "escaped ", false));
        assert!(!ignored(rules, "comment", false));
        assert!(Gitignore::parse(Path::new("."), "# only\n\n").rules.is_empty());
    }

    #[test]
    fn crlf_lines() {
        assert!(ignored("*.tmp\r\nbuild/\r\n", "x.tmp", false));
        assert!(ignored("*.tmp\r\nbuild/\r\n", "build", true));
    }

    #[test]
    fn nested_gitignore_overrides_outer() {
        let outer = Gitignore::parse(Path::new("."), "*.gen\n");
        let inner = Gitignore::parse(Path::new("src"), "!keep.gen\n/local.txt\n");
        let stack = [outer, inner];
        assert!(is_ignored(&stack, Path::new("a.gen"), false));
        assert!(!is_ignored(&stack, Path::new("src/keep.gen"), false));
        assert!(is_ignored(&stack, Path::new("src/other.gen"), false));
        assert!(is_ignored(&stack, Path::new("src/local.txt"), false));
        assert!(!is_ignored(&stack, Path::new("local.txt"), false), "anchored to src/");
    }

    #[test]
    fn windows_paths_are_normalized() {
        let g = Gitignore::parse(Path::new("root"), "/sub/*.o\n");
        assert!(is_ignored(&[g], &Path::new("root").join("sub").join("x.o"), false));
    }
}
