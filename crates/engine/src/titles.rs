//! Chat auto-titling — after the first user+assistant exchange completes on an
//! untitled chat, name it with the harness's cheapest model.
//!
//! Flow (fire-and-forget from the run task; every failure is a silent skip with
//! tracing — a title must never fail or delay a run):
//! 1. skip when the chat already has a title (or has no workspace row);
//! 2. pick the run harness's cheapest model (small-tier name heuristic, else the
//!    last listed model);
//! 3. run an ephemeral, non-streaming-collected titling prompt through the
//!    [`Harness`] trait (read-only sandbox, minimal reasoning, auto-approve),
//!    retrying with a short backoff ladder; fall back to the prompt's first
//!    words when every attempt produces nothing;
//! 4. re-check the title (a user rename during generation wins);
//! 5. when the chat sits in a jolt worktree (`jolt/<name>` branch), rename the
//!    branch from the title and update the chat's branch row;
//! 6. `rename_chat` in the workspace doc.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use futures::StreamExt;

use jolt_harness::{CancellationToken, RunControls, SteerMessage};
use jolt_proto::{
    AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, UserInputAnswer,
    UserInputQuestion,
};

use crate::EngineError;
use crate::model_selection::cheap_model_id;
use crate::registry::HarnessRegistry;
use crate::usage::{UsageCapture, UsagePurpose, UsageStore};
use crate::workspace_host::WorkspaceHost;
use jolt_vcs::Repos;

/// Throwaway title runs are cheap but still cross a process boundary — retry a
/// couple of times with a short backoff before falling back.
const RETRY_DELAYS_MS: &[u64] = &[250, 1_000];

struct Inner {
    workspace: WorkspaceHost,
    registry: Arc<HarnessRegistry>,
    repos: Repos,
    usage: UsageStore,
    auto_generating: Mutex<HashSet<String>>,
}

#[derive(Clone)]
pub struct TitleGenerator {
    inner: Arc<Inner>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GenerationMode {
    Untitled,
    ReplaceCurrent,
}

impl TitleGenerator {
    pub fn new(
        workspace: WorkspaceHost,
        registry: Arc<HarnessRegistry>,
        repos: Repos,
        usage: UsageStore,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                workspace,
                registry,
                repos,
                usage,
                auto_generating: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// Fire-and-forget: title `chat_id` if it's still untitled. Called by the run
    /// task after a completed exchange; runs detached so it never delays anything.
    pub fn maybe_generate(&self, chat_id: &str, harness: HarnessId, prompt: &str, cwd: &str) {
        let mut generating = self
            .inner
            .auto_generating
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !generating.insert(chat_id.to_string()) {
            return;
        }
        drop(generating);

        let this = self.clone();
        let chat_id = chat_id.to_string();
        let prompt = prompt.to_string();
        let cwd = cwd.to_string();
        tokio::spawn(async move {
            let result = this
                .generate(&chat_id, harness, &prompt, &cwd, GenerationMode::Untitled)
                .await;
            this.inner
                .auto_generating
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&chat_id);
            if let Err(err) = result {
                tracing::debug!(chat = %chat_id, error = %err, "chat auto-titling skipped");
            }
        });
    }

    /// Replace the current title using the same economy-model path as automatic
    /// titling. A concurrent rename still wins.
    pub async fn regenerate(
        &self,
        chat_id: &str,
        harness: HarnessId,
        prompt: &str,
        cwd: &str,
    ) -> Result<(), EngineError> {
        self.generate(
            chat_id,
            harness,
            prompt,
            cwd,
            GenerationMode::ReplaceCurrent,
        )
        .await
    }

    async fn generate(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        prompt: &str,
        cwd: &str,
        mode: GenerationMode,
    ) -> Result<(), EngineError> {
        let chat = self
            .inner
            .workspace
            .chat(chat_id)?
            .ok_or_else(|| EngineError::Other("chat has no workspace row".into()))?;
        let original_title = chat.title.clone();
        if mode == GenerationMode::Untitled
            && original_title
                .as_deref()
                .is_some_and(|title| !title.trim().is_empty())
        {
            return Ok(()); // already named
        }

        let generated = self.run_title_model(chat_id, harness_id, prompt, cwd).await;
        // Fallback so a chat is always named even if the model run produced nothing.
        let fallback: String = prompt
            .split_whitespace()
            .take(7)
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(48)
            .collect();
        let title = generated.unwrap_or(fallback);
        if title.is_empty() {
            return Ok(());
        }

        // Re-read after the model call: a concurrent user rename always wins.
        let latest = self.inner.workspace.chat(chat_id)?.unwrap_or(chat);
        if latest.title != original_title {
            return Ok(());
        }

        // Initial automatic generation may rename an untouched Jolt worktree
        // branch. Regeneration changes only the requested session name.
        if mode == GenerationMode::Untitled
            && let (Some(chat_cwd), Some(branch)) = (&latest.cwd, &latest.branch)
            && branch.starts_with("jolt/")
        {
            match self
                .inner
                .repos
                .rename_worktree_branch(std::path::Path::new(chat_cwd), branch, &title)
                .await
            {
                Ok(renamed) if &renamed != branch => {
                    if let Err(err) = self.inner.workspace.set_chat_branch(chat_id, &renamed) {
                        tracing::warn!(chat = %chat_id, error = %err, "chat branch update failed");
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "automatic worktree branch rename failed");
                }
            }
        }

        self.inner.workspace.rename_chat(chat_id, &title)?;
        match mode {
            GenerationMode::Untitled => {
                tracing::info!(chat = %chat_id, title = %title, "chat auto-titled");
            }
            GenerationMode::ReplaceCurrent => {
                tracing::info!(chat = %chat_id, title = %title, "chat title regenerated");
            }
        }
        Ok(())
    }

    /// Generate a commit message from one immutable selected diff. This uses the
    /// same economy-model, read-only, non-persistent path as chat titles.
    pub(crate) async fn generate_commit_message(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        cwd: &str,
        paths: &[String],
        patch: &str,
    ) -> Result<String, EngineError> {
        let harness = self.inner.registry.resolve(harness_id)?;
        let cheap = cheap_model_id(&harness.models().await.unwrap_or_default(), None);
        let bounded_patch = patch.chars().take(50_000).collect::<String>();
        let path_list = paths
            .iter()
            .take(200)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "Write a concise commit message for the selected changes below. Follow the repository's existing commit-message style. Reply with ONLY the commit message: a short imperative subject on the first line and an optional explanatory body after one blank line. Do not use Markdown fences.\n\nFiles:\n{path_list}\n\nDiff:\n{bounded_patch}"
        );
        let mut model_options = serde_json::Map::new();
        if harness_id == HarnessId::Pi {
            model_options.insert("projectTrust".into(), serde_json::json!("ignore"));
            model_options.insert("toolAccess".into(), serde_json::json!("readOnly"));
        }
        let request = RunRequest {
            prompt,
            harness: Some(harness_id),
            model: cheap.clone(),
            reasoning: Some(ReasoningLevel::Minimal),
            model_options,
            cwd: cwd.to_string(),
            sandbox: SandboxLevel::ReadOnly,
            auto_approve: true,
            attachments: Vec::new(),
            resume: None,
        };
        let mut usage = UsageCapture::new(
            self.inner.usage.clone(),
            chat_id,
            UsagePurpose::CommitMessageGeneration,
            harness_id,
            cheap.as_deref(),
            cwd,
        );
        let raw = collect_text(harness.as_ref(), request, |event| {
            if let Err(error) = usage.observe(event) {
                tracing::error!(chat = %chat_id, %error, "commit-message usage ledger write failed");
            }
        })
        .await?;
        let message = raw
            .trim()
            .trim_start_matches("```text")
            .trim_start_matches("```gitcommit")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
            .chars()
            .take(10_000)
            .collect::<String>();
        if message.lines().next().unwrap_or_default().trim().is_empty() {
            return Err(EngineError::Other(
                "Commit-message generation returned no message".into(),
            ));
        }
        Ok(message)
    }

    /// One-shot titling run: collect TextDeltas until Done; retries on failure.
    async fn run_title_model(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        prompt: &str,
        cwd: &str,
    ) -> Option<String> {
        let harness = match self.inner.registry.resolve(harness_id) {
            Ok(harness) => harness,
            Err(err) => {
                tracing::debug!(error = %err, "titling harness unavailable");
                return None;
            }
        };
        let cheap = cheap_model_id(&harness.models().await.unwrap_or_default(), None);
        let title_prompt = format!(
            "Reply with ONLY a concise 3-5 word title in sentence case (capitalize only the first \
             word and proper nouns; no quotes, no punctuation) for a coding session that begins \
             with this request:\n\n{prompt}"
        );
        let mut model_options = serde_json::Map::new();
        // Titling never needs repository-provided Pi settings/extensions and
        // has no interactive input surface. Explicitly ignore project
        // resources instead of triggering (and auto-cancelling) a trust ask.
        if harness_id == HarnessId::Pi {
            model_options.insert("projectTrust".into(), serde_json::json!("ignore"));
            model_options.insert("toolAccess".into(), serde_json::json!("readOnly"));
        }
        for attempt in 0..=RETRY_DELAYS_MS.len() {
            let request = RunRequest {
                prompt: title_prompt.clone(),
                harness: Some(harness_id),
                model: cheap.clone(),
                reasoning: Some(ReasoningLevel::Minimal),
                model_options: model_options.clone(),
                cwd: cwd.to_string(),
                sandbox: SandboxLevel::ReadOnly,
                auto_approve: true,
                attachments: Vec::new(),
                resume: None,
            };
            let mut usage = UsageCapture::new(
                self.inner.usage.clone(),
                chat_id,
                UsagePurpose::TitleGeneration,
                harness_id,
                cheap.as_deref(),
                cwd,
            );
            match collect_text(harness.as_ref(), request, |event| {
                if let Err(error) = usage.observe(event) {
                    tracing::error!(chat = %chat_id, %error, "title usage ledger write failed");
                }
            })
            .await
            {
                Ok(raw) => {
                    let candidate = clean_title(&raw);
                    if !candidate.is_empty() {
                        return Some(candidate);
                    }
                }
                Err(err) => {
                    tracing::warn!(attempt = attempt + 1, error = %err,
                        "automatic chat title generation attempt failed");
                }
            }
            if let Some(delay) = RETRY_DELAYS_MS.get(attempt) {
                tokio::time::sleep(std::time::Duration::from_millis(*delay)).await;
            }
        }
        None
    }
}

/// First line, stripped of quote/heading dressing, capped at 60 chars.
fn clean_title(raw: &str) -> String {
    let first = raw.trim().lines().next().unwrap_or("");
    first
        .trim_start_matches(['"', '\'', '#', ' ', '\t'])
        .trim_end_matches(['"', '\'', ' ', '\t'])
        .chars()
        .take(60)
        .collect()
}

/// Drive one titling run through the harness: no steering, questions resolved
/// empty immediately (a titling prompt must never block on input).
pub(crate) async fn collect_text(
    harness: &dyn jolt_harness::Harness,
    request: RunRequest,
    mut observe: impl FnMut(&AgentEvent),
) -> Result<String, EngineError> {
    let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<SteerMessage>(1);
    let controls = RunControls {
        persist_session: false,
        mcp: None,
        request_input: Box::new(|_questions: Vec<UserInputQuestion>| {
            let (tx, rx) = tokio::sync::oneshot::channel::<Vec<UserInputAnswer>>();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        bash: tokio::sync::mpsc::channel(1).1,
        interrupt: CancellationToken::new(),
    };
    let mut stream = harness.run(request, controls).await?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        let event = event?;
        observe(&event);
        match event {
            AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
            AgentEvent::Error { message } => {
                return Err(EngineError::Other(format!("titling run error: {message}")));
            }
            AgentEvent::Done { status, error, .. } => {
                if status == DoneStatus::Completed {
                    break;
                }
                return Err(EngineError::Other(format!(
                    "titling run ended {status:?}: {}",
                    error.unwrap_or_default()
                )));
            }
            _ => {}
        }
    }
    drop(steer_tx); // keep the mailbox open for the run's whole lifetime
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn titles_are_cleaned() {
        assert_eq!(clean_title("\"Fix Login Flow\"\nextra"), "Fix Login Flow");
        assert_eq!(clean_title("# Add Dark Mode  "), "Add Dark Mode");
        assert_eq!(clean_title("   "), "");
    }

    #[tokio::test]
    async fn title_collection_records_reported_usage() {
        use jolt_harness::mock::MockHarness;

        let dir = tempfile::tempdir().unwrap();
        let store = UsageStore::open(&dir.path().join("usage.sqlite"), "d1".into()).unwrap();
        let harness = MockHarness {
            script: vec![
                AgentEvent::Usage {
                    input_tokens: 8,
                    output_tokens: 2,
                    cache_read_input_tokens: 3,
                    cache_write_input_tokens: 0,
                    cost_usd: Some(0.01),
                    cost_provenance: None,
                    context_tokens: Some(11),
                    context_window: Some(200_000),
                },
                AgentEvent::TextDelta {
                    text: "Track internal usage".into(),
                },
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                },
            ],
        };
        let request = RunRequest {
            prompt: "title this".into(),
            harness: Some(HarnessId::Mock),
            model: Some("haiku".into()),
            reasoning: Some(ReasoningLevel::Minimal),
            model_options: serde_json::Map::new(),
            cwd: "/repo".into(),
            sandbox: SandboxLevel::ReadOnly,
            auto_approve: true,
            attachments: Vec::new(),
            resume: None,
        };
        let mut usage = UsageCapture::new(
            store.clone(),
            "c1",
            UsagePurpose::TitleGeneration,
            HarnessId::Mock,
            Some("haiku"),
            "/repo",
        );
        let title = collect_text(&harness, request, |event| usage.observe(event).unwrap())
            .await
            .unwrap();

        assert_eq!(title, "Track internal usage");
        let summary = store.summary("c1").unwrap();
        assert_eq!(summary.calls, 1);
        assert_eq!(summary.total_tokens(), 13);
        assert_eq!(summary.model, None);
        assert_eq!(store.breakdown(30).unwrap().rows[0].model, "haiku");
    }
}
