//! Device-local harness secret metadata.
//!
//! No response type carries secret values. Creation sends a value only over
//! local IPC; persisted values remain in the host device's native credential
//! store and are injected only into selected harness child processes.

use serde::{Deserialize, Serialize};

use crate::HarnessId;

/// Non-secret metadata for one environment variable exposed to harnesses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSecret {
    pub id: String,
    pub label: String,
    pub environment_variable: String,
    pub harnesses: Vec<HarnessId>,
}

/// Device-local secure-storage state returned to Settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSecretsSnapshot {
    pub secrets: Vec<HarnessSecret>,
    pub storage_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_error: Option<String>,
}
