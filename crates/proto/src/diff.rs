use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::VcsKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffCompleteness {
    Complete,
    Binary,
    SnapshotTruncated,
    OversizedLine,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffPageDescriptor {
    pub id: String,
    pub file_id: String,
    pub first_row: usize,
    pub row_count: usize,
    pub notice_count: usize,
    pub hunk_count: usize,
    pub line_count: usize,
    pub estimated_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffFileDescriptor {
    pub id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    #[serde(default)]
    pub binary: bool,
    pub row_count: usize,
    pub estimated_bytes: usize,
    pub completeness: DiffCompleteness,
    #[serde(default)]
    pub page_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDiffManifest {
    pub catalog_revision: String,
    pub checkout_id: String,
    pub device_id: String,
    pub cwd: String,
    #[serde(default)]
    pub vcs: VcsKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub files: Vec<DiffFileDescriptor>,
    pub pages: Vec<DiffPageDescriptor>,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDiffPage {
    pub id: String,
    pub catalog_revision: String,
    pub file_id: String,
    pub patch: String,
}

/// Immutable filesystem changes attributed to one assistant transcript entry.
///
/// The manifest is small enough to travel with transcript metadata. Patch
/// bodies remain in content-addressed [`CheckoutDiffPage`] payloads and are
/// fetched only when the desktop diff viewer opens them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnDiffManifest {
    pub catalog_revision: String,
    pub chat_id: String,
    pub assistant_message_id: String,
    pub device_id: String,
    pub cwd: String,
    #[serde(default)]
    pub vcs: VcsKind,
    pub files: Vec<DiffFileDescriptor>,
    pub pages: Vec<DiffPageDescriptor>,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDiffBootstrap {
    pub sequence: u64,
    pub manifest: CheckoutDiffManifest,
    #[serde(default)]
    pub pages: Vec<CheckoutDiffPage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CheckoutDiffWatchFrame {
    Bootstrap {
        bootstrap: CheckoutDiffBootstrap,
    },
    Manifest {
        sequence: u64,
        manifest: CheckoutDiffManifest,
    },
}
