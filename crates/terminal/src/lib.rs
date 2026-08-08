//! jolt-terminal — engine-side PTY ownership, replay, and lifecycle.

mod simd_base64;
mod terminals;

pub use terminals::{TerminalOutput, Terminals};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TerminalError(pub String);

impl TerminalError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
