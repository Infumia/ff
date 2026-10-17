//! Recursive directory listing.
//!
//! Directories are visited depth-first with entries sorted by name, so the
//! output order is deterministic. Hidden entries (dot-prefixed) and paths
//! matched by `.gitignore` files are skipped unless requested, and a few
//! well-known heavy directories are always skipped.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;

use crate::ignore::{is_ignored, Gitignore};

/// Directory names that are never descended into.
pub const ALWAYS_SKIPPED: &[&str] = &[".git", ".hg", ".svn", "node_modules", "target"];

const BATCH_SIZE: usize = 512;

#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// Include dot-prefixed files and directories.
    pub hidden: bool,
    /// Do not read `.gitignore` files.
    pub no_ignore: bool,
}

pub enum WalkMsg {
    Batch(Vec<Arc<str>>),
    Done,
}

/// Walks `root`, calling `emit` with batches of file paths. Paths are shown
/// relative to the current directory when `root` is `.`, otherwise they are
/// prefixed with `root`. Stops early if `emit` returns `false`.
pub fn walk(root: &Path, opts: &WalkOptions, mut emit: impl FnMut(Vec<Arc<str>>) -> bool) {
    // Each directory carries the `.gitignore`s that apply to it, outermost first.
    let mut stack = vec![(root.to_path_buf(), Arc::new(Vec::new()))];
    let mut batch = Vec::with_capacity(BATCH_SIZE);

    while let Some((dir, mut ignores)) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else { continue };
        if !opts.no_ignore {
            if let Some(gitignore) = Gitignore::load(&dir) {
                let mut extended: Vec<Gitignore> = ignores.as_ref().clone();
                extended.push(gitignore);
                ignores = Arc::new(extended);
            }
        }
        let mut entries: Vec<_> = read.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());

        let mut subdirs = Vec::new();
        for entry in entries {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !opts.hidden && name_str.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else { continue };
            let path = child_path(root, &dir, &name);

            let is_dir = if file_type.is_symlink() {
                // Follow symlinks to files, but never to directories (cycles).
                match fs::metadata(&path) {
                    Ok(meta) if meta.is_file() => false,
                    _ => continue,
                }
            } else {
                file_type.is_dir()
            };

            if is_ignored(&ignores, &path, is_dir) {
                continue;
            }
            if is_dir {
                if !ALWAYS_SKIPPED.contains(&name_str.as_ref()) {
                    subdirs.push((path, ignores.clone()));
                }
            } else {
                batch.push(Arc::from(path.to_string_lossy().as_ref()));
                if batch.len() >= BATCH_SIZE && !emit(std::mem::take(&mut batch)) {
                    return;
                }
            }
        }
        // Reverse so the stack pops subdirectories in sorted order.
        stack.extend(subdirs.into_iter().rev());
    }
    if !batch.is_empty() {
        emit(batch);
    }
}

fn child_path(root: &Path, dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    if dir == root && root == Path::new(".") {
        PathBuf::from(name)
    } else {
        dir.join(name)
    }
}

/// Collects every file under `root` synchronously.
pub fn collect(root: &Path, opts: &WalkOptions) -> Vec<Arc<str>> {
    let mut all = Vec::new();
    walk(root, opts, |batch| {
        all.extend(batch);
        true
    });
    all
}

/// Walks `root` on a background thread, streaming batches over a channel.
pub fn spawn(root: PathBuf, opts: WalkOptions) -> Receiver<WalkMsg> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        walk(&root, &opts, |batch| tx.send(WalkMsg::Batch(batch)).is_ok());
        let _ = tx.send(WalkMsg::Done);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    fn names(files: &[Arc<str>], root: &Path) -> Vec<String> {
        files
            .iter()
            .map(|f| {
                Path::new(f.as_ref())
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    fn sample() -> TempDir {
        let t = TempDir::new("walk");
        t.write("b.txt", "");
        t.write("a.txt", "");
        t.write("src/main.rs", "");
        t.write("src/util/mod.rs", "");
        t.write(".hidden/secret.txt", "");
        t.write(".env", "");
        t.write("node_modules/pkg/index.js", "");
        t.write("target/debug/out", "");
        t.write(".git/HEAD", "");
        t
    }

    #[test]
    fn lists_files_sorted_and_skips_hidden_and_heavy_dirs() {
        let t = sample();
        let files = collect(t.path(), &WalkOptions::default());
        assert_eq!(
            names(&files, t.path()),
            vec!["a.txt", "b.txt", "src/main.rs", "src/util/mod.rs"]
        );
    }

    #[test]
    fn hidden_option_includes_dotfiles_but_not_vcs_dirs() {
        let t = sample();
        let files = collect(t.path(), &WalkOptions { hidden: true, ..Default::default() });
        assert_eq!(
            names(&files, t.path()),
            vec![".env", "a.txt", "b.txt", ".hidden/secret.txt", "src/main.rs", "src/util/mod.rs"]
        );
    }

    #[test]
    fn respects_gitignore_files() {
        let t = TempDir::new("walk-ignore");
        t.write(".gitignore", "*.log\nbuild/\n/only_root.txt\n");
        t.write("a.log", "");
        t.write("keep.txt", "");
        t.write("only_root.txt", "");
        t.write("build/out.bin", "");
        t.write("src/only_root.txt", "");
        t.write("src/x.log", "");
        t.write("src/.gitignore", "!x.log\ngen/\n");
        t.write("src/gen/g.rs", "");
        t.write("other/gen/g.rs", "");

        let files = collect(t.path(), &WalkOptions::default());
        assert_eq!(
            names(&files, t.path()),
            vec!["keep.txt", "other/gen/g.rs", "src/only_root.txt", "src/x.log"]
        );

        let all = collect(t.path(), &WalkOptions { no_ignore: true, ..Default::default() });
        assert_eq!(all.len(), 8);
    }

    #[test]
    fn gitignore_works_with_dot_root() {
        // Paths under `.` have no `./` prefix; rules must still apply.
        let t = TempDir::new("walk-ignore-dot");
        t.write(".gitignore", "/skip/\n");
        t.write("skip/a.txt", "");
        t.write("keep/a.txt", "");
        let g = Gitignore::parse(Path::new("."), "/skip/\n");
        assert!(is_ignored(&[g], Path::new("skip"), true));
        let files = collect(t.path(), &WalkOptions::default());
        assert_eq!(names(&files, t.path()), vec!["keep/a.txt"]);
    }

    #[test]
    fn missing_root_yields_nothing() {
        let t = TempDir::new("walk-missing");
        assert!(collect(&t.path().join("nope"), &WalkOptions::default()).is_empty());
    }

    #[test]
    fn batches_are_bounded_and_complete() {
        let t = TempDir::new("walk-batch");
        for i in 0..(BATCH_SIZE * 2 + 7) {
            t.write(&format!("f{i:05}.txt"), "");
        }
        let mut sizes = Vec::new();
        walk(t.path(), &WalkOptions::default(), |b| {
            sizes.push(b.len());
            true
        });
        assert_eq!(sizes, vec![BATCH_SIZE, BATCH_SIZE, 7]);
    }

    #[test]
    fn emit_returning_false_stops_walk() {
        let t = TempDir::new("walk-stop");
        for i in 0..(BATCH_SIZE * 3) {
            t.write(&format!("f{i:05}.txt"), "");
        }
        let mut calls = 0;
        walk(t.path(), &WalkOptions::default(), |_| {
            calls += 1;
            false
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn background_walk_streams_then_finishes() {
        let t = sample();
        let rx = spawn(t.path().to_path_buf(), WalkOptions::default());
        let mut count = 0;
        let mut done = false;
        for msg in rx {
            match msg {
                WalkMsg::Batch(b) => count += b.len(),
                WalkMsg::Done => done = true,
            }
        }
        assert!(done);
        assert_eq!(count, 4);
    }

    #[test]
    fn dot_root_gives_relative_paths() {
        assert_eq!(child_path(Path::new("."), Path::new("."), "a.txt".as_ref()), PathBuf::from("a.txt"));
        assert_eq!(
            child_path(Path::new("."), Path::new("src"), "a.txt".as_ref()),
            Path::new("src").join("a.txt")
        );
        assert_eq!(
            child_path(Path::new("dir"), Path::new("dir"), "a.txt".as_ref()),
            Path::new("dir").join("a.txt")
        );
    }
}
