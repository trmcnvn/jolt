//! Device-local review drafts and typed anchors for reviewable surfaces.
//!
//! Drafts are never part of a session document. They stay on the reviewing
//! device until their formatted feedback is submitted as an ordinary message.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::CheckoutDiffManifest;

pub const REVIEW_DRAFT_SCHEMA_VERSION: u32 = 1;

/// One open code review associated with a concrete checkout. Forge-specific
/// adapters populate this provider-neutral shape (GitHub PRs, GitLab MRs, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutReview {
    /// Stable adapter id such as `github`; clients must tolerate new values.
    pub forge: String,
    /// Forge-local PR/MR number (GitLab's project-scoped IID).
    pub number: u64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDraft {
    pub schema_version: u32,
    pub review_id: String,
    pub review_key: String,
    pub destination_chat_id: String,
    pub snapshot_id: String,
    pub target: ReviewDraftTarget,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ReviewDraftTarget {
    Diff {
        subject: DiffReviewSubject,
        snapshot: DiffReviewSnapshot,
        comments: Vec<DiffReviewComment>,
    },
    AssistantMessage {
        subject: AssistantMessageReviewSubject,
        snapshot: AssistantMessageReviewSnapshot,
        comments: Vec<MessageReviewComment>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DiffReviewSubject {
    WorkingCopy {
        chat_id: String,
    },
    AssistantTurn {
        chat_id: String,
        assistant_message_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffReviewSnapshot {
    pub manifest: CheckoutDiffManifest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffReviewComment {
    pub id: String,
    pub anchor: DiffReviewAnchor,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffReviewAnchor {
    pub file_id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_lines: Option<InclusiveLineRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_lines: Option<InclusiveLineRange>,
    pub excerpt: Vec<DiffReviewExcerptLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InclusiveLineRange {
    pub start: u32,
    pub end: u32,
}

impl InclusiveLineRange {
    pub fn containing(numbers: impl IntoIterator<Item = u32>) -> Option<Self> {
        let mut numbers = numbers.into_iter();
        let first = numbers.next()?;
        let (start, end) = numbers.fold((first, first), |(start, end), number| {
            (start.min(number), end.max(number))
        });
        Some(Self { start, end })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffReviewExcerptLine {
    pub kind: DiffReviewLineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_number: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffReviewLineKind {
    Context,
    Addition,
    Deletion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessageReviewSubject {
    pub chat_id: String,
    pub root_message_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessageReviewSnapshot {
    pub revision: String,
    pub text_parts: Vec<AssistantMessageTextPart>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessageTextPart {
    pub part_id: String,
    pub markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageReviewComment {
    pub id: String,
    pub anchor: MessageReviewAnchor,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageReviewAnchor {
    pub part_id: String,
    pub start_byte: u32,
    pub end_byte: u32,
    pub exact: String,
    pub prefix: String,
    pub suffix: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusive_range_contains_unsorted_numbers() {
        assert_eq!(
            InclusiveLineRange::containing([8, 4, 6]),
            Some(InclusiveLineRange { start: 4, end: 8 })
        );
        assert_eq!(InclusiveLineRange::containing([]), None);
    }
}
