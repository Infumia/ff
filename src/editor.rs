//! Opening results in an editor (Ctrl+O).
//!
//! The editor comes from `FF_EDITOR`, `VISUAL` or `EDITOR`, falling back to
//! VS Code when it is installed, then to notepad (Windows) or vi. Known
//! editors are told the line number in the way they expect.

use std::env;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A file to open, optionally at a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: String,
    pub line: Option<usize>,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{}", self.path, line),
            None => f.write_str(&self.path),
        }
    }
}

/// The editor command line to use, from the environment.
pub fn configured_editor() -> String {
    ["FF_EDITOR", "VISUAL", "EDITOR"]
        .iter()
        .filter_map(|var| env::var(var).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| {
            if find_program("code").is_some() {
                "code".to_string()
            } else if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        })
}

/// Splits `editor` (e.g. `"code --wait"`) and adds the targets in the form
/// the editor understands.
pub fn command_line(editor: &str, targets: &[Target]) -> (String, Vec<String>) {
    let mut words = editor.split_whitespace().map(str::to_string);
    let program = words.next().unwrap_or_default();
    let mut args: Vec<String> = words.collect();
    let name = Path::new(&program)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    for t in targets {
        match (name.as_str(), t.line) {
            ("code" | "code-insiders" | "codium" | "cursor" | "windsurf", Some(line)) => {
                args.push("-g".into());
                args.push(format!("{}:{line}", t.path));
            }
            ("vim" | "nvim" | "vi" | "nano" | "emacs" | "emacsclient" | "kak" | "gedit", Some(line)) => {
                args.push(format!("+{line}"));
                args.push(t.path.clone());
            }
            ("hx" | "helix" | "micro" | "subl" | "sublime_text" | "zed", Some(line)) => {
                args.push(format!("{}:{line}", t.path));
            }
            ("notepad++", Some(line)) => {
                args.push(format!("-n{line}"));
                args.push(t.path.clone());
            }
            _ => args.push(t.path.clone()),
        }
    }
    (program, args)
}

/// Opens the targets and waits for the editor to exit.
pub fn open(targets: &[Target]) -> io::Result<()> {
    let editor = configured_editor();
    let (program, args) = command_line(&editor, targets);
    // On Windows `code` is `code.cmd`, which `Command` only finds by full path.
    let resolved = find_program(&program).unwrap_or_else(|| PathBuf::from(&program));
    let status = Command::new(&resolved)
        .args(&args)
        .status()
        .map_err(|e| io::Error::new(e.kind(), format!("cannot run editor '{program}': {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("editor '{program}' exited with {status}")))
    }
}

/// Finds `name` on `PATH`, trying Windows executable extensions.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = env::split_paths(&env::var_os("PATH").unwrap_or_default()).collect();
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    find_in(&dirs, name, exts)
}

fn find_in(dirs: &[PathBuf], name: &str, exts: &[&str]) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if Path::new(name).components().count() > 1 {
        return Path::new(name).is_file().then(|| PathBuf::from(name));
    }
    dirs.iter()
        .flat_map(|dir| exts.iter().map(move |ext| dir.join(format!("{name}{ext}"))))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    fn t(path: &str, line: Option<usize>) -> Target {
        Target { path: path.into(), line }
    }

    fn cmd(editor: &str, targets: &[Target]) -> Vec<String> {
        let (program, mut args) = command_line(editor, targets);
        args.insert(0, program);
        args
    }

    #[test]
    fn target_display() {
        assert_eq!(t("a.rs", None).to_string(), "a.rs");
        assert_eq!(t("a.rs", Some(3)).to_string(), "a.rs:3");
    }

    #[test]
    fn vscode_family_uses_goto() {
        assert_eq!(cmd("code", &[t("a.rs", Some(3))]), ["code", "-g", "a.rs:3"]);
        assert_eq!(cmd("code --wait", &[t("a.rs", None)]), ["code", "--wait", "a.rs"]);
        assert_eq!(cmd(r"C:\bin\Cursor.exe", &[t("a", Some(1))]), [r"C:\bin\Cursor.exe", "-g", "a:1"]);
    }

    #[test]
    fn terminal_editors_use_plus_line() {
        assert_eq!(cmd("nvim", &[t("a.rs", Some(12))]), ["nvim", "+12", "a.rs"]);
        assert_eq!(cmd("vim -p", &[t("a", Some(1)), t("b", None)]), ["vim", "-p", "+1", "a", "b"]);
    }

    #[test]
    fn other_line_syntaxes() {
        assert_eq!(cmd("hx", &[t("a.rs", Some(4))]), ["hx", "a.rs:4"]);
        assert_eq!(cmd("notepad++", &[t("a.rs", Some(4))]), ["notepad++", "-n4", "a.rs"]);
        assert_eq!(cmd("notepad", &[t("a.rs", Some(4))]), ["notepad", "a.rs"], "no line support");
    }

    #[test]
    fn finds_programs_with_extensions() {
        let tmp = TempDir::new("editor");
        tmp.write("bin/mycode.cmd", "");
        tmp.write("bin/plain", "");
        let dirs = vec![tmp.path().join("nope"), tmp.path().join("bin")];
        assert_eq!(find_in(&dirs, "mycode", &[".exe", ".cmd"]), Some(tmp.path().join("bin").join("mycode.cmd")));
        assert_eq!(find_in(&dirs, "plain", &[""]), Some(tmp.path().join("bin").join("plain")));
        assert_eq!(find_in(&dirs, "missing", &[".exe", ""]), None);
        assert_eq!(find_in(&dirs, "", &[""]), None);
        let full = tmp.path().join("bin").join("plain");
        assert_eq!(find_in(&[], &full.to_string_lossy(), &[""]), Some(full));
    }
}
