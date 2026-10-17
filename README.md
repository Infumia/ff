# ff

Tiny interactive fuzzy finder for file names and file contents, written in Rust.

- Fuzzy file-name search with fzf-style operators (`'exact`, `^prefix`, `suffix$`, `!exclude`, `a | b`)
- Literal or regex search inside files (`Tab` / `Ctrl+R`)
- Smart-case matching, `.gitignore` support, tree view, preview pane
- Multi-select, open in editor, copy to clipboard, frecency-ranked history
- Shell integration for PowerShell, bash and zsh (`Ctrl+T` picker, `ffcd`)
- Non-interactive `--filter` mode for scripting

## Install

```sh
cargo install --git https://github.com/infumia/ff
```

## Usage

```sh
ff [OPTIONS] [DIR]
```

Type to filter; `Enter` prints the selection to stdout (`path` in file mode, `path:line` in content mode).

| Option | Description |
| --- | --- |
| `-f, --content` | Start in content mode (search inside files) |
| `-e, --regex` | Treat the query as a regular expression |
| `-t, --tree` | Show results as a directory tree |
| `-q, --query <TEXT>` | Start with TEXT already typed |
| `-s, --scope <TEXT>` | File-name query that limits content search |
| `-H, --hidden` | Include hidden files and directories |
| `-I, --no-ignore` | Do not respect `.gitignore` files |
| `--init <SHELL>` | Print shell integration (`powershell`, `bash`, `zsh`) |
| `--filter <TEXT>` | No UI: print every match and exit |

Run `ff --help` for the full list of options and key bindings.

### Shell integration

```sh
# bash / zsh
eval "$(ff --init bash)"
```

```powershell
# PowerShell
Invoke-Expression (& ff --init powershell | Out-String)
```

## License

MIT
