use serde::{Deserialize, Serialize};

use crate::{CheckoutDiffManifest, VcsKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutVcsStatus {
    pub checkout_id: String,
    pub backend: VcsKind,
    pub reference: String,
    pub working_copy: CheckoutDiffManifest,
    pub publication: VcsPublicationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum VcsPublicationState {
    NoRemote,
    NoCompletedChanges {
        target: VcsPublishTarget,
        is_default_ref: bool,
    },
    Ready {
        target: VcsPublishTarget,
        ahead: u32,
        behind: u32,
        is_default_ref: bool,
    },
    Behind {
        target: VcsPublishTarget,
        behind: u32,
        is_default_ref: bool,
    },
    Diverged {
        target: VcsPublishTarget,
        ahead: u32,
        behind: u32,
        is_default_ref: bool,
    },
    Ambiguous {
        candidates: Vec<VcsPublishTarget>,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsPublishTarget {
    pub ref_name: String,
    pub remote: String,
    pub remote_ref: String,
    pub revision: String,
    pub creates_ref: bool,
    pub sets_upstream: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VcsCommitSelection {
    All,
    Files { file_ids: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VcsCommitMessage {
    Generate,
    Provided { value: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VcsAction {
    Commit {
        expected_working_copy: String,
        selection: VcsCommitSelection,
        message: VcsCommitMessage,
    },
    Push {
        expected_publication: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        publish_ref: Option<String>,
        #[serde(default)]
        allow_default_ref: bool,
    },
    CommitAndPush {
        expected_working_copy: String,
        expected_publication: String,
        selection: VcsCommitSelection,
        message: VcsCommitMessage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        publish_ref: Option<String>,
        #[serde(default)]
        allow_default_ref: bool,
    },
}

impl VcsAction {
    pub fn includes_commit(&self) -> bool {
        matches!(self, Self::Commit { .. } | Self::CommitAndPush { .. })
    }

    pub fn includes_push(&self) -> bool {
        matches!(self, Self::Push { .. } | Self::CommitAndPush { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VcsActionPhase {
    GeneratingMessage,
    Committing,
    Pushing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum VcsActionEvent {
    Started {
        action_id: String,
        phases: Vec<VcsActionPhase>,
    },
    PhaseStarted {
        action_id: String,
        phase: VcsActionPhase,
        label: String,
    },
    Finished {
        action_id: String,
        result: VcsActionResult,
    },
    Failed {
        action_id: String,
        phase: Option<VcsActionPhase>,
        completed_commit: Option<VcsCommitResult>,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum VcsActionResult {
    Commit {
        commit: VcsCommitResult,
    },
    Push {
        push: VcsPushResult,
    },
    CommitAndPush {
        commit: VcsCommitResult,
        push: VcsPushResult,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsCommitResult {
    pub revision: String,
    pub subject: String,
    pub remaining_changes: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advanced_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsPushResult {
    pub revision: String,
    pub ref_name: String,
    pub remote: String,
    pub remote_ref: String,
    pub created_ref: bool,
    pub set_upstream: bool,
    pub up_to_date: bool,
}
