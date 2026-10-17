//! End-to-end tests of the `ff` binary through its non-interactive mode.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ff-cli-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.0.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ff(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ff")).current_dir(dir).args(args).output().unwrap()
}

fn stdout_lines(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout).lines().map(|l| l.replace('\\', "/")).collect()
}

fn project() -> TempDir {
    let t = TempDir::new(&format!("{:?}", std::thread::current().id()).replace(['(', ')'], ""));
    t.write("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n");
    t.write("src/lib.rs", "pub fn greet() -> &'static str {\n    \"Hello\"\n}\n");
    t.write("docs/guide.md", "# Guide\nSay hello to ff.\n");
    t.write(".secret", "hello from a hidden file\n");
    t.write("node_modules/dep/index.js", "hello\n");
    t
}

#[test]
fn help_and_version() {
    let t = TempDir::new("help");
    let out = ff(&t.0, &["--help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage: ff"));

    let out = ff(&t.0, &["-V"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("ff "));
}

#[test]
fn bad_arguments_exit_with_2() {
    let t = TempDir::new("badargs");
    let out = ff(&t.0, &["--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown option"));
}

#[test]
fn missing_directory_exits_with_2() {
    let t = TempDir::new("nodir");
    let out = ff(&t.0, &["--filter", "x", "does-not-exist"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a directory"));
}

#[test]
fn filter_file_names() {
    let t = project();
    let out = ff(&t.0, &["--filter", "main"]);
    assert!(out.status.success());
    assert_eq!(stdout_lines(&out), vec!["src/main.rs"]);

    let out = ff(&t.0, &["--filter", "src rs"]);
    assert_eq!(stdout_lines(&out), vec!["src/lib.rs", "src/main.rs"]);
}

#[test]
fn filter_lists_everything_for_empty_query() {
    let t = project();
    let out = ff(&t.0, &["--filter", ""]);
    assert_eq!(stdout_lines(&out), vec!["docs/guide.md", "src/lib.rs", "src/main.rs"]);
}

#[test]
fn filter_content_with_f_flag() {
    let t = project();
    let out = ff(&t.0, &["-f", "--filter", "hello"]);
    assert!(out.status.success());
    assert_eq!(
        stdout_lines(&out),
        vec![
            "docs/guide.md:2:Say hello to ff.",
            "src/lib.rs:2:\"Hello\"",
            "src/main.rs:2:println!(\"hello\");",
        ]
    );
}

#[test]
fn content_search_is_smart_case() {
    let t = project();
    let out = ff(&t.0, &["-f", "--filter", "Hello"]);
    assert_eq!(stdout_lines(&out), vec!["src/lib.rs:2:\"Hello\""]);
}

#[test]
fn hidden_flag_includes_dotfiles() {
    let t = project();
    let out = ff(&t.0, &["-fH", "--filter", "hidden file"]);
    assert_eq!(stdout_lines(&out), vec![".secret:1:hello from a hidden file"]);
}

#[test]
fn directory_argument_prefixes_paths() {
    let t = project();
    let out = ff(&t.0, &["--filter", "guide", "docs"]);
    assert_eq!(stdout_lines(&out), vec!["docs/guide.md"]);
}

#[test]
fn regex_flag_for_names_and_contents() {
    let t = project();
    let out = ff(&t.0, &["-e", "--filter", r"^src/(main|lib)\.rs$"]);
    assert_eq!(stdout_lines(&out), vec!["src/lib.rs", "src/main.rs"]);

    let out = ff(&t.0, &["-fe", "--filter", r"^\s*println!"]);
    assert_eq!(stdout_lines(&out), vec!["src/main.rs:2:println!(\"hello\");"]);
}

#[test]
fn invalid_regex_exits_with_2() {
    let t = project();
    let out = ff(&t.0, &["-e", "--filter", "("]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid regex"));
}

#[test]
fn tree_flag_prints_tree() {
    let t = project();
    let out = ff(&t.0, &["-t", "--filter", "rs"]);
    assert!(out.status.success());
    assert_eq!(stdout_lines(&out), vec!["src/", "\u{251c}\u{2500} lib.rs", "\u{2514}\u{2500} main.rs"]);
}

#[test]
fn gitignore_is_respected_unless_disabled() {
    let t = project();
    t.write(".gitignore", "docs/\n");
    let out = ff(&t.0, &["--filter", ""]);
    assert_eq!(stdout_lines(&out), vec!["src/lib.rs", "src/main.rs"]);
    let out = ff(&t.0, &["-I", "--filter", ""]);
    assert_eq!(stdout_lines(&out), vec!["docs/guide.md", "src/lib.rs", "src/main.rs"]);
}

#[test]
fn search_operators() {
    let t = project();
    let out = ff(&t.0, &["--filter", "^src !main"]);
    assert_eq!(stdout_lines(&out), vec!["src/lib.rs"]);
    let out = ff(&t.0, &["--filter", ".md$ | lib.rs$"]);
    assert_eq!(stdout_lines(&out), vec!["src/lib.rs", "docs/guide.md"]);
}

#[test]
fn scope_limits_content_search() {
    let t = project();
    let out = ff(&t.0, &["-f", "-s", ".md$", "--filter", "hello"]);
    assert_eq!(stdout_lines(&out), vec!["docs/guide.md:2:Say hello to ff."]);
}

#[test]
fn init_prints_shell_script() {
    let t = TempDir::new("init");
    let out = ff(&t.0, &["--init", "powershell"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Set-PSReadLineKeyHandler"));
    let out = ff(&t.0, &["--init", "fish"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("supported: powershell, bash, zsh"));
}

#[test]
fn no_match_exits_with_1() {
    let t = project();
    let out = ff(&t.0, &["--filter", "zzzzzz"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}
