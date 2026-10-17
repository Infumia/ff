//! `ff`: a tiny interactive fuzzy finder for file names and file contents.

pub mod app;
pub mod cli;
pub mod clipboard;
pub mod content;
pub mod editor;
pub mod fuzzy;
pub mod history;
pub mod ignore;
pub mod matcher;
pub mod preview;
pub mod shell;
pub mod tree;
pub mod tui;
pub mod ui;
pub mod walk;

#[cfg(test)]
mod test_util;

use std::io::{self, Write};

use crate::app::FileFilter;
use crate::cli::Config;
use crate::matcher::Matcher;
use crate::walk::WalkOptions;

/// Non-interactive mode: writes every match for `query` and returns how many
/// were written. File mode prints paths (best first for fuzzy queries);
/// content mode prints `path:line:text` in walk order. With `cfg.tree` the
/// output is drawn as a directory tree instead. An invalid regex is an
/// `InvalidInput` error.
pub fn run_filter(cfg: &Config, query: &str, out: &mut impl Write) -> io::Result<usize> {
    let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid regex: {e}"));
    let files = walk::collect(&cfg.root, &WalkOptions { hidden: cfg.hidden, no_ignore: cfg.no_ignore });
    let root = cfg.root.to_string_lossy();
    let mut lines = Vec::new();
    if cfg.content {
        let Some(matcher) = Matcher::new(query, cfg.regex).map_err(invalid)? else { return Ok(0) };
        let files = match &cfg.scope {
            Some(scope) => {
                let filter = FileFilter::new(scope, cfg.regex).map_err(invalid)?;
                let mut scoped = app::rank_files(&files, &filter, 0..files.len());
                scoped.sort_by_key(|m| m.idx);
                scoped.iter().map(|m| files[m.idx].clone()).collect()
            }
            None => files,
        };
        let matches = content::search_files(&files, &matcher, usize::MAX);
        if cfg.tree {
            let t = tree::build(&root, matches.iter().map(|m| (m.path.as_ref(), true)));
            for row in &t.rows {
                match row.result {
                    Some(i) => lines.push(format!("{}{}: {}", row.guide, matches[i].line, matches[i].text)),
                    None => lines.push(format!("{}{}", row.guide, row.label)),
                }
            }
        } else {
            lines.extend(matches.iter().map(|m| format!("{}:{}:{}", m.path, m.line, m.text)));
        }
        write_lines(out, &lines)?;
        return Ok(matches.len());
    }

    let filter = FileFilter::new(query, cfg.regex).map_err(invalid)?;
    let mut matches = app::rank_files(&files, &filter, 0..files.len());
    if cfg.tree {
        matches.sort_by_key(|m| m.idx);
        let t = tree::build(&root, matches.iter().map(|m| (files[m.idx].as_ref(), false)));
        lines.extend(t.rows.iter().map(|r| format!("{}{}", r.guide, r.label)));
    } else {
        lines.extend(matches.iter().map(|m| files[m.idx].to_string()));
    }
    write_lines(out, &lines)?;
    Ok(matches.len())
}

fn write_lines(out: &mut impl Write, lines: &[String]) -> io::Result<()> {
    for line in lines {
        writeln!(out, "{line}")?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    fn filter(t: &TempDir, content: bool, query: &str) -> (usize, Vec<String>) {
        let cfg = Config { root: t.path().to_path_buf(), content, ..Config::default() };
        let mut buf = Vec::new();
        let n = run_filter(&cfg, query, &mut buf).unwrap();
        let prefix = t.path().to_string_lossy().to_string();
        let lines = String::from_utf8(buf)
            .unwrap()
            .lines()
            .map(|l| l.replace(&prefix, "").replace('\\', "/").trim_start_matches('/').to_string())
            .collect();
        (n, lines)
    }

    #[test]
    fn filter_files_ranks_best_first() {
        let t = TempDir::new("lib-files");
        t.write("src/main.rs", "");
        t.write("src/domain/model.rs", "");
        t.write("README.md", "");
        let (n, lines) = filter(&t, false, "main");
        assert_eq!(n, 2);
        assert_eq!(lines, vec!["src/main.rs", "src/domain/model.rs"]);
    }

    #[test]
    fn filter_content_prints_location_and_text() {
        let t = TempDir::new("lib-content");
        t.write("a.txt", "one\n  TODO: fix\n");
        t.write("b/c.txt", "todo later");
        t.write("bin.dat", b"todo\0\x01");
        let (n, lines) = filter(&t, true, "todo");
        assert_eq!(n, 2);
        assert_eq!(lines, vec!["a.txt:2:TODO: fix", "b/c.txt:1:todo later"]);

        let (n, lines) = filter(&t, true, "TODO");
        assert_eq!((n, lines), (1, vec!["a.txt:2:TODO: fix".to_string()]));
    }

    #[test]
    fn filter_content_with_blank_query_prints_nothing() {
        let t = TempDir::new("lib-blank");
        t.write("a.txt", "x");
        assert_eq!(filter(&t, true, " ").0, 0);
    }

    #[test]
    fn filter_regex_in_both_modes() {
        let t = TempDir::new("lib-regex");
        t.write("src/a.rs", "fn one() {}\nlet x = 1;\n");
        t.write("src/b.txt", "fn two() {}\n");
        let mut cfg = Config { root: t.path().to_path_buf(), regex: true, ..Config::default() };
        let mut buf = Vec::new();
        assert_eq!(run_filter(&cfg, r"\.rs$", &mut buf).unwrap(), 1);
        cfg.content = true;
        let mut buf = Vec::new();
        assert_eq!(run_filter(&cfg, r"^fn \w+\(\)", &mut buf).unwrap(), 2);
        let err = run_filter(&cfg, "fn(", &mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("invalid regex"));
    }

    #[test]
    fn filter_tree_output() {
        let t = TempDir::new("lib-tree");
        t.write("a.txt", "hit\n");
        t.write("src/b.txt", "x\nhit\n");
        t.write("src/sub/c.txt", "hit\n");
        let mut cfg = Config { root: t.path().to_path_buf(), tree: true, ..Config::default() };
        let mut buf = Vec::new();
        assert_eq!(run_filter(&cfg, "", &mut buf).unwrap(), 3);
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("{}/", t.path().display()), "root is one row");
        assert_eq!(
            lines[1..],
            ["\u{251c}\u{2500} a.txt", "\u{2514}\u{2500} src/", "   \u{251c}\u{2500} b.txt", "   \u{2514}\u{2500} sub/", "      \u{2514}\u{2500} c.txt"]
        );

        cfg.content = true;
        let mut buf = Vec::new();
        assert_eq!(run_filter(&cfg, "hit", &mut buf).unwrap(), 3);
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("\u{251c}\u{2500} a.txt\n\u{2502}  \u{2514}\u{2500} 1: hit\n"), "{text}");
        assert!(text.contains("   \u{251c}\u{2500} b.txt\n   \u{2502}  \u{2514}\u{2500} 2: hit\n"), "{text}");
    }

    #[test]
    fn filter_hidden_option() {
        let t = TempDir::new("lib-hidden");
        t.write(".env", "SECRET=1");
        t.write("app.txt", "SECRET=2");
        let mut cfg = Config { root: t.path().to_path_buf(), content: true, ..Config::default() };
        let mut buf = Vec::new();
        assert_eq!(run_filter(&cfg, "SECRET", &mut buf).unwrap(), 1);
        cfg.hidden = true;
        assert_eq!(run_filter(&cfg, "SECRET", &mut Vec::new()).unwrap(), 2);
    }
}
