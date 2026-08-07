//! One-shot extraction of user-facing questions from completed assistant prose.

use std::sync::Arc;
use std::time::Duration;

use jolt_proto::{ExtractedQuestion, HarnessId, ReasoningLevel, RunRequest, SandboxLevel};
use serde::Deserialize;

use crate::EngineError;
use crate::model_selection::cheap_model_id;
use crate::registry::HarnessRegistry;
use crate::titles::collect_text;
use crate::usage::{UsageCapture, UsagePurpose, UsageStore};

const EXTRACTION_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_QUESTIONS: usize = 24;

const EXTRACTION_PROMPT: &str = r#"You are a question extractor. Given text from a conversation, extract any questions that need answering.

Output a JSON object with this structure:
{
  "questions": [
    {
      "question": "The question text",
      "context": "Optional context that helps answer the question"
    }
  ]
}

Rules:
- Extract all questions that require user input
- Keep questions in the order they appeared
- Be concise with question text
- Include context only when it provides essential information for answering
- If no questions are found, return {"questions": []}
- Output only the JSON object

Text to inspect follows between the delimiters.

<assistant-response>
"#;

#[derive(Debug, Deserialize)]
struct ExtractionEnvelope {
    questions: Vec<ExtractedQuestion>,
}

/// Run a read-only throwaway harness turn and parse its structured result.
pub(crate) async fn extract_questions(
    registry: &Arc<HarnessRegistry>,
    usage_store: UsageStore,
    chat_id: &str,
    harness_id: HarnessId,
    configured_model: Option<&str>,
    cwd: &str,
    assistant_text: &str,
) -> Result<Vec<ExtractedQuestion>, EngineError> {
    let harness = registry.resolve(harness_id)?;
    let models = harness.models().await.unwrap_or_default();
    let model = cheap_model_id(&models, configured_model);
    let mut model_options = serde_json::Map::new();
    if harness_id == HarnessId::Pi {
        model_options.insert("projectTrust".into(), serde_json::json!("ignore"));
        model_options.insert("toolAccess".into(), serde_json::json!("readOnly"));
    }
    let prompt = format!("{EXTRACTION_PROMPT}{assistant_text}\n</assistant-response>");
    let request = RunRequest {
        prompt,
        model: model.clone(),
        reasoning: Some(ReasoningLevel::Minimal),
        model_options,
        cwd: cwd.to_string(),
        sandbox: SandboxLevel::ReadOnly,
        auto_approve: true,
        resume: None,
        attachments: Vec::new(),
    };
    let mut usage = UsageCapture::new(
        usage_store,
        chat_id,
        UsagePurpose::QuestionExtraction,
        harness_id,
        model.as_deref(),
        cwd,
    );
    let raw = tokio::time::timeout(
        EXTRACTION_TIMEOUT,
        collect_text(harness.as_ref(), request, |event| {
            if let Err(error) = usage.observe(event) {
                tracing::error!(chat = %chat_id, %error, "question extraction usage ledger write failed");
            }
        }),
    )
    .await
    .map_err(|_| EngineError::Other("question extraction timed out".into()))??;
    parse_extraction(&raw)
        .ok_or_else(|| EngineError::Other("question extraction returned invalid JSON".into()))
}

fn parse_extraction(raw: &str) -> Option<Vec<ExtractedQuestion>> {
    let trimmed = raw.trim();
    let mut candidates = Vec::new();
    if let Some(fenced) = fenced_json(trimmed) {
        candidates.push(fenced);
    }
    candidates.push(trimmed);
    if let (Some(first), Some(last)) = (trimmed.find('{'), trimmed.rfind('}'))
        && first < last
    {
        candidates.push(&trimmed[first..=last]);
    }

    candidates.into_iter().find_map(|candidate| {
        let parsed: ExtractionEnvelope = serde_json::from_str(candidate).ok()?;
        let questions: Vec<ExtractedQuestion> = parsed
            .questions
            .into_iter()
            .take(MAX_QUESTIONS)
            .filter_map(|question| {
                let text = question.question.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                let context = question
                    .context
                    .map(|context| context.trim().to_string())
                    .filter(|context| !context.is_empty());
                Some(ExtractedQuestion {
                    question: text,
                    context,
                })
            })
            .collect();
        Some(questions)
    })
}

fn fenced_json(text: &str) -> Option<&str> {
    let open = text.find("```")?;
    let after_open = &text[open + 3..];
    let content = after_open
        .strip_prefix("json")
        .or_else(|| after_open.strip_prefix("JSON"))
        .unwrap_or(after_open)
        .trim_start_matches(['\r', '\n']);
    let close = content.find("```")?;
    Some(content[..close].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn extracts_questions_through_a_throwaway_harness_run() {
        use jolt_harness::mock::MockHarness;
        use jolt_proto::{AgentEvent, DoneStatus};

        let registry = Arc::new(HarnessRegistry::new());
        registry.register(Arc::new(MockHarness {
            script: vec![
                AgentEvent::TextDelta {
                    text:
                        r#"{"questions":[{"question":"Pick one?","context":"Choose carefully."}]}"#
                            .into(),
                },
                AgentEvent::Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    cache_read_input_tokens: 4,
                    cache_write_input_tokens: 0,
                    cost_usd: Some(0.02),
                    context_tokens: Some(16),
                    context_window: Some(200_000),
                },
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                },
            ],
        }));
        let dir = tempfile::tempdir().unwrap();
        let usage = UsageStore::open(&dir.path().join("usage.sqlite"), "d1".into()).unwrap();
        let questions = extract_questions(
            &registry,
            usage.clone(),
            "chat-1",
            HarnessId::Mock,
            Some("mock-1"),
            ".",
            "Would you like A or B?",
        )
        .await
        .expect("question extraction");
        assert_eq!(
            questions,
            vec![ExtractedQuestion {
                question: "Pick one?".into(),
                context: Some("Choose carefully.".into()),
            }]
        );
        let summary = usage.summary("chat-1").unwrap();
        assert_eq!(summary.calls, 1);
        assert_eq!(summary.input_tokens, 12);
        assert_eq!(
            summary.model, None,
            "internal use is not the active chat model"
        );
        let breakdown = usage.breakdown(30).unwrap();
        assert_eq!(breakdown.calls, 1);
        assert_eq!(breakdown.rows[0].model, "mock-1");
    }

    #[test]
    fn parses_raw_fenced_and_surrounding_json() {
        let json = r#"{"questions":[{"question":" Pick one? ","context":" Useful context "}]}"#;
        let expected = vec![ExtractedQuestion {
            question: "Pick one?".into(),
            context: Some("Useful context".into()),
        }];
        assert_eq!(parse_extraction(json), Some(expected.clone()));
        assert_eq!(
            parse_extraction(&format!("```json\n{json}\n```")),
            Some(expected.clone())
        );
        assert_eq!(
            parse_extraction(&format!("Result:\n{json}\nDone")),
            Some(expected)
        );
    }

    #[test]
    fn rejects_bad_shape_and_discards_blank_questions() {
        assert_eq!(parse_extraction("not json"), None);
        assert_eq!(parse_extraction(r#"{"questions":"nope"}"#), None);
        assert_eq!(
            parse_extraction(r#"{"questions":[{"question":"  "}]}"#),
            Some(Vec::new())
        );
    }
}
