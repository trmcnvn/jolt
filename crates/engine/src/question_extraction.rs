//! One-shot extraction of user-facing questions from completed assistant prose.

use std::sync::Arc;
use std::time::Duration;

use jolt_proto::{ExtractedQuestion, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel};
use serde::Deserialize;

use crate::EngineError;
use crate::registry::HarnessRegistry;
use crate::titles::collect_text;

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
    harness_id: HarnessId,
    configured_model: Option<&str>,
    cwd: &str,
    assistant_text: &str,
) -> Result<Vec<ExtractedQuestion>, EngineError> {
    let harness = registry.resolve(harness_id)?;
    let models = harness.models().await.unwrap_or_default();
    let model = extraction_model(&models, configured_model);
    let mut model_options = serde_json::Map::new();
    if harness_id == HarnessId::Pi {
        model_options.insert("projectTrust".into(), serde_json::json!("ignore"));
        model_options.insert("toolAccess".into(), serde_json::json!("readOnly"));
    }
    let prompt = format!("{EXTRACTION_PROMPT}{assistant_text}\n</assistant-response>");
    let request = RunRequest {
        prompt,
        model,
        reasoning: Some(ReasoningLevel::Minimal),
        model_options,
        cwd: cwd.to_string(),
        sandbox: SandboxLevel::ReadOnly,
        auto_approve: true,
        resume: None,
        attachments: Vec::new(),
    };
    let raw = tokio::time::timeout(EXTRACTION_TIMEOUT, collect_text(harness.as_ref(), request))
        .await
        .map_err(|_| EngineError::Other("question extraction timed out".into()))??;
    parse_extraction(&raw)
        .ok_or_else(|| EngineError::Other("question extraction returned invalid JSON".into()))
}

fn extraction_model(models: &[Model], configured: Option<&str>) -> Option<String> {
    let small = models.iter().find(|model| {
        let name = format!("{} {}", model.id, model.label).to_lowercase();
        ["mini", "haiku", "nano", "flash", "small", "lite"]
            .iter()
            .any(|tier| name.contains(tier))
    });
    small
        .or_else(|| {
            configured.and_then(|configured| models.iter().find(|model| model.id == configured))
        })
        .or_else(|| models.last())
        .map(|model| model.id.clone())
        .or_else(|| configured.map(str::to_owned))
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
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                },
            ],
        }));
        let questions = extract_questions(
            &registry,
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

    #[test]
    fn model_selection_prefers_small_then_configured() {
        let model = |id: &str, label: &str| Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        };
        let models = vec![model("large", "Large"), model("quick", "Mini")];
        assert_eq!(
            extraction_model(&models, Some("large")).as_deref(),
            Some("quick")
        );
        let models = vec![model("large", "Large"), model("other", "Other")];
        assert_eq!(
            extraction_model(&models, Some("large")).as_deref(),
            Some("large")
        );
    }
}
