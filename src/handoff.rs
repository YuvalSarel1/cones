//! Explicit cross-harness handoff: a bounded export of one session's conversation,
//! used as the first prompt of a fresh native session in another harness.
//!
//! This is neither a fork nor a resume. Nothing native crosses over: no session id,
//! no account, no credentials, no permission state. The target reads text and starts
//! its own conversation under its own identity, so the export says so in its header
//! and names what was left out. The source session is untouched and stays usable.
use crate::{
    config::HarnessKind,
    harness,
    transcript::{Role, Transcript},
};
use std::path::Path;

/// Export budget. Large enough for a working handoff, small enough that every harness
/// accepts it as one launch prompt.
pub const BUDGET: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    pub text: String,
    /// Messages carried over.
    pub included: usize,
    /// Messages the budget or the read window left behind.
    pub omitted: usize,
    /// The oldest included message lost its beginning to the budget.
    pub truncated: bool,
}

/// Only a harness whose transcripts cones reads natively can be a source. OpenCode keeps
/// its conversation in a database rather than a readable transcript file, so it is not one.
pub fn source_supported(harness: &str) -> bool {
    harness::by_name(harness).is_some_and(|s| s.transcript.available) && harness != "opencode"
}

/// Targets are the harnesses that can be launched with a prompt, minus the source itself.
/// A missing CLI is not filtered here: the launch reports it, and an unlaunchable target
/// must never look like a delivered handoff.
pub fn targets(source: &str) -> Vec<HarnessKind> {
    harness::launchable()
        .iter()
        .copied()
        .filter(|kind| kind.to_string() != source)
        .collect()
}

/// Bounded newest-first fill: the most recent messages are the ones worth carrying, and the
/// oldest included message is cut at its head rather than dropped whole.
pub fn export(
    source: &str,
    session_id: &str,
    cwd: &Path,
    conversation: &Transcript,
    budget: usize,
) -> Export {
    let mut blocks: Vec<String> = Vec::new();
    let mut left = budget;
    let mut truncated = false;
    let mut included = 0;
    for message in conversation.messages.iter().rev() {
        let who = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Output => "output",
        };
        let text = message.text.trim();
        if text.is_empty() {
            continue;
        }
        const CUT: &str = "…";
        let head = format!("## {who}\n");
        // The header, a trailing newline and, if it comes to that, the cut mark.
        let Some(room) = left.checked_sub(head.len() + 1 + CUT.len()) else {
            break;
        };
        let body = if text.len() <= room + CUT.len() {
            text.to_owned()
        } else {
            truncated = true;
            let at = (text.len() - room..text.len())
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(text.len());
            format!("{CUT}{}", &text[at..])
        };
        left -= head.len() + body.len() + 1;
        blocks.push(format!("{head}{body}"));
        included += 1;
        if truncated {
            break;
        }
    }
    let omitted = conversation
        .messages
        .iter()
        .filter(|m| !m.text.trim().is_empty())
        .count()
        - included;
    let mut text = format!(
        "You are a fresh session, handed a conversation from another agent.\n\n\
         Source: {source} session {session_id} in {}.\n\
         This is a bounded text export, not a continuation: the source session is still \
         running under its own identity, account and permissions, and none of that carried \
         over to you. Tool calls, tool results and file contents are not included.\n",
        cwd.display(),
    );
    text.push_str(&format!(
        "Included: {included} message{}. Omitted: {omitted}{}.{}\n\n\
         Read the folder yourself before acting on anything below.\n\n\
         --- exported conversation, oldest first ---\n\n",
        if included == 1 { "" } else { "s" },
        if conversation.earlier {
            " (earlier conversation beyond the read window is also missing)"
        } else {
            ""
        },
        if truncated {
            " The oldest included message is cut at its beginning."
        } else {
            ""
        },
    ));
    for block in blocks.iter().rev() {
        text.push_str(block);
        text.push('\n');
    }
    Export {
        text,
        included,
        omitted,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript;

    fn conversation(lines: &[(&str, &str)]) -> Transcript {
        let bytes: String = lines
            .iter()
            .map(|(role, text)| {
                serde_json::json!({
                    "type": role,
                    "message": {"role": role, "content": [{"type": "text", "text": text}]},
                })
                .to_string()
                    + "\n"
            })
            .collect();
        let parsed = transcript::parse("claude", bytes.as_bytes());
        assert_eq!(parsed.messages.len(), lines.len(), "fixture must parse");
        parsed
    }

    #[test]
    fn the_export_names_its_source_and_says_it_is_not_a_continuation() {
        let export = export(
            "claude",
            "abc-123",
            Path::new("/project"),
            &conversation(&[("user", "fix the parser"), ("assistant", "done")]),
            BUDGET,
        );
        assert!(export.text.contains("claude session abc-123 in /project"));
        assert!(export.text.contains("not a continuation"));
        assert!(export.text.contains("## user\nfix the parser"));
        assert!(export.text.contains("## assistant\ndone"));
        assert_eq!(
            (export.included, export.omitted, export.truncated),
            (2, 0, false)
        );
    }

    #[test]
    fn the_newest_messages_survive_the_budget_and_the_loss_is_visible() {
        let old = "old ".repeat(200);
        let export = export(
            "claude",
            "s",
            Path::new("/project"),
            &conversation(&[("user", &old), ("user", "middle"), ("assistant", "newest")]),
            300,
        );
        assert!(export.text.contains("newest"));
        assert!(export.text.contains("middle"));
        assert_eq!(export.included, 3);
        assert!(export.truncated);
        assert!(export.text.contains("cut at its beginning"));
        assert!(export.text.contains('…'));
        // The oldest message kept its tail, not its head.
        assert!(!export.text.contains(&format!("## user\n{old}")));
    }

    #[test]
    fn messages_beyond_the_budget_are_counted_as_omitted() {
        let wall = "x".repeat(400);
        let export = export(
            "claude",
            "s",
            Path::new("/project"),
            &conversation(&[("user", "forgotten"), ("assistant", &wall)]),
            200,
        );
        assert_eq!((export.included, export.omitted), (1, 1));
        assert!(!export.text.contains("forgotten"));
        assert!(export.text.contains("Omitted: 1"));
    }

    #[test]
    fn an_earlier_window_is_reported_even_when_nothing_was_dropped_here() {
        let mut conversation = conversation(&[("user", "hi")]);
        conversation.earlier = true;
        let export = export("claude", "s", Path::new("/p"), &conversation, BUDGET);
        assert!(export.text.contains("beyond the read window"));
    }

    #[test]
    fn a_handoff_never_targets_its_own_harness_and_needs_a_readable_transcript() {
        let targets = targets("claude");
        assert!(!targets.is_empty());
        assert!(!targets.iter().any(|k| k.to_string() == "claude"));
        assert!(source_supported("claude"));
        assert!(!source_supported("opencode"));
        assert!(!source_supported("terminal"));
    }
}
