//! Copy actions for the read-only preview: the last response, a row's details and the
//! fenced code blocks inside that response.
//!
//! Transcript text is data, never terminal commands, so every payload goes through
//! [`crate::transcript::plain`] before it reaches the clipboard.
use crate::transcript;
use anyhow::{Context, Result, ensure};
use std::{
    io::Write,
    process::{Command, Stdio},
};

/// One offer in the copy menu: what it is called and the text it puts on the clipboard.
#[derive(Debug, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub text: String,
}

/// A fenced block in a reply. `complete` is false when the closing fence is missing, which
/// is what a block still being streamed looks like.
#[derive(Debug, PartialEq, Eq)]
pub struct Block {
    pub lang: String,
    pub text: String,
    pub complete: bool,
}

impl Block {
    /// A block is named by its language and its first content line, so several blocks in one
    /// reply stay apart. An unfinished one says so rather than looking like a short block.
    fn label(&self, n: usize) -> String {
        let first = self
            .text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        let mut label = format!("code {n} · {}", self.lang);
        if !first.is_empty() {
            label.push_str(" · ");
            label.extend(first.chars().take(48));
            if first.chars().count() > 48 {
                label.push('…');
            }
        }
        if !self.complete {
            label.push_str(" · still streaming");
        }
        label
    }
}

/// Fenced blocks in reply order. Text outside a fence is ignored.
pub fn blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut open: Option<Block> = None;
    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        match (&mut open, fence) {
            (None, true) => {
                let lang = line.trim_start().trim_start_matches('`').trim();
                open = Some(Block {
                    lang: if lang.is_empty() {
                        "text".to_owned()
                    } else {
                        lang.to_owned()
                    },
                    text: String::new(),
                    complete: false,
                });
            }
            (Some(_), true) => {
                let mut block = open.take().expect("open block");
                block.complete = true;
                blocks.push(block);
            }
            (Some(block), false) => {
                block.text.push_str(line);
                block.text.push('\n');
            }
            (None, false) => {}
        }
    }
    // A block with no closing fence is still worth copying; it is marked, not dropped.
    blocks.extend(open);
    blocks
}

/// The copy menu for one row: its details always, and the last reply with its code blocks
/// when the preview has actually read one.
pub fn items(details: String, last_reply: Option<&str>) -> Vec<Item> {
    let mut items = Vec::new();
    if let Some(reply) = last_reply {
        let reply = transcript::plain(reply);
        for (i, block) in blocks(&reply).iter().enumerate() {
            items.push(Item {
                label: block.label(i + 1),
                text: block.text.clone(),
            });
        }
        items.insert(
            0,
            Item {
                label: "last response".to_owned(),
                text: reply,
            },
        );
    }
    items.push(Item {
        label: "session details".to_owned(),
        text: transcript::plain(&details),
    });
    items
}

/// The program that reads text for the clipboard on stdin: pbcopy on macOS; on Linux the
/// Wayland or X11 tool for the desktop session, and none outside one, such as over ssh.
fn clipboard_program() -> Option<(&'static str, &'static [&'static str])> {
    if cfg!(target_os = "macos") {
        Some(("pbcopy", &[]))
    } else if desktop("WAYLAND_DISPLAY") {
        Some(("wl-copy", &[]))
    } else if desktop("DISPLAY") {
        Some(("xclip", &["-selection", "clipboard"]))
    } else {
        None
    }
}

/// Whether a Linux desktop session names its display in `variable`.
pub(crate) fn desktop(variable: &str) -> bool {
    std::env::var_os(variable).is_some_and(|v| !v.is_empty())
}

pub fn to_clipboard(text: &str) -> Result<()> {
    let Some((program, args)) = clipboard_program() else {
        return osc52(text);
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .context(program)?;
    child
        .stdin
        .take()
        .context("clipboard stdin")?
        .write_all(text.as_bytes())?;
    ensure!(child.wait()?.success(), "{program} failed");
    Ok(())
}

/// Ask the terminal to set its clipboard (OSC 52), which reaches the desktop across ssh.
// ponytail: unconfirmed; a terminal that ignores OSC 52 drops it silently, and tmux forwards
// it only with `set-clipboard on`.
fn osc52(text: &str) -> Result<()> {
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .context("/dev/tty")?;
    write!(tty, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                TABLE[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OSC 52 carries base64, and no E2E test has a terminal that reads it back. A wrong
    /// padding or bit order puts garbage on the clipboard with nothing reporting it.
    #[test]
    fn osc52_payload_is_rfc_4648_base64() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
            ("—", "4oCU"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain}");
        }
    }

    #[test]
    fn an_unclosed_block_is_offered_and_marked() {
        let found = blocks("```python\nprint(1)\n");
        assert_eq!(found.len(), 1);
        assert!(!found[0].complete);
        assert_eq!(found[0].text, "print(1)\n");
        assert!(
            found[0].label(1).ends_with("· still streaming"),
            "{}",
            found[0].label(1)
        );
    }

    #[test]
    fn menu_payloads_carry_no_terminal_escapes() {
        let reply = "\u{1b}[31mred\u{1b}[0m\n```sh\n\u{1b}]0;title\u{7}echo hi\n```\n";
        let items = items("claude \u{1b}[1m· idle".to_owned(), Some(reply));
        assert_eq!(items[0].label, "last response");
        assert_eq!(items[1].label, "code 1 · sh · echo hi");
        assert_eq!(items[2].label, "session details");
        for item in &items {
            assert!(!item.text.contains('\u{1b}'), "{item:?}");
        }
        assert_eq!(items[1].text, "echo hi\n");
        assert_eq!(items[2].text, "claude · idle");
    }
}
