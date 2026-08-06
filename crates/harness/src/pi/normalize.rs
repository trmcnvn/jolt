//! Pi RPC event and tool normalization.

use jolt_proto::{AgentEvent, ToolCall};
use serde_json::Value;

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn tool_start(event: &Value) -> Option<AgentEvent> {
    let id = string(event, "toolCallId");
    let name = string(event, "toolName");
    if id.is_empty() || name.is_empty() {
        return None;
    }
    let args = event.get("args").cloned().unwrap_or(Value::Null);
    let path = || string(&args, "path");
    let call = match name.as_str() {
        "bash" => ToolCall::Exec {
            command: string(&args, "command"),
        },
        "read" => ToolCall::ReadFile { path: path() },
        "write" => ToolCall::WriteFile {
            path: path(),
            content: None,
        },
        "edit" => ToolCall::EditFile {
            path: path(),
            old_string: None,
            new_string: None,
        },
        "grep" => ToolCall::Search {
            pattern: string(&args, "pattern"),
            path: args
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(str::to_owned),
        },
        "find" => ToolCall::Glob {
            pattern: string(&args, "pattern"),
        },
        "webfetch" | "web_fetch" => ToolCall::WebFetch {
            url: string(&args, "url"),
            prompt: args
                .get("prompt")
                .and_then(Value::as_str)
                .filter(|prompt| !prompt.is_empty())
                .map(str::to_owned),
        },
        "websearch" | "web_search" => ToolCall::WebSearch {
            query: string(&args, "query"),
        },
        _ => ToolCall::Unknown {
            name,
            input: (!args.is_null()).then_some(args),
        },
    };
    Some(AgentEvent::ToolCall { id, call })
}

pub(crate) fn tool_end(event: &Value) -> Option<AgentEvent> {
    let id = string(event, "toolCallId");
    (!id.is_empty()).then(|| AgentEvent::ToolResult {
        id,
        is_error: event.get("isError").and_then(Value::as_bool) == Some(true),
    })
}

pub(crate) fn message_update(event: &Value) -> Option<AgentEvent> {
    let update = event.get("assistantMessageEvent")?;
    let text = update
        .get("delta")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())?
        .to_owned();
    match update.get("type").and_then(Value::as_str) {
        Some("text_delta") => Some(AgentEvent::TextDelta { text }),
        Some("thinking_delta") => Some(AgentEvent::ReasoningDelta { text }),
        _ => None,
    }
}

pub(crate) fn message_error(event: &Value) -> Option<String> {
    let update = event.get("assistantMessageEvent")?;
    if update.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    update
        .get("error")
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .or_else(|| update.get("reason").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| Some("Pi assistant failed".into()))
}

pub(crate) fn custom_message(event: &Value) -> Option<AgentEvent> {
    let message = event.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("custom")
        || message.get("display").and_then(Value::as_bool) == Some(false)
    {
        return None;
    }
    let content = message.get("content")?;
    let text = if let Some(text) = content.as_str() {
        text.to_owned()
    } else {
        content
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("")
    };
    (!text.is_empty()).then_some(AgentEvent::TextDelta { text })
}

pub(crate) fn message_end_error(event: &Value) -> Option<String> {
    let message = event.get("message")?;
    if message.get("stopReason").and_then(Value::as_str) != Some("error") {
        return None;
    }
    message
        .get("errorMessage")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| Some("Pi assistant failed".into()))
}

pub(crate) fn usage(event: &Value, context_window: Option<u64>) -> Option<AgentEvent> {
    let usage = event.get("message")?.get("usage")?;
    let number = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or_default();
    let input_tokens = number("input");
    let cache_read_input_tokens = number("cacheRead");
    let cache_write_input_tokens = number("cacheWrite");
    Some(AgentEvent::Usage {
        input_tokens,
        output_tokens: number("output"),
        cache_read_input_tokens,
        cache_write_input_tokens,
        cost_usd: usage
            .get("cost")
            .and_then(|cost| cost.get("total"))
            .and_then(Value::as_f64),
        context_tokens: Some(
            input_tokens
                .saturating_add(cache_read_input_tokens)
                .saturating_add(cache_write_input_tokens),
        ),
        context_window,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn built_in_tools_map_to_typed_calls() {
        assert_eq!(
            tool_start(&json!({
                "toolCallId": "b1", "toolName": "bash", "args": {"command": "cargo test"}
            })),
            Some(AgentEvent::ToolCall {
                id: "b1".into(),
                call: ToolCall::Exec {
                    command: "cargo test".into()
                }
            })
        );
        assert_eq!(
            tool_start(&json!({
                "toolCallId": "g1", "toolName": "grep",
                "args": {"pattern": "PiHarness", "path": "crates"}
            })),
            Some(AgentEvent::ToolCall {
                id: "g1".into(),
                call: ToolCall::Search {
                    pattern: "PiHarness".into(),
                    path: Some("crates".into())
                }
            })
        );
    }

    #[test]
    fn deltas_usage_and_results_map() {
        assert_eq!(
            message_update(&json!({
                "assistantMessageEvent": {"type": "thinking_delta", "delta": "hmm"}
            })),
            Some(AgentEvent::ReasoningDelta { text: "hmm".into() })
        );
        assert_eq!(
            usage(
                &json!({"message": {"usage": {
                    "input": 3, "output": 4, "cacheRead": 5, "cacheWrite": 6,
                    "cost": {"total": 0.25}
                }}}),
                Some(200_000)
            ),
            Some(AgentEvent::Usage {
                input_tokens: 3,
                output_tokens: 4,
                cache_read_input_tokens: 5,
                cache_write_input_tokens: 6,
                cost_usd: Some(0.25),
                context_tokens: Some(14),
                context_window: Some(200_000),
            })
        );
        assert_eq!(
            tool_end(&json!({"toolCallId": "x", "isError": true})),
            Some(AgentEvent::ToolResult {
                id: "x".into(),
                is_error: true
            })
        );
    }
}
