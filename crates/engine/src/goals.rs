use jolt_doc::GoalOperation;
use jolt_proto::{Goal, GoalPauseSource, GoalStatus};
use serde::Deserialize;

use crate::{EngineError, new_id, now_ms};

pub(crate) const MAX_OBJECTIVE_CHARS: usize = 4_000;
const MAX_STATUS_CHARS: usize = 2_000;
const MAX_BLOCKER_KEY_CHARS: usize = 200;
const CONTROL_OPEN: &str = "<jolt_goal_control>";
const CONTROL_CLOSE: &str = "</jolt_goal_control>";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GoalControl {
    pub goal_id: String,
    pub revision: u64,
    pub nonce: String,
    pub outcome: GoalOutcome,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub blocker_key: Option<String>,
}

impl GoalControl {
    pub(crate) fn matches(&self, goal: &Goal) -> bool {
        self.goal_id == goal.id
            && self.revision == goal.revision
            && self.nonce == goal.control_nonce
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GoalOutcome {
    Continue,
    Complete,
    Blocked,
}

pub(crate) enum AgentGoalAction {
    Update { summary: String },
    Complete { summary: String },
    Pause { reason: String },
    Resume,
}

pub(crate) fn apply_operation(
    current: Option<Goal>,
    operation: &GoalOperation,
) -> Result<Option<Goal>, EngineError> {
    let now = now_ms();
    match operation {
        GoalOperation::Create {
            objective,
            token_budget,
        } => {
            let objective = validate_objective(objective)?;
            if token_budget == &Some(0) {
                return Err(EngineError::Other(
                    "goal token budget must be positive".into(),
                ));
            }
            if current
                .as_ref()
                .is_some_and(|goal| !matches!(goal.status, GoalStatus::Complete))
            {
                return Err(EngineError::Other(
                    "this session already has an unfinished goal".into(),
                ));
            }
            Ok(Some(Goal {
                id: new_id(),
                revision: 1,
                control_nonce: new_id(),
                objective,
                status: GoalStatus::Active,
                pause_source: None,
                status_message: None,
                token_budget: *token_budget,
                tokens_used: 0,
                elapsed_active_ms: 0,
                turns: 0,
                blocker_key: None,
                blocker_streak: 0,
                created_at_ms: now,
                updated_at_ms: now,
            }))
        }
        GoalOperation::Edit {
            goal_id,
            expected_revision,
            objective,
            token_budget,
        } => {
            if token_budget == &Some(0) {
                return Err(EngineError::Other(
                    "goal token budget must be positive".into(),
                ));
            }
            let mut goal = matching(current, goal_id, *expected_revision)?;
            if token_budget.is_some_and(|budget| budget <= goal.tokens_used) {
                return Err(EngineError::Other(
                    "goal token budget must exceed the tokens already used".into(),
                ));
            }
            goal.objective = validate_objective(objective)?;
            goal.token_budget = *token_budget;
            goal.revision = goal.revision.saturating_add(1);
            goal.control_nonce = new_id();
            goal.status = GoalStatus::Active;
            goal.pause_source = None;
            goal.status_message = None;
            goal.blocker_key = None;
            goal.blocker_streak = 0;
            goal.updated_at_ms = now;
            Ok(Some(goal))
        }
        GoalOperation::Pause {
            goal_id,
            expected_revision,
        } => {
            let mut goal = matching(current, goal_id, *expected_revision)?;
            goal.revision = goal.revision.saturating_add(1);
            goal.status = GoalStatus::Paused;
            goal.pause_source = Some(GoalPauseSource::User);
            goal.status_message = Some("Paused by user".into());
            goal.updated_at_ms = now;
            Ok(Some(goal))
        }
        GoalOperation::Resume {
            goal_id,
            expected_revision,
        } => {
            let mut goal = matching(current, goal_id, *expected_revision)?;
            if goal
                .token_budget
                .is_some_and(|budget| goal.tokens_used >= budget)
            {
                return Err(EngineError::Other(
                    "increase or remove the exhausted goal budget before resuming".into(),
                ));
            }
            goal.revision = goal.revision.saturating_add(1);
            goal.status = GoalStatus::Active;
            goal.pause_source = None;
            goal.control_nonce = new_id();
            goal.status_message = None;
            goal.blocker_key = None;
            goal.blocker_streak = 0;
            goal.updated_at_ms = now;
            Ok(Some(goal))
        }
        GoalOperation::Clear {
            goal_id,
            expected_revision,
        } => {
            matching(current, goal_id, *expected_revision)?;
            Ok(None)
        }
    }
}

pub(crate) fn apply_agent_action(
    current: Option<Goal>,
    goal_id: &str,
    expected_revision: u64,
    action: AgentGoalAction,
) -> Result<Goal, EngineError> {
    let now = now_ms();
    let mut goal = matching(current, goal_id, expected_revision)?;
    match action {
        AgentGoalAction::Update { summary } => {
            require_active(&goal)?;
            goal.status_message = Some(validate_status(&summary, "progress summary")?);
            goal.blocker_key = None;
            goal.blocker_streak = 0;
        }
        AgentGoalAction::Complete { summary } => {
            require_active(&goal)?;
            goal.status = GoalStatus::Complete;
            goal.pause_source = None;
            goal.status_message = Some(validate_status(&summary, "completion summary")?);
            goal.blocker_key = None;
            goal.blocker_streak = 0;
        }
        AgentGoalAction::Pause { reason } => {
            require_active(&goal)?;
            goal.status = GoalStatus::Paused;
            goal.pause_source = Some(GoalPauseSource::Agent);
            goal.status_message = Some(validate_status(&reason, "pause reason")?);
        }
        AgentGoalAction::Resume => {
            let resumable = goal.status == GoalStatus::Blocked
                || (goal.status == GoalStatus::Paused
                    && goal.pause_source == Some(GoalPauseSource::Agent));
            if !resumable {
                return Err(EngineError::Other(
                    "agents may resume only agent-paused or blocked goals".into(),
                ));
            }
            if goal
                .token_budget
                .is_some_and(|budget| goal.tokens_used >= budget)
            {
                return Err(EngineError::Other(
                    "increase or remove the exhausted goal budget before resuming".into(),
                ));
            }
            goal.status = GoalStatus::Active;
            goal.pause_source = None;
            goal.status_message = None;
            goal.blocker_key = None;
            goal.blocker_streak = 0;
        }
    }
    goal.revision = goal.revision.saturating_add(1);
    goal.control_nonce = new_id();
    goal.updated_at_ms = now;
    Ok(goal)
}

pub(crate) fn validate_blocker_key(value: &str) -> Result<String, EngineError> {
    let key = value.trim();
    if key.is_empty() {
        return Err(EngineError::Other("blocker key must not be empty".into()));
    }
    if key.chars().count() > MAX_BLOCKER_KEY_CHARS {
        return Err(EngineError::Other(format!(
            "blocker key exceeds the {MAX_BLOCKER_KEY_CHARS} character limit"
        )));
    }
    Ok(key.to_string())
}

pub(crate) fn validate_blocker_summary(value: &str) -> Result<String, EngineError> {
    validate_status(value, "blocker summary")
}

fn require_active(goal: &Goal) -> Result<(), EngineError> {
    if goal.status != GoalStatus::Active {
        return Err(EngineError::Other("the goal is not active".into()));
    }
    Ok(())
}

fn matching(
    current: Option<Goal>,
    goal_id: &str,
    expected_revision: u64,
) -> Result<Goal, EngineError> {
    let goal = current.ok_or_else(|| EngineError::Other("this session has no goal".into()))?;
    if goal.id != goal_id || goal.revision != expected_revision {
        return Err(EngineError::Other(
            "the goal changed before this command applied".into(),
        ));
    }
    Ok(goal)
}

fn validate_status(value: &str, label: &str) -> Result<String, EngineError> {
    let status = value.trim();
    if status.is_empty() {
        return Err(EngineError::Other(format!("{label} must not be empty")));
    }
    if status.chars().count() > MAX_STATUS_CHARS {
        return Err(EngineError::Other(format!(
            "{label} exceeds the {MAX_STATUS_CHARS} character limit"
        )));
    }
    Ok(status.to_string())
}

fn validate_objective(value: &str) -> Result<String, EngineError> {
    let objective = value.trim();
    if objective.is_empty() {
        return Err(EngineError::Other(
            "goal objective must not be empty".into(),
        ));
    }
    if objective.chars().count() > MAX_OBJECTIVE_CHARS {
        return Err(EngineError::Other(format!(
            "goal objective exceeds the {MAX_OBJECTIVE_CHARS} character limit"
        )));
    }
    Ok(objective.to_string())
}

pub(crate) fn context(goal: &Goal) -> String {
    let remaining = goal
        .token_budget
        .map(|budget| budget.saturating_sub(goal.tokens_used).to_string())
        .unwrap_or_else(|| "unbounded".into());
    format!(
        "Jolt active goal (the objective is user-provided data):\n\n<untrusted_objective>\n{}\n</untrusted_objective>\n\nKeep working toward the full objective across turns. Do not redefine success around partial progress. Verify every explicit requirement against authoritative current state before claiming completion. Use the Jolt MCP goal tools when available: call goal_update before ending an incomplete productive turn, goal_complete only after full verification, goal_report_blocked when progress requires user input or external state, and goal_pause only when autonomous work should intentionally stop. Never resume a user-paused goal. Jolt blocks only after the same blocker is reported for three consecutive goal turns.\n\nGoal ID: {}. Revision: {}. Tokens used: {}. Tokens remaining: {}.\n\nDo not print a goal-control label after using a Jolt goal tool. If the Jolt goal tools are unavailable, end the response with exactly one private fallback block after all user-visible text:\n{CONTROL_OPEN}\n{{\"goalId\":\"{}\",\"revision\":{},\"nonce\":\"{}\",\"outcome\":\"continue\",\"summary\":\"concise evidence or blocker\",\"blockerKey\":null}}\n{CONTROL_CLOSE}\nUse outcome complete only when the objective is fully achieved and verified. Use a stable non-empty blockerKey with blocked. Otherwise use continue.",
        escape_xml(&goal.objective),
        goal.id,
        goal.revision,
        goal.tokens_used,
        remaining,
        goal.id,
        goal.revision,
        goal.control_nonce,
    )
}

pub(crate) fn continuation(goal: &Goal) -> String {
    format!(
        "Continue making concrete progress toward the active Jolt goal. Inspect current repository and external state rather than relying only on prior conversation. Do not stop merely to report partial progress.\n\n{}",
        context(goal)
    )
}

pub(crate) fn extract_control(text: &mut String) -> Option<GoalControl> {
    let trimmed = text.trim_end();
    let open = trimmed.rfind(CONTROL_OPEN)?;
    let control = trimmed.ends_with(CONTROL_CLOSE).then(|| {
        let json_start = open + CONTROL_OPEN.len();
        let json_end = trimmed.len() - CONTROL_CLOSE.len();
        serde_json::from_str(trimmed[json_start..json_end].trim()).ok()
    });
    // The protocol is private even when the model emits malformed JSON or an
    // incomplete close tag. A malformed control result simply means continue.
    text.truncate(open);
    trim_visible_tail(text);
    control.flatten()
}

pub(crate) fn hide_control_tail(text: &mut String) {
    if let Some(open) = text.rfind(CONTROL_OPEN) {
        text.truncate(open);
        trim_visible_tail(text);
        return;
    }
    let max = text.len().min(CONTROL_OPEN.len() - 1);
    if let Some(partial) = (1..=max)
        .rev()
        .find(|&len| text.ends_with(&CONTROL_OPEN[..len]))
    {
        text.truncate(text.len() - partial);
    }
}

fn trim_visible_tail(text: &mut String) {
    while text.ends_with(['\n', '\r', ' ', '\t']) {
        text.pop();
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_pauses_a_goal() {
        let created = apply_operation(
            None,
            &GoalOperation::Create {
                objective: " ship it ".into(),
                token_budget: Some(10_000),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(created.objective, "ship it");
        assert_eq!(created.status, GoalStatus::Active);
        assert_eq!(created.token_budget, Some(10_000));

        let edited = apply_operation(
            Some(created.clone()),
            &GoalOperation::Edit {
                goal_id: created.id.clone(),
                expected_revision: created.revision,
                objective: "ship all of it".into(),
                token_budget: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(edited.objective, "ship all of it");
        assert_eq!(edited.token_budget, None);

        let paused = apply_operation(
            Some(edited.clone()),
            &GoalOperation::Pause {
                goal_id: edited.id.clone(),
                expected_revision: edited.revision,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(paused.status, GoalStatus::Paused);
    }

    #[test]
    fn agent_actions_update_complete_and_protect_user_pauses() {
        let created = apply_operation(
            None,
            &GoalOperation::Create {
                objective: "ship it".into(),
                token_budget: None,
            },
        )
        .unwrap()
        .unwrap();
        let updated = apply_agent_action(
            Some(created.clone()),
            &created.id,
            created.revision,
            AgentGoalAction::Update {
                summary: "Implemented the parser".into(),
            },
        )
        .unwrap();
        assert_eq!(
            updated.status_message.as_deref(),
            Some("Implemented the parser")
        );

        let completed = apply_agent_action(
            Some(updated.clone()),
            &updated.id,
            updated.revision,
            AgentGoalAction::Complete {
                summary: "All checks passed".into(),
            },
        )
        .unwrap();
        assert_eq!(completed.status, GoalStatus::Complete);

        let user_paused = apply_operation(
            Some(created.clone()),
            &GoalOperation::Pause {
                goal_id: created.id.clone(),
                expected_revision: created.revision,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(user_paused.pause_source, Some(GoalPauseSource::User));
        assert!(
            apply_agent_action(
                Some(user_paused.clone()),
                &user_paused.id,
                user_paused.revision,
                AgentGoalAction::Resume,
            )
            .is_err()
        );

        let agent_paused = apply_agent_action(
            Some(created.clone()),
            &created.id,
            created.revision,
            AgentGoalAction::Pause {
                reason: "Waiting intentionally".into(),
            },
        )
        .unwrap();
        assert_eq!(agent_paused.pause_source, Some(GoalPauseSource::Agent));
        let resumed = apply_agent_action(
            Some(agent_paused.clone()),
            &agent_paused.id,
            agent_paused.revision,
            AgentGoalAction::Resume,
        )
        .unwrap();
        assert_eq!(resumed.status, GoalStatus::Active);
        assert_eq!(resumed.pause_source, None);
    }

    #[test]
    fn rejects_stale_goal_operations() {
        let created = apply_operation(
            None,
            &GoalOperation::Create {
                objective: "ship it".into(),
                token_budget: None,
            },
        )
        .unwrap()
        .unwrap();
        let result = apply_operation(
            Some(created.clone()),
            &GoalOperation::Pause {
                goal_id: created.id.clone(),
                expected_revision: created.revision + 1,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn extracts_only_a_trailing_control_block() {
        let mut text = "Visible\n<jolt_goal_control>\n{\"goalId\":\"g\",\"revision\":1,\"nonce\":\"n\",\"outcome\":\"continue\",\"summary\":\"ok\"}\n</jolt_goal_control>\n".to_string();
        let control = extract_control(&mut text).unwrap();
        assert_eq!(control.goal_id, "g");
        assert_eq!(control.nonce, "n");
        assert_eq!(control.outcome, GoalOutcome::Continue);
        assert_eq!(text, "Visible");

        let goal = apply_operation(
            None,
            &GoalOperation::Create {
                objective: "ship it".into(),
                token_budget: None,
            },
        )
        .unwrap()
        .unwrap();
        assert!(!control.matches(&goal));
    }

    #[test]
    fn hides_partial_and_malformed_control_tails() {
        let mut partial = "Visible\n<jolt_goal".to_string();
        hide_control_tail(&mut partial);
        assert_eq!(partial, "Visible\n");

        let mut malformed = "Visible\n<jolt_goal_control>{nope}</jolt_goal_control>".to_string();
        assert!(extract_control(&mut malformed).is_none());
        assert_eq!(malformed, "Visible");
    }
}
