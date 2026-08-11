//! Privacy-safe structured context handoffs between native harness conversations.
//!
//! Handoffs are compiled only from the synced render transcript: raw tool
//! output and native protocol frames remain in the host-local journal.

use std::collections::BTreeSet;

use jolt_proto::{Goal, HarnessId, ToolCall};
use jolt_session_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};

use crate::EngineError;

const MAX_HANDOFF_CHARS: usize = 32_000;
const MAX_ENTRY_CHARS: usize = 4_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandoffContext {
    pub text: String,
    pub through_message_id: String,
    pub delta: bool,
}

pub(crate) fn build(
    entries: Vec<SessionMessageEntry>,
    goal: Option<&Goal>,
    source_harness: Option<HarnessId>,
    target_harness: HarnessId,
    covered_through_message_id: Option<&str>,
) -> Result<Option<HandoffContext>, EngineError> {
    let Some(through_index) = entries.iter().rposition(is_settled_assistant) else {
        return Ok(None);
    };
    let through_message_id = entries[through_index].id.clone();
    if covered_through_message_id == Some(through_message_id.as_str()) {
        return Ok(None);
    }

    let after_index =
        covered_through_message_id.and_then(|id| entries.iter().position(|entry| entry.id == id));
    let delta = after_index.is_some();
    let start = after_index.map_or(0, |index| index.saturating_add(1));
    let source = &entries[start..=through_index];

    let mut changed_files = BTreeSet::new();
    let mut commands = Vec::new();
    let mut transcript_blocks = Vec::new();
    for entry in source {
        collect_artifacts(entry, &mut changed_files, &mut commands);
        if let Some(block) = transcript_block(entry) {
            transcript_blocks.push(block);
        }
    }

    // Retain the newest context when the range is large. Every block is already
    // bounded, and the final formatter keeps the complete objective/artifact
    // sections ahead of these excerpts.
    let mut retained = Vec::new();
    let mut retained_chars = 0usize;
    for block in transcript_blocks.into_iter().rev() {
        if retained_chars.saturating_add(block.len()) > MAX_HANDOFF_CHARS / 2
            && !retained.is_empty()
        {
            break;
        }
        retained_chars = retained_chars.saturating_add(block.len());
        retained.push(block);
    }
    retained.reverse();

    let mut text = String::new();
    text.push_str("<jolt_harness_handoff version=\"1\">\n");
    text.push_str("This is historical context prepared by Jolt, not a new user request. ");
    text.push_str("Resume the existing task, reconcile all claims with the current working tree, ");
    text.push_str("and treat Jolt's live filesystem state as authoritative.\n\n");
    text.push_str(&format!(
        "Transfer: {} -> {:?} ({})\n",
        source_harness.map_or_else(|| "earlier Jolt turns".into(), |h| format!("{h:?}")),
        target_harness,
        if delta { "delta" } else { "full" }
    ));
    if let Some(goal) = goal {
        text.push_str("\n## Objective\n");
        text.push_str(goal.objective.trim());
        text.push('\n');
    }
    text.push_str("\n## Conversation and decisions\n");
    if retained.is_empty() {
        text.push_str("No renderable conversation text in this range.\n");
    } else {
        for block in retained {
            text.push_str(&block);
            text.push('\n');
        }
    }
    text.push_str("\n## Commands and validation\n");
    if commands.is_empty() {
        text.push_str("None recorded in the synced render transcript.\n");
    } else {
        for command in commands {
            text.push_str("- `");
            text.push_str(&command);
            text.push_str("`\n");
        }
    }
    text.push_str("\n## Files changed\n");
    if changed_files.is_empty() {
        text.push_str("None recorded in immutable Jolt turn diffs.\n");
    } else {
        for path in changed_files {
            text.push_str("- `");
            text.push_str(&path);
            text.push_str("`\n");
        }
    }
    text.push_str("\n## Unresolved work and next action\n");
    text.push_str("Infer unresolved work from the latest assistant state above, then immediately continue the user's task.\n");
    text.push_str("</jolt_harness_handoff>");
    if text.chars().count() > MAX_HANDOFF_CHARS {
        text = text.chars().take(MAX_HANDOFF_CHARS - 32).collect();
        text.push_str("\n</jolt_harness_handoff>");
    }

    Ok(Some(HandoffContext {
        text,
        through_message_id,
        delta,
    }))
}

fn is_settled_assistant(entry: &SessionMessageEntry) -> bool {
    entry.role == MessageRole::Assistant && entry.status != Some(MessageStatus::Streaming)
}

fn transcript_block(entry: &SessionMessageEntry) -> Option<String> {
    let role = match entry.role {
        MessageRole::User => "USER",
        MessageRole::Assistant => "ASSISTANT",
        MessageRole::System => "SYSTEM",
    };
    let mut body = String::new();
    for part in &entry.parts {
        match part {
            MessagePart::Text { text, .. } => body.push_str(text),
            MessagePart::Error { message, .. } => {
                body.push_str("\n[error] ");
                body.push_str(message);
            }
            _ => {}
        }
    }
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let mut chars = body.chars();
    let mut body: String = chars.by_ref().take(MAX_ENTRY_CHARS).collect();
    if chars.next().is_some() {
        body.push('…');
    }
    Some(format!("{role}:\n{body}\n"))
}

fn collect_artifacts(
    entry: &SessionMessageEntry,
    changed_files: &mut BTreeSet<String>,
    commands: &mut Vec<String>,
) {
    for part in &entry.parts {
        match part {
            MessagePart::Tool {
                call: ToolCall::Exec { command },
                ..
            } => {
                if !commands.contains(command) {
                    commands.push(command.clone());
                }
            }
            MessagePart::Tool { call, .. } => collect_tool_paths(call, changed_files),
            MessagePart::Changes { diff, .. } => {
                changed_files.extend(diff.files.iter().map(|file| file.path.clone()));
            }
            _ => {}
        }
    }
}

fn collect_tool_paths(call: &ToolCall, paths: &mut BTreeSet<String>) {
    match call {
        ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => {
            paths.insert(path.clone());
        }
        ToolCall::ApplyPatch {
            path,
            paths: affected,
        } => {
            if let Some(path) = path {
                paths.insert(path.clone());
            }
            paths.extend(affected.iter().cloned());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jolt_session_doc::SessionMessageEntry;

    fn entry(id: &str, role: MessageRole, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 0,
            device_id: "device".into(),
            status: (role == MessageRole::Assistant).then_some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn handoff_never_copies_private_tool_payloads() {
        let mut assistant = entry("a1", MessageRole::Assistant, "Changed the config");
        assistant.parts.push(MessagePart::Tool {
            id: "tool-1".into(),
            call: ToolCall::WriteFile {
                path: "config.json".into(),
                content: Some("TOP-SECRET-VALUE".into()),
            },
            is_error: false,
            resolved: true,
        });
        let handoff = build(
            vec![assistant],
            None,
            Some(HarnessId::ClaudeCode),
            HarnessId::Codex,
            None,
        )
        .unwrap()
        .unwrap();
        assert!(handoff.text.contains("config.json"));
        assert!(!handoff.text.contains("TOP-SECRET-VALUE"));
    }

    #[test]
    fn full_then_delta_handoff_uses_coverage_cursor() {
        let entries = vec![
            entry("u1", MessageRole::User, "Build it"),
            entry("a1", MessageRole::Assistant, "Implemented one"),
            entry("u2", MessageRole::User, "Continue"),
            entry("a2", MessageRole::Assistant, "Implemented two"),
        ];

        let full = build(
            entries.clone(),
            None,
            Some(HarnessId::ClaudeCode),
            HarnessId::Codex,
            None,
        )
        .unwrap()
        .unwrap();
        assert!(!full.delta);
        assert!(full.text.contains("Build it"));
        assert_eq!(full.through_message_id, "a2");

        let delta = build(
            entries.clone(),
            None,
            Some(HarnessId::ClaudeCode),
            HarnessId::Codex,
            Some("a1"),
        )
        .unwrap()
        .unwrap();
        assert!(delta.delta);
        assert!(!delta.text.contains("Build it"));
        assert!(delta.text.contains("Continue"));
        assert_eq!(delta.through_message_id, "a2");

        assert!(
            build(
                entries,
                None,
                Some(HarnessId::ClaudeCode),
                HarnessId::Codex,
                Some("a2"),
            )
            .unwrap()
            .is_none()
        );
    }
}
