//! Shell integration scripts printed by `ff --init <shell>`.
//!
//! Each script binds Ctrl+T to insert the paths picked with `ff` at the
//! cursor, and defines `ffcd` (go to the picked file's directory).

pub const SHELLS: &[&str] = &["powershell", "bash", "zsh"];

const POWERSHELL: &str = r#"# ff shell integration. Add to $PROFILE:
#   Invoke-Expression (& ff --init powershell | Out-String)
Set-PSReadLineKeyHandler -Chord 'Ctrl+t' -BriefDescription 'ff' -Description 'Insert paths picked with ff' -ScriptBlock {
    $picked = @(& ff)
    if ($LASTEXITCODE -eq 0 -and $picked.Count -gt 0) {
        $quoted = $picked | ForEach-Object { if ($_ -match '[\s''`$]') { "'" + ($_ -replace "'", "''") + "'" } else { $_ } }
        [Microsoft.PowerShell.PSConsoleReadLine]::Insert(($quoted -join ' '))
    }
    [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
}
function ffcd {
    $picked = @(& ff @args)
    if ($LASTEXITCODE -eq 0 -and $picked.Count -gt 0) {
        $dir = Split-Path -Parent ($picked[0] -replace ':\d+$', '')
        if ($dir) { Set-Location $dir }
    }
}
"#;

const BASH: &str = r#"# ff shell integration. Add to ~/.bashrc:
#   eval "$(ff --init bash)"
__ff_insert() {
    local picked
    picked="$(ff | while IFS= read -r line; do printf '%q ' "$line"; done)"
    READLINE_LINE="${READLINE_LINE:0:$READLINE_POINT}$picked${READLINE_LINE:$READLINE_POINT}"
    READLINE_POINT=$((READLINE_POINT + ${#picked}))
}
bind -x '"\C-t": __ff_insert'
ffcd() {
    local picked
    picked="$(ff "$@" | head -n 1 | sed 's/:[0-9]*$//')" && [ -n "$picked" ] && cd "$(dirname "$picked")"
}
"#;

const ZSH: &str = r#"# ff shell integration. Add to ~/.zshrc:
#   eval "$(ff --init zsh)"
__ff_insert() {
    local picked
    picked="$(ff < /dev/tty | while IFS= read -r line; do printf '%q ' "$line"; done)"
    LBUFFER+="$picked"
    zle reset-prompt
}
zle -N __ff_insert
bindkey '^T' __ff_insert
ffcd() {
    local picked
    picked="$(ff "$@" | head -n 1 | sed 's/:[0-9]*$//')" && [ -n "$picked" ] && cd "$(dirname "$picked")"
}
"#;

/// The integration script for `shell`, or `None` if unsupported.
pub fn script(shell: &str) -> Option<&'static str> {
    match shell.to_ascii_lowercase().as_str() {
        "powershell" | "pwsh" => Some(POWERSHELL),
        "bash" => Some(BASH),
        "zsh" => Some(ZSH),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_shell_has_a_script() {
        for shell in SHELLS {
            let s = script(shell).unwrap();
            assert!(s.contains("ffcd"), "{shell}");
            assert!(s.contains("ff --init"), "{shell} explains setup");
        }
        assert_eq!(script("pwsh"), script("PowerShell"));
        assert_eq!(script("fish"), None);
    }

    #[test]
    fn scripts_bind_ctrl_t() {
        assert!(script("powershell").unwrap().contains("-Chord 'Ctrl+t'"));
        assert!(script("bash").unwrap().contains(r#"bind -x '"\C-t": __ff_insert'"#));
        assert!(script("zsh").unwrap().contains("bindkey '^T' __ff_insert"));
    }

    #[test]
    fn ffcd_strips_line_numbers() {
        assert!(script("powershell").unwrap().contains(r"-replace ':\d+$', ''"));
        assert!(script("bash").unwrap().contains(r"sed 's/:[0-9]*$//'"));
    }
}
