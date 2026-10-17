//! Command-line parsing (hand-rolled to avoid a dependency).

use std::path::PathBuf;

pub const USAGE: &str = "\
ff - fuzzy find files by name or by content

Usage: ff [OPTIONS] [DIR]

Opens an interactive finder in DIR (default: current directory). Type to
filter; Enter prints the selection to stdout and exits:
  file mode     path
  content mode  path:line

Options:
  -f, --content        Start in content mode (search inside files)
  -e, --regex          Treat the query as a regular expression
  -t, --tree           Show results as a directory tree
  -q, --query <TEXT>   Start with TEXT already typed
  -s, --scope <TEXT>   File-name query that limits content search
                       (same as typing TEXT in file mode, then Tab)
  -H, --hidden         Include hidden (dot) files and directories
  -I, --no-ignore      Do not respect .gitignore files
      --no-mouse       Leave the mouse to the terminal (text selection)
      --no-history     Do not read or write the history file
      --init <SHELL>   Print shell integration (powershell, bash, zsh):
                       Ctrl+T inserts picked paths, `ffcd` jumps to one
      --filter <TEXT>  No UI: print every match for TEXT and exit
                       (file mode: one path per line,
                        content mode: path:line:text)
  -h, --help           Print this help
  -V, --version        Print version

Keys:
  Tab, Ctrl+F          Switch between file-name and content search
  Ctrl+R               Toggle regex mode
  Ctrl+L               Toggle tree view
  Ctrl+T               Show/hide the preview pane (right side)
  Up/Down, Ctrl+P/N    Move selection (PgUp/PgDn: one page)
  Shift+Up/Down        Scroll the preview (Shift+PgUp/PgDn: one page)
  Mouse                Wheel moves the selection or scrolls the preview,
                       click selects
  Enter                Print selection (or all marked entries) and exit
  Ctrl+S, Ctrl+Space   Mark/unmark entry (multi-select)
  Ctrl+O               Open selection (or marked entries) in the editor:
                       $FF_EDITOR, $VISUAL, $EDITOR, else VS Code if found
  Ctrl+Y               Copy selection (or marked entries) to the clipboard
  Esc, Ctrl+C          Exit without selecting
  Left/Right, Home/End Move cursor in the query
  Ctrl+U, Ctrl+W       Clear to start / delete previous word
  Alt+Up/Down          Previous/next query from the history (or Ctrl+Up/Down)

File mode is fuzzy: 'srcmn' finds src/main.rs; space-separated words must
all match. Content mode is a literal substring search. In regex mode both
use the query as a regex (paths are matched with '/' separators). All
searches are smart-case: case-insensitive unless the query contains an
uppercase letter.
Paths in .gitignore files are skipped (use -I to include them);
.git, node_modules and target directories are always skipped.
Files you pick often or recently rank higher (history: $FF_HISTORY, or
%LOCALAPPDATA%\\ff\\history, or ~/.local/share/ff/history).

File-name search operators (fzf syntax):
  'word   exact substring        ^src    path starts with
  .rs$    path ends with         !test   exclude paths containing
  a | b   either term matches
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub root: PathBuf,
    pub content: bool,
    pub query: String,
    pub hidden: bool,
    pub no_ignore: bool,
    pub no_mouse: bool,
    pub no_history: bool,
    pub regex: bool,
    pub tree: bool,
    pub scope: Option<String>,
    pub filter: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            root: PathBuf::from("."),
            content: false,
            query: String::new(),
            hidden: false,
            no_ignore: false,
            no_mouse: false,
            no_history: false,
            regex: false,
            tree: false,
            scope: None,
            filter: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Config),
    Help,
    Version,
    /// Print the integration script for a shell.
    Init(String),
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut cfg = Config::default();
    let mut root: Option<PathBuf> = None;
    let mut args = args.into_iter();
    let mut positional_only = false;

    while let Some(arg) = args.next() {
        if positional_only || !arg.starts_with('-') || arg == "-" {
            if root.replace(PathBuf::from(&arg)).is_some() {
                return Err(format!("unexpected argument '{arg}' (only one DIR is allowed)"));
            }
        } else if arg == "--" {
            positional_only = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let mut value = |inline: Option<String>| {
                inline.or_else(|| args.next()).ok_or_else(|| format!("option '--{name}' needs a value"))
            };
            match name {
                "query" => cfg.query = value(inline)?,
                "filter" => cfg.filter = Some(value(inline)?),
                "init" => return Ok(Command::Init(value(inline)?)),
                "scope" => cfg.scope = Some(value(inline)?),
                _ if inline.is_some() => return Err(format!("option '--{name}' does not take a value")),
                "content" => cfg.content = true,
                "hidden" => cfg.hidden = true,
                "no-ignore" => cfg.no_ignore = true,
                "no-mouse" => cfg.no_mouse = true,
                "no-history" => cfg.no_history = true,
                "regex" => cfg.regex = true,
                "tree" => cfg.tree = true,
                "help" => return Ok(Command::Help),
                "version" => return Ok(Command::Version),
                _ => return Err(format!("unknown option '--{name}'")),
            }
        } else {
            // A cluster of short flags, e.g. `-fH` or `-qfoo`.
            let flags: Vec<char> = arg[1..].chars().collect();
            for (i, &flag) in flags.iter().enumerate() {
                match flag {
                    'f' => cfg.content = true,
                    'H' => cfg.hidden = true,
                    'I' => cfg.no_ignore = true,
                    'e' => cfg.regex = true,
                    't' => cfg.tree = true,
                    'h' => return Ok(Command::Help),
                    'V' => return Ok(Command::Version),
                    'q' | 's' => {
                        let rest: String = flags[i + 1..].iter().collect();
                        let value = if rest.is_empty() {
                            args.next().ok_or(format!("option '-{flag}' needs a value"))?
                        } else {
                            rest
                        };
                        if flag == 'q' {
                            cfg.query = value;
                        } else {
                            cfg.scope = Some(value);
                        }
                        break;
                    }
                    other => return Err(format!("unknown option '-{other}'")),
                }
            }
        }
    }
    if let Some(root) = root {
        cfg.root = root;
    }
    Ok(Command::Run(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Config {
        match parse(args.iter().map(|s| s.to_string())) {
            Ok(Command::Run(cfg)) => cfg,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    fn err(args: &[&str]) -> String {
        parse(args.iter().map(|s| s.to_string())).unwrap_err()
    }

    #[test]
    fn defaults() {
        assert_eq!(run(&[]), Config::default());
        assert_eq!(Config::default().root, PathBuf::from("."));
    }

    #[test]
    fn content_flag() {
        assert!(run(&["-f"]).content);
        assert!(run(&["--content"]).content);
    }

    #[test]
    fn directory_argument() {
        let cfg = run(&["-f", "some/dir"]);
        assert_eq!(cfg.root, PathBuf::from("some/dir"));
        assert!(cfg.content);
        assert_eq!(run(&["some/dir", "-H"]).root, PathBuf::from("some/dir"));
    }

    #[test]
    fn query_forms() {
        assert_eq!(run(&["-q", "foo bar"]).query, "foo bar");
        assert_eq!(run(&["-qfoo"]).query, "foo");
        assert_eq!(run(&["--query", "x"]).query, "x");
        assert_eq!(run(&["--query=a=b"]).query, "a=b");
        assert_eq!(run(&["-q", "-f"]).query, "-f", "value may start with a dash");
        let cfg = run(&["-fq", "x"]);
        assert!(cfg.content);
        assert_eq!(cfg.query, "x");
    }

    #[test]
    fn combined_short_flags() {
        let cfg = run(&["-fH"]);
        assert!(cfg.content && cfg.hidden);
        let cfg = run(&["-fet"]);
        assert!(cfg.content && cfg.regex && cfg.tree && !cfg.hidden);
    }

    #[test]
    fn regex_and_tree_flags() {
        assert!(run(&["-e"]).regex);
        assert!(run(&["--regex"]).regex);
        assert!(run(&["-t"]).tree);
        assert!(run(&["--tree"]).tree);
        assert!(!run(&[]).regex && !run(&[]).tree);
    }

    #[test]
    fn scope_option() {
        assert_eq!(run(&["-s", ".rs$"]).scope, Some(".rs$".into()));
        assert_eq!(run(&["--scope=src"]).scope, Some("src".into()));
        assert_eq!(run(&["-fs", "x"]).scope, Some("x".into()));
        assert!(err(&["-s"]).contains("'-s' needs a value"));
    }

    #[test]
    fn no_ignore_flag() {
        assert!(run(&["-I"]).no_ignore);
        assert!(run(&["--no-ignore"]).no_ignore);
        assert!(!run(&[]).no_ignore);
        assert!(run(&["--no-mouse"]).no_mouse);
        assert!(run(&["--no-history"]).no_history);
    }

    #[test]
    fn init_command() {
        assert_eq!(parse(["--init".to_string(), "bash".to_string()]), Ok(Command::Init("bash".into())));
        assert_eq!(parse(["--init=zsh".to_string()]), Ok(Command::Init("zsh".into())));
        assert!(err(&["--init"]).contains("needs a value"));
    }

    #[test]
    fn filter_option() {
        assert_eq!(run(&["--filter", "abc"]).filter, Some("abc".into()));
        assert_eq!(run(&["--filter="]).filter, Some(String::new()));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(run(&["--", "-weird-dir"]).root, PathBuf::from("-weird-dir"));
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse(["-h".to_string()]), Ok(Command::Help));
        assert_eq!(parse(["--help".to_string()]), Ok(Command::Help));
        assert_eq!(parse(["-V".to_string()]), Ok(Command::Version));
        assert_eq!(parse(["--version".to_string()]), Ok(Command::Version));
    }

    #[test]
    fn errors() {
        assert!(err(&["-x"]).contains("unknown option '-x'"));
        assert!(err(&["--nope"]).contains("unknown option '--nope'"));
        assert!(err(&["-q"]).contains("needs a value"));
        assert!(err(&["--query"]).contains("needs a value"));
        assert!(err(&["--hidden=yes"]).contains("does not take a value"));
        assert!(err(&["a", "b"]).contains("only one DIR"));
    }

    #[test]
    fn usage_mentions_every_option() {
        for opt in [
            "--content", "--regex", "--tree", "--query", "--scope", "--hidden", "--no-ignore", "--no-mouse",
            "--no-history", "--init", "--filter", "--help", "--version",
        ] {
            assert!(USAGE.contains(opt), "{opt}");
        }
    }
}
