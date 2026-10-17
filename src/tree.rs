//! Groups results into a directory tree for display.
//!
//! Input entries must be in walk order (each directory's files, then its
//! subdirectories, depth-first), which keeps every directory's entries
//! contiguous. Directory rows are inserted whenever the path changes
//! directory; in content mode each file also gets a header row and its
//! matching lines become leaves.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    /// A matching line in content mode.
    Line,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub kind: Kind,
    pub depth: usize,
    /// Guides drawn before the label, e.g. `│  └─ `.
    pub guide: String,
    /// Directory name with a trailing `/`, or a file name. Empty for lines.
    pub label: String,
    /// Index into the result list for rows that stand for a result.
    pub result: Option<usize>,
    /// File rows: char index in the full path where `label` starts.
    pub name_start: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    pub rows: Vec<TreeRow>,
    /// Row index of each result.
    pub row_of_result: Vec<usize>,
}

/// Builds the tree from `(path, is_line)` entries, one per result. Paths
/// under `root` (the searched directory, unless it is `.`) are shown below a
/// single row for the root instead of one row per root component.
pub fn build<'a>(root: &'a str, entries: impl IntoIterator<Item = (&'a str, bool)>) -> Tree {
    let mut tree = Tree::default();
    let mut open: Vec<&str> = Vec::new();
    let mut open_file: Option<&str> = None;
    let root = root.trim_end_matches(['/', '\\']);

    for (i, (path, is_line)) in entries.into_iter().enumerate() {
        let (dirs, name, name_start) = split(root, path);
        let common = open.iter().zip(&dirs).take_while(|(a, b)| a == b).count();
        if common < open.len() || common < dirs.len() {
            open_file = None;
        }
        open.truncate(common);
        for &dir in &dirs[common..] {
            tree.rows.push(row(Kind::Dir, open.len(), format!("{dir}/"), None, 0));
            open.push(dir);
        }

        let depth = dirs.len();
        if is_line {
            if open_file != Some(path) {
                tree.rows.push(row(Kind::File, depth, name.to_string(), None, name_start));
                open_file = Some(path);
            }
            tree.row_of_result.push(tree.rows.len());
            tree.rows.push(row(Kind::Line, depth + 1, String::new(), Some(i), 0));
        } else {
            tree.row_of_result.push(tree.rows.len());
            tree.rows.push(row(Kind::File, depth, name.to_string(), Some(i), name_start));
        }
    }
    add_guides(&mut tree.rows);
    tree
}

fn row(kind: Kind, depth: usize, label: String, result: Option<usize>, name_start: usize) -> TreeRow {
    TreeRow { kind, depth, guide: String::new(), label, result, name_start }
}

/// Splits a path into directory components, the file name, and the char
/// index where the file name starts. Accepts `/` and `\`. A `root` prefix
/// stays one component.
fn split<'a>(root: &'a str, path: &'a str) -> (Vec<&'a str>, &'a str, usize) {
    let (mut parts, rest) = match path.strip_prefix(root) {
        Some(rest) if !root.is_empty() && root != "." && rest.starts_with(['/', '\\']) => (vec![root], rest),
        _ => (Vec::new(), path),
    };
    parts.extend(rest.split(['/', '\\']).filter(|p| !p.is_empty()));
    let name = parts.pop().unwrap_or("");
    let name_start = path.chars().count() - name.chars().count();
    (parts, name, name_start)
}

/// Fills in `guide` for every row. Walking bottom-up, `more[k]` records
/// whether a later row at depth `k` exists under the same parent, which
/// decides between `├─`/`└─` and whether an ancestor's `│` continues.
fn add_guides(rows: &mut [TreeRow]) {
    let mut more: Vec<bool> = Vec::new();
    for row in rows.iter_mut().rev() {
        let d = row.depth;
        if more.len() <= d {
            more.resize(d + 1, false);
        }
        let mut guide = String::new();
        if d > 0 {
            for &continues in &more[1..d] {
                guide.push_str(if continues { "│  " } else { "   " });
            }
            guide.push_str(if more[d] { "├─ " } else { "└─ " });
        }
        row.guide = guide;
        more[d] = true;
        // Deeper rows above this one belong to an earlier sibling.
        more.truncate(d + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(tree: &Tree) -> Vec<String> {
        tree.rows
            .iter()
            .map(|r| match r.kind {
                Kind::Line => format!("{}#{}", r.guide, r.result.unwrap()),
                _ => format!("{}{}", r.guide, r.label),
            })
            .collect()
    }

    fn files(paths: &[&'static str]) -> Tree {
        build(".", paths.iter().map(|&p| (p, false)))
    }

    #[test]
    fn builds_tree_with_guides() {
        let t = files(&["Cargo.toml", "src/app.rs", "src/ui.rs", "src/util/mod.rs", "src/util/x.rs", "tests/cli.rs"]);
        assert_eq!(
            render(&t),
            vec![
                "Cargo.toml",
                "src/",
                "├─ app.rs",
                "├─ ui.rs",
                "└─ util/",
                "   ├─ mod.rs",
                "   └─ x.rs",
                "tests/",
                "└─ cli.rs",
            ]
        );
    }

    #[test]
    fn vertical_guides_continue_past_nested_dirs() {
        let t = files(&["a/b/c/deep.rs", "a/b/mid.rs", "a/top.rs"]);
        // Walk order puts files before subdirectories, but the builder only
        // relies on directories being contiguous.
        assert_eq!(
            render(&t),
            vec!["a/", "├─ b/", "│  ├─ c/", "│  │  └─ deep.rs", "│  └─ mid.rs", "└─ top.rs"]
        );
    }

    #[test]
    fn result_rows_map_back_to_results() {
        let t = files(&["x/a", "x/b", "y/c"]);
        assert_eq!(t.row_of_result, vec![1, 2, 4]);
        for (i, &r) in t.row_of_result.iter().enumerate() {
            assert_eq!(t.rows[r].result, Some(i));
        }
        assert!(t.rows[0].result.is_none() && t.rows[3].result.is_none());
    }

    #[test]
    fn content_lines_are_grouped_under_files() {
        let t = build(".", [("src/a.rs", true), ("src/a.rs", true), ("src/b.rs", true), ("top.md", true)]);
        assert_eq!(
            render(&t),
            vec!["src/", "├─ a.rs", "│  ├─ #0", "│  └─ #1", "└─ b.rs", "   └─ #2", "top.md", "└─ #3"]
        );
        assert_eq!(t.row_of_result, vec![2, 3, 5, 7]);
        assert_eq!(t.rows[1].kind, Kind::File);
        assert_eq!(t.rows[1].result, None, "file headers are not results in content mode");
    }

    #[test]
    fn same_file_name_in_different_dirs_gets_new_header() {
        let t = build(".", [("a/mod.rs", true), ("b/mod.rs", true)]);
        assert_eq!(render(&t), vec!["a/", "└─ mod.rs", "   └─ #0", "b/", "└─ mod.rs", "   └─ #1"]);
    }

    #[test]
    fn windows_separators_and_name_start() {
        let t = files(&["src\\ui.rs", "ğ\\ü.rs"]);
        assert_eq!(render(&t), vec!["src/", "└─ ui.rs", "ğ/", "└─ ü.rs"]);
        assert_eq!(t.rows[1].name_start, 4);
        assert_eq!(t.rows[3].name_start, 2, "char index, not byte index");
    }

    #[test]
    fn root_prefix_becomes_top_dirs() {
        let t = files(&["proj/src/a.rs", "proj/src/b.rs"]);
        assert_eq!(render(&t), vec!["proj/", "└─ src/", "   ├─ a.rs", "   └─ b.rs"]);
    }

    #[test]
    fn searched_root_is_a_single_row() {
        let paths = [r"C:\tmp\proj\a.rs", r"C:\tmp\proj\src\b.rs"];
        let t = build(r"C:\tmp\proj\", paths.iter().map(|&p| (p, false)));
        assert_eq!(render(&t), vec![r"C:\tmp\proj/", "├─ a.rs", "└─ src/", "   └─ b.rs"]);
        // A sibling that merely shares the prefix is not under the root.
        let t = build("pro", [("project/x.rs", false)]);
        assert_eq!(render(&t), vec!["project/", "└─ x.rs"]);
    }

    #[test]
    fn empty_input() {
        assert_eq!(files(&[]), Tree::default());
    }
}
