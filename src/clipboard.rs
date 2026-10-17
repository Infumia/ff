//! Copying text to the system clipboard without extra dependencies.
//!
//! Uses the platform tool (`clip.exe`, `pbcopy`, `wl-copy`, `xclip`,
//! `xsel`); if none works, falls back to the OSC 52 escape sequence, which
//! most modern terminals (Windows Terminal, iTerm2, kitty, ...) understand.

use std::io::{self, Write};
use std::process::{Command, Stdio};

/// Copies `text`; returns how it was done, e.g. `"clip.exe"`.
pub fn copy(text: &str) -> io::Result<&'static str> {
    for (program, args, utf16) in tools() {
        if pipe_to(program, args, &encode(text, *utf16)).is_ok() {
            return Ok(program);
        }
    }
    let mut err = io::stderr();
    err.write_all(osc52(text).as_bytes())?;
    err.flush()?;
    Ok("terminal (OSC 52)")
}

type Tool = (&'static str, &'static [&'static str], bool);

/// Clipboard programs to try, with their arguments and whether they want
/// UTF-16 input.
fn tools() -> &'static [Tool] {
    if cfg!(windows) {
        // clip.exe reads UTF-16LE when the input starts with a BOM.
        &[("clip.exe", &[], true)]
    } else if cfg!(target_os = "macos") {
        &[("pbcopy", &[], false)]
    } else {
        &[
            ("wl-copy", &[], false),
            ("xclip", &["-selection", "clipboard"], false),
            ("xsel", &["--clipboard", "--input"], false),
        ]
    }
}

fn pipe_to(program: &str, args: &[&str], input: &[u8]) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child.stdin.take().expect("piped stdin").write_all(input)?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} failed")))
    }
}

fn encode(text: &str, utf16: bool) -> Vec<u8> {
    if !utf16 {
        return text.as_bytes().to_vec();
    }
    let mut out = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// The OSC 52 "set clipboard" sequence for `text`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let cases = [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")];
        for (input, expected) in cases {
            assert_eq!(base64(input.as_bytes()), expected, "{input:?}");
        }
        assert_eq!(base64("ğ".as_bytes()), "xJ8=");
    }

    #[test]
    fn osc52_sequence() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn utf16_encoding_has_bom() {
        assert_eq!(encode("aş", true), vec![0xFF, 0xFE, b'a', 0, 0x5F, 0x01]);
        assert_eq!(encode("aş", false), "aş".as_bytes());
    }

    #[test]
    fn missing_tool_is_an_error() {
        assert!(pipe_to("ff-no-such-clipboard-tool", &[], b"x").is_err());
    }
}
