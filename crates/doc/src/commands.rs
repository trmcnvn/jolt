//! Durable command ledger.
//!
//! Rules:
//! 1. Each device inserts only its own entries; entries are append-only and immutable.
//! 2. The chat's HOST is the sole writer of command outcomes; a composer may only set
//!    `cancelled` on its own still-pending entries.
//! 3. Evaluation (`evaluate_command`, pure): processed-id dedupe → Skip; expired TTL → Expired;
//!    a newer command of the same kind supersedes steer/interrupt; an interrupt whose
//!    `based_on.turn_id` is already past → Superseded; otherwise Execute.

use serde::{Deserialize, Serialize};

use jolt_proto::{RunRequest, UserInputAnswer};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum GoalOperation {
    Create {
        objective: String,
        #[serde(default)]
        token_budget: Option<u64>,
    },
    Edit {
        goal_id: String,
        expected_revision: u64,
        objective: String,
        #[serde(default)]
        token_budget: Option<u64>,
    },
    Pause {
        goal_id: String,
        expected_revision: u64,
    },
    Resume {
        goal_id: String,
        expected_revision: u64,
    },
    Clear {
        goal_id: String,
        expected_revision: u64,
    },
}

use crate::constants::COMMAND_DEFAULT_TTL_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionCommandKind {
    Run,
    Queue,
    ResumeQueue,
    Bash,
    Steer,
    Interrupt,
    RespondInput,
    Goal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionCommandStatus {
    Pending,
    Applied,
    Rejected,
    Expired,
    Superseded,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SessionCommandPayload {
    #[serde(rename_all = "camelCase")]
    Run {
        request: RunRequest,
        /// Client-minted message id for the optimistic user entry (dedup key).
        message_id: String,
    },
    /// An agent turn whose prompt is sent to the harness but omitted from the
    /// user-visible transcript. Native Jolt commands use this for control
    /// prompts that should produce only an assistant response.
    #[serde(rename_all = "camelCase")]
    HiddenPrompt {
        request: RunRequest,
    },
    /// A user prompt held by Jolt until the current turn completes. Unlike a
    /// steer, it is not delivered into that active turn; the pending FIFO batch
    /// drains together when the next turn boundary opens.
    #[serde(rename_all = "camelCase")]
    Queue {
        request: RunRequest,
        /// Client-minted message id written only when the queued turn starts.
        message_id: String,
    },
    /// Resume a queue paused by an interrupted or errored turn.
    ResumeQueue {},
    #[serde(rename_all = "camelCase")]
    Bash {
        command: String,
        exclude_from_context: bool,
        cwd: String,
        /// Client-minted id for the resulting system transcript entry.
        message_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Steer {
        prompt: String,
        message_id: Option<String>,
    },
    Interrupt {},
    #[serde(rename_all = "camelCase")]
    RespondInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    Goal {
        operation: GoalOperation,
    },
}

impl SessionCommandPayload {
    pub fn kind(&self) -> SessionCommandKind {
        match self {
            SessionCommandPayload::Run { .. } | SessionCommandPayload::HiddenPrompt { .. } => {
                SessionCommandKind::Run
            }
            SessionCommandPayload::Queue { .. } => SessionCommandKind::Queue,
            SessionCommandPayload::ResumeQueue {} => SessionCommandKind::ResumeQueue,
            SessionCommandPayload::Bash { .. } => SessionCommandKind::Bash,
            SessionCommandPayload::Steer { .. } => SessionCommandKind::Steer,
            SessionCommandPayload::Interrupt {} => SessionCommandKind::Interrupt,
            SessionCommandPayload::RespondInput { .. } => SessionCommandKind::RespondInput,
            SessionCommandPayload::Goal { .. } => SessionCommandKind::Goal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandBasedOn {
    pub turn_id: Option<String>,
    pub frontier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCommandEntry {
    pub id: String,
    pub payload: SessionCommandPayload,
    pub issued_by: String,
    /// Epoch millis.
    pub issued_at: i64,
    #[serde(default)]
    pub based_on: Option<CommandBasedOn>,
    /// Epoch millis; defaults to issued_at + COMMAND_DEFAULT_TTL_MS when absent.
    #[serde(default)]
    pub expires_at: Option<i64>,
    pub status: SessionCommandStatus,
    #[serde(default)]
    pub resolution: Option<String>,
}

/// Pending queued turn projected to clients without exposing the full run
/// configuration carried by the command ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedPrompt {
    pub command_id: String,
    pub message_id: String,
    pub prompt: String,
    pub issued_at: i64,
    pub cancellable: bool,
}

impl SessionCommandEntry {
    pub fn kind(&self) -> SessionCommandKind {
        self.payload.kind()
    }

    pub fn effective_expiry(&self) -> i64 {
        self.expires_at
            .unwrap_or(self.issued_at + COMMAND_DEFAULT_TTL_MS)
    }
}

/// Rule 2: only the composer that issued a still-pending command may cancel it.
pub fn can_composer_cancel(entry: &SessionCommandEntry, device_id: &str) -> bool {
    entry.status == SessionCommandStatus::Pending && entry.issued_by == device_id
}

/// Project pending queue commands in FIFO document order.
pub fn queued_prompts(entries: &[SessionCommandEntry], device_id: &str) -> Vec<QueuedPrompt> {
    entries
        .iter()
        .filter_map(|entry| {
            let SessionCommandPayload::Queue {
                request,
                message_id,
            } = &entry.payload
            else {
                return None;
            };
            (entry.status == SessionCommandStatus::Pending).then(|| QueuedPrompt {
                command_id: entry.id.clone(),
                message_id: message_id.clone(),
                prompt: request.prompt.clone(),
                issued_at: entry.issued_at,
                cancellable: can_composer_cancel(entry, device_id),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandDisposition {
    /// Already in the processed ledger — do nothing (idempotence).
    Skip,
    /// Mark expired.
    Expired,
    /// Mark superseded.
    Superseded,
    /// Mark processed BEFORE executing, then execute.
    Execute,
}

/// Context the host evaluates a pending command against.
pub struct EvaluationContext<'a> {
    /// Processed-command ledger membership test.
    pub is_processed: &'a dyn Fn(&str) -> bool,
    /// Current wall clock, epoch millis.
    pub now_ms: i64,
    /// All command entries in doc order (used to find newer same-kind entries).
    pub entries: &'a [SessionCommandEntry],
    /// The id of the turn currently (or most recently) running, if any.
    pub current_turn_id: Option<&'a str>,
    /// True when the given turn id has already completed.
    pub turn_is_past: &'a dyn Fn(&str) -> bool,
}

/// Rule 3 — pure evaluation of a single pending command.
pub fn evaluate_command(
    entry: &SessionCommandEntry,
    cx: &EvaluationContext<'_>,
) -> CommandDisposition {
    if (cx.is_processed)(&entry.id) {
        return CommandDisposition::Skip;
    }
    if cx.now_ms >= entry.effective_expiry() {
        return CommandDisposition::Expired;
    }
    // A newer pending command of the same kind supersedes steer/interrupt.
    let kind = entry.kind();
    if matches!(
        kind,
        SessionCommandKind::Steer | SessionCommandKind::Interrupt
    ) {
        let has_newer_same_kind = cx.entries.iter().any(|other| {
            other.id != entry.id
                && other.kind() == kind
                && other.status == SessionCommandStatus::Pending
                && other.issued_at > entry.issued_at
        });
        if has_newer_same_kind {
            return CommandDisposition::Superseded;
        }
    }
    // An interrupt aimed at a turn that already finished is moot.
    if kind == SessionCommandKind::Interrupt
        && let Some(based_on) = &entry.based_on
        && let Some(turn_id) = &based_on.turn_id
    {
        let is_current = cx.current_turn_id == Some(turn_id.as_str());
        if !is_current && (cx.turn_is_past)(turn_id) {
            return CommandDisposition::Superseded;
        }
    }
    CommandDisposition::Execute
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, payload: SessionCommandPayload, issued_at: i64) -> SessionCommandEntry {
        SessionCommandEntry {
            id: id.into(),
            payload,
            issued_by: "device-a".into(),
            issued_at,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        }
    }

    fn steer(id: &str, issued_at: i64) -> SessionCommandEntry {
        entry(
            id,
            SessionCommandPayload::Steer {
                prompt: "go".into(),
                message_id: None,
            },
            issued_at,
        )
    }

    fn cx<'a>(
        entries: &'a [SessionCommandEntry],
        processed: &'a dyn Fn(&str) -> bool,
        turn_is_past: &'a dyn Fn(&str) -> bool,
        now_ms: i64,
        current_turn_id: Option<&'a str>,
    ) -> EvaluationContext<'a> {
        EvaluationContext {
            is_processed: processed,
            now_ms,
            entries,
            current_turn_id,
            turn_is_past,
        }
    }

    const NEVER: fn(&str) -> bool = |_| false;

    #[test]
    fn processed_commands_are_skipped() {
        let e = steer("c1", 1_000);
        let entries = vec![e.clone()];
        let processed = |id: &str| id == "c1";
        let cx = cx(&entries, &processed, &NEVER, 2_000, None);
        assert_eq!(evaluate_command(&e, &cx), CommandDisposition::Skip);
    }

    #[test]
    fn expired_commands_are_expired() {
        let e = steer("c1", 0);
        let entries = vec![e.clone()];
        let cx = cx(&entries, &NEVER, &NEVER, COMMAND_DEFAULT_TTL_MS + 1, None);
        assert_eq!(evaluate_command(&e, &cx), CommandDisposition::Expired);
    }

    #[test]
    fn newer_steer_supersedes_older_pending_steer() {
        let older = steer("c1", 1_000);
        let newer = steer("c2", 2_000);
        let entries = vec![older.clone(), newer.clone()];
        let cx1 = cx(&entries, &NEVER, &NEVER, 3_000, None);
        assert_eq!(
            evaluate_command(&older, &cx1),
            CommandDisposition::Superseded
        );
        assert_eq!(evaluate_command(&newer, &cx1), CommandDisposition::Execute);
    }

    #[test]
    fn interrupt_for_past_turn_is_superseded() {
        let mut e = entry("c1", SessionCommandPayload::Interrupt {}, 1_000);
        e.based_on = Some(CommandBasedOn {
            turn_id: Some("turn-1".into()),
            frontier: None,
        });
        let entries = vec![e.clone()];
        let past = |id: &str| id == "turn-1";
        let cx1 = cx(&entries, &NEVER, &past, 2_000, Some("turn-2"));
        assert_eq!(evaluate_command(&e, &cx1), CommandDisposition::Superseded);
        // …but if that turn is still the current one, execute.
        let cx2 = cx(&entries, &NEVER, &past, 2_000, Some("turn-1"));
        assert_eq!(evaluate_command(&e, &cx2), CommandDisposition::Execute);
    }

    #[test]
    fn runs_are_not_superseded_by_newer_runs() {
        // Two queued runs both execute (in order); supersession applies to steer/interrupt only.
        let r1 = entry(
            "r1",
            SessionCommandPayload::Run {
                request: run_request(),
                message_id: "m1".into(),
            },
            1_000,
        );
        let r2 = entry(
            "r2",
            SessionCommandPayload::Run {
                request: run_request(),
                message_id: "m2".into(),
            },
            2_000,
        );
        let entries = vec![r1.clone(), r2.clone()];
        let cx1 = cx(&entries, &NEVER, &NEVER, 3_000, None);
        assert_eq!(evaluate_command(&r1, &cx1), CommandDisposition::Execute);
        assert_eq!(evaluate_command(&r2, &cx1), CommandDisposition::Execute);
    }

    #[test]
    fn queued_prompt_projection_is_fifo_and_device_scoped() {
        let mut first = entry(
            "q1",
            SessionCommandPayload::Queue {
                request: run_request(),
                message_id: "m1".into(),
            },
            1_000,
        );
        first.issued_by = "device-a".into();
        let mut second = entry(
            "q2",
            SessionCommandPayload::Queue {
                request: RunRequest {
                    prompt: "second".into(),
                    ..run_request()
                },
                message_id: "m2".into(),
            },
            2_000,
        );
        second.issued_by = "device-b".into();
        let prompts = queued_prompts(&[first, second], "device-a");
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0].prompt, "hello");
        assert!(prompts[0].cancellable);
        assert!(!prompts[1].cancellable);
    }

    #[test]
    fn composer_cancel_rules() {
        let e = steer("c1", 1_000);
        assert!(can_composer_cancel(&e, "device-a"));
        assert!(!can_composer_cancel(&e, "device-b"));
        let mut applied = e.clone();
        applied.status = SessionCommandStatus::Applied;
        assert!(!can_composer_cancel(&applied, "device-a"));
    }

    fn run_request() -> RunRequest {
        RunRequest {
            prompt: "hello".into(),
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "/tmp".into(),
            sandbox: jolt_proto::SandboxLevel::WorkspaceWrite,
            auto_approve: false,
            attachments: Vec::new(),
            resume: None,
        }
    }
}
