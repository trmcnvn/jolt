//! Message parts: event folding, render-only privacy policy, and continuation
//! splitting.

use serde::{Deserialize, Serialize};

use jolt_proto::{AgentEvent, HarnessId, ToolCall, TurnDiffManifest, UserInputQuestion};

use crate::constants::MSG_INLINE_MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageStatus {
    Streaming,
    Complete,
    Aborted,
}

/// One rendered part of an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MessagePart {
    Text {
        id: String,
        text: String,
    },
    /// Presentation boundary for buffered assistant prose. Text before this
    /// marker is safe to render as one stable chunk; text after
    /// the final marker remains hidden while the entry is streaming.
    TextReveal {
        id: String,
    },
    #[serde(rename_all = "camelCase")]
    Tool {
        id: String,
        call: ToolCall,
        #[serde(default)]
        is_error: bool,
        /// True once a ToolResult arrived.
        #[serde(default)]
        resolved: bool,
    },
    #[serde(rename_all = "camelCase")]
    Input {
        id: String,
        request_id: String,
        questions: Vec<UserInputQuestion>,
        #[serde(default)]
        resolved: bool,
    },
    Error {
        id: String,
        message: String,
    },
    /// Durable transcript boundary recording a coding-harness transition.
    HarnessSwitch {
        id: String,
        from: HarnessId,
        to: HarnessId,
    },
    /// Immutable net filesystem changes attributed to this assistant entry.
    Changes {
        id: String,
        diff: TurnDiffManifest,
    },
}

impl MessagePart {
    pub fn id(&self) -> &str {
        match self {
            MessagePart::Text { id, .. }
            | MessagePart::TextReveal { id }
            | MessagePart::Tool { id, .. }
            | MessagePart::Input { id, .. }
            | MessagePart::Error { id, .. }
            | MessagePart::HarnessSwitch { id, .. }
            | MessagePart::Changes { id, .. } => id,
        }
    }

    pub fn byte_len(&self) -> usize {
        match self {
            MessagePart::Text { text, .. } => text.len(),
            MessagePart::TextReveal { .. } => 0,
            MessagePart::Tool { call, .. } => serde_json::to_vec(call).map_or(0, |v| v.len()),
            MessagePart::Input { questions, .. } => {
                serde_json::to_vec(questions).map_or(0, |v| v.len())
            }
            MessagePart::Error { message, .. } => message.len(),
            MessagePart::HarnessSwitch { .. } => 32,
            MessagePart::Changes { diff, .. } => {
                serde_json::to_vec(diff).map_or(0, |value| value.len())
            }
        }
    }
}

/// Fold one agent event into a parts accumulator, in place.
///
/// In place because the fold runs once per streamed event: rebuilding the
/// accumulator each time made long turns O(n²) in allocations.
///
/// Fold semantics:
/// - `SessionStarted` / `Steered` reset the accumulator (turn boundary — makes replay safe).
/// - `TextDelta` appends to the trailing buffered text part, or starts a new one if the trail is
///   not text. Chunks are safety-revealed once they reach [`MAX_BUFFERED_ASSISTANT_BYTES`].
/// - `AssistantMessageCompleted` appends a `TextReveal` boundary, making the preceding semantic
///   text chunk renderable while later deltas start a fresh hidden text part.
/// - A new `ToolCall` first reveals pending prose, then appends the tool. Existing calls refresh
///   in place for SDK retry idempotence.
/// - `ToolResult` marks the matching tool part resolved / errored in place.
/// - `InputRequested` appends an input part; `InputResolved` marks it resolved.
/// - `Error` and `Done{error}` become visible error parts.
const MAX_BUFFERED_ASSISTANT_BYTES: usize = 24_000;

fn reveal_buffered_text(out: &mut Vec<MessagePart>) {
    let after = out
        .iter()
        .rposition(|part| matches!(part, MessagePart::TextReveal { .. }))
        .map_or(0, |index| index + 1);
    if out[after..]
        .iter()
        .any(|part| matches!(part, MessagePart::Text { text, .. } if !text.is_empty()))
    {
        out.push(MessagePart::TextReveal {
            id: format!("r{}", out.len()),
        });
    }
}

pub fn fold_event_into_parts(out: &mut Vec<MessagePart>, event: &AgentEvent) {
    match event {
        AgentEvent::SessionStarted { .. } | AgentEvent::Steered { .. } => {
            out.clear();
        }
        AgentEvent::TextDelta { text } => {
            let buffered_bytes = if let Some(MessagePart::Text { text: tail, .. }) = out.last_mut()
            {
                tail.push_str(text);
                tail.len()
            } else {
                let id = format!("t{}", out.len());
                out.push(MessagePart::Text {
                    id,
                    text: text.clone(),
                });
                text.len()
            };
            if buffered_bytes >= MAX_BUFFERED_ASSISTANT_BYTES {
                reveal_buffered_text(out);
            }
        }
        AgentEvent::ReasoningDelta { .. } => {
            // Reasoning is not rendered as a transcript part.
        }
        AgentEvent::ToolCall { id, call } => {
            if let Some(existing) = out.iter_mut().find_map(|p| match p {
                MessagePart::Tool {
                    id: pid, call: c, ..
                } if pid == id => Some(c),
                _ => None,
            }) {
                *existing = call.clone();
            } else {
                reveal_buffered_text(out);
                out.push(MessagePart::Tool {
                    id: id.clone(),
                    call: call.clone(),
                    is_error: false,
                    resolved: false,
                });
            }
        }
        AgentEvent::ToolResult { id, is_error } => {
            for p in out.iter_mut() {
                if let MessagePart::Tool {
                    id: pid,
                    is_error: e,
                    resolved,
                    ..
                } = p
                    && pid == id
                {
                    *e = *is_error;
                    *resolved = true;
                }
            }
        }
        AgentEvent::InputRequested {
            request_id,
            questions,
        } => {
            reveal_buffered_text(out);
            let id = format!("in-{request_id}");
            if !out.iter().any(|p| p.id() == id) {
                out.push(MessagePart::Input {
                    id,
                    request_id: request_id.clone(),
                    questions: questions.clone(),
                    resolved: false,
                });
            }
        }
        AgentEvent::InputResolved { request_id } => {
            for p in out.iter_mut() {
                if let MessagePart::Input {
                    request_id: rid,
                    resolved,
                    ..
                } = p
                    && rid == request_id
                {
                    *resolved = true;
                }
            }
        }
        AgentEvent::Error { message } => {
            let id = format!("e{}", out.len());
            out.push(MessagePart::Error {
                id,
                message: message.clone(),
            });
        }
        AgentEvent::Done { error, .. } => {
            reveal_buffered_text(out);
            if let Some(message) = error {
                let id = format!("e{}", out.len());
                out.push(MessagePart::Error {
                    id,
                    message: message.clone(),
                });
            }
        }
        AgentEvent::AssistantMessageCompleted { .. } => reveal_buffered_text(out),
        AgentEvent::Usage { .. }
        | AgentEvent::CompactionStarted
        | AgentEvent::CompactionFinished => {}
    }
}

/// Render-only privacy policy — strip heavy/sensitive tool inputs before a call enters the doc.
///
/// Keeps: command / path + read range / pattern / url / query / todo items / server+tool names.
/// Drops: WriteFile content, EditFile old/new strings, WebFetch prompt, Mcp/Unknown input.
/// Full inputs remain only in the host's local run journal. Idempotent.
pub fn sanitize_tool_call(call: &ToolCall) -> ToolCall {
    match call {
        ToolCall::WriteFile { path, .. } => ToolCall::WriteFile {
            path: path.clone(),
            content: None,
        },
        ToolCall::EditFile { path, .. } => ToolCall::EditFile {
            path: path.clone(),
            old_string: None,
            new_string: None,
        },
        ToolCall::WebFetch { url, .. } => ToolCall::WebFetch {
            url: url.clone(),
            prompt: None,
        },
        ToolCall::Mcp { server, tool, .. } => ToolCall::Mcp {
            server: server.clone(),
            tool: tool.clone(),
            input: None,
        },
        ToolCall::Unknown { name, .. } => ToolCall::Unknown {
            name: name.clone(),
            input: None,
        },
        other => other.clone(),
    }
}

/// Deterministic continuation id: `"{root}#c{n}"`.
pub fn continuation_id(root: &str, index: usize) -> String {
    format!("{root}#c{index}")
}

/// Split an oversized parts list into chunks each under `MSG_INLINE_MAX` bytes.
///
/// Splitting happens at part boundaries; an oversized text part is itself chunked at char
/// boundaries. Returns one Vec per resulting entry — the first keeps the root id, the rest are
/// continuations (`continuation_id(root, i)`), matching `splitMessageEntry` in jolt.
pub fn split_parts(parts: &[MessagePart]) -> Vec<Vec<MessagePart>> {
    let mut chunks: Vec<Vec<MessagePart>> = vec![Vec::new()];
    let mut current_bytes = 0usize;

    let push_part = |chunks: &mut Vec<Vec<MessagePart>>, current: &mut usize, part: MessagePart| {
        let len = part.byte_len();
        if *current > 0 && *current + len > MSG_INLINE_MAX {
            chunks.push(Vec::new());
            *current = 0;
        }
        *current += len;
        chunks.last_mut().unwrap().push(part);
    };

    for part in parts {
        match part {
            MessagePart::Text { id, text } if text.len() > MSG_INLINE_MAX => {
                // Chunk oversized text at char boundaries.
                let mut start = 0usize;
                let mut piece = 0usize;
                while start < text.len() {
                    let mut end = (start + MSG_INLINE_MAX).min(text.len());
                    while end < text.len() && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    // Guard: ensure forward progress on pathological boundaries.
                    if end <= start {
                        end = text.len();
                    }
                    let sub = MessagePart::Text {
                        id: if piece == 0 {
                            id.clone()
                        } else {
                            format!("{id}~{piece}")
                        },
                        text: text[start..end].to_string(),
                    };
                    push_part(&mut chunks, &mut current_bytes, sub);
                    start = end;
                    piece += 1;
                }
            }
            other => push_part(&mut chunks, &mut current_bytes, other.clone()),
        }
    }
    chunks
}

/// Render-time inverse of splitting: concatenate continuation entries' parts in list order.
pub fn join_continuations(entries: Vec<Vec<MessagePart>>) -> Vec<MessagePart> {
    entries.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_delta(s: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: s.into() }
    }

    #[test]
    fn tool_call_reveals_preceding_text_and_starts_the_next_chunk() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &text_delta("Hello "));
        fold_event_into_parts(&mut parts, &text_delta("world"));
        assert_eq!(parts.len(), 1);
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        fold_event_into_parts(&mut parts, &text_delta("after"));
        assert!(matches!(
            parts.as_slice(),
            [
                MessagePart::Text { text, .. },
                MessagePart::TextReveal { .. },
                MessagePart::Tool { .. },
                MessagePart::Text { text: after, .. }
            ] if text == "Hello world" && after == "after"
        ));
    }

    #[test]
    fn pi_style_tools_reveal_each_interleaved_prose_chunk() {
        let mut parts = Vec::new();
        for (text, tool_id) in [("first", "tool-1"), ("second", "tool-2")] {
            fold_event_into_parts(&mut parts, &text_delta(text));
            fold_event_into_parts(
                &mut parts,
                &AgentEvent::ToolCall {
                    id: tool_id.into(),
                    call: ToolCall::Exec {
                        command: "true".into(),
                    },
                },
            );
            fold_event_into_parts(
                &mut parts,
                &AgentEvent::ToolResult {
                    id: tool_id.into(),
                    is_error: false,
                },
            );
        }

        assert!(matches!(
            parts.as_slice(),
            [
                MessagePart::Text { text: first, .. },
                MessagePart::TextReveal { .. },
                MessagePart::Tool { .. },
                MessagePart::Text { text: second, .. },
                MessagePart::TextReveal { .. },
                MessagePart::Tool { .. }
            ] if first == "first" && second == "second"
        ));
    }

    #[test]
    fn assistant_completion_reveals_one_chunk_and_starts_the_next() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &text_delta("first"));
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::AssistantMessageCompleted {
                assistant_message_id: "a1".into(),
            },
        );
        fold_event_into_parts(&mut parts, &text_delta("second"));

        assert!(matches!(
            parts.as_slice(),
            [
                MessagePart::Text { text, .. },
                MessagePart::TextReveal { .. },
                MessagePart::Text { text: second, .. }
            ] if text == "first" && second == "second"
        ));
    }

    #[test]
    fn oversized_buffer_reveals_without_waiting_for_provider_boundary() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &text_delta(&"x".repeat(MAX_BUFFERED_ASSISTANT_BYTES)),
        );
        assert!(matches!(parts.last(), Some(MessagePart::TextReveal { .. })));
    }

    #[test]
    fn completion_without_buffered_text_does_not_add_a_marker() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::AssistantMessageCompleted {
                assistant_message_id: "a1".into(),
            },
        );
        assert!(parts.is_empty());
    }

    #[test]
    fn session_started_resets_accumulator() {
        let mut parts = Vec::new();
        fold_event_into_parts(&mut parts, &text_delta("junk"));
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::SessionStarted {
                harness: jolt_proto::HarnessId::Mock,
                model: "m".into(),
                tools: vec![],
                cwd: "/".into(),
                session_id: "s".into(),
                assistant_message_id: "a".into(),
            },
        );
        assert!(parts.is_empty());
    }

    #[test]
    fn tool_call_refresh_is_idempotent() {
        let call = AgentEvent::ToolCall {
            id: "t".into(),
            call: ToolCall::Exec {
                command: "ls".into(),
            },
        };
        let mut once = Vec::new();
        fold_event_into_parts(&mut once, &call);
        let mut twice = once.clone();
        fold_event_into_parts(&mut twice, &call);
        assert_eq!(once, twice);
    }

    #[test]
    fn tool_result_marks_resolution() {
        let mut parts = Vec::new();
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolCall {
                id: "t".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
            },
        );
        fold_event_into_parts(
            &mut parts,
            &AgentEvent::ToolResult {
                id: "t".into(),
                is_error: true,
            },
        );
        match &parts[0] {
            MessagePart::Tool {
                is_error, resolved, ..
            } => {
                assert!(*is_error);
                assert!(*resolved);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sanitize_strips_heavy_inputs_and_is_idempotent() {
        let call = ToolCall::WriteFile {
            path: "/x".into(),
            content: Some("secret".into()),
        };
        let clean = sanitize_tool_call(&call);
        assert_eq!(
            clean,
            ToolCall::WriteFile {
                path: "/x".into(),
                content: None
            }
        );
        assert_eq!(sanitize_tool_call(&clean), clean);
    }

    #[test]
    fn split_and_join_round_trip() {
        let big = "x".repeat(MSG_INLINE_MAX * 2 + 100);
        let parts = vec![
            MessagePart::Text {
                id: "t0".into(),
                text: big.clone(),
            },
            MessagePart::Tool {
                id: "tool-1".into(),
                call: ToolCall::Exec {
                    command: "ls".into(),
                },
                is_error: false,
                resolved: true,
            },
        ];
        let chunks = split_parts(&parts);
        assert!(
            chunks.len() >= 3,
            "expected >=3 chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            let bytes: usize = chunk.iter().map(|p| p.byte_len()).sum();
            assert!(bytes <= MSG_INLINE_MAX, "chunk over cap: {bytes}");
        }
        let joined = join_continuations(chunks);
        let text: String = joined
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, big);
        assert!(matches!(joined.last().unwrap(), MessagePart::Tool { .. }));
    }

    #[test]
    fn continuation_ids_are_deterministic() {
        assert_eq!(continuation_id("m1", 1), "m1#c1");
    }
}
