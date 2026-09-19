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

/// ponytail: pbcopy, the clipboard of the machine cones already targets. OSC 52 is the
/// upgrade path if a dashboard ever runs over ssh.
pub fn to_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("pbcopy")?;
    child
        .stdin
        .take()
        .context("pbcopy stdin")?
        .write_all(text.as_bytes())?;
    ensure!(child.wait()?.success(), "pbcopy failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_keep_their_language_first_line_and_unfinished_state_apart() {
        let reply = "here you go\n```rust\nfn one() {}\n```\nand\n```\nplain two\n```\n";
        let found = blocks(reply);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].lang, "rust");
        assert_eq!(found[0].text, "fn one() {}\n");
        assert_eq!(found[1].lang, "text");
        assert!(found[0].complete && found[1].complete);
        let labels: Vec<String> = found
            .iter()
            .enumerate()
            .map(|(i, b)| b.label(i + 1))
            .collect();
        assert_eq!(labels[0], "code 1 · rust · fn one() {}");
        assert_eq!(labels[1], "code 2 · text · plain two");
        assert_ne!(labels[0], labels[1]);
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

    #[test]
    fn a_row_without_a_read_reply_still_offers_its_details() {
        let items = items("codex · active".to_owned(), None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "session details");
    }
}
