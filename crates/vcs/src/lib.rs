//! jolt-vcs — device-local repositories, VCS commands, refs, workspaces, and review lookup.

mod forge;
mod managed;
mod repos;
mod vcs;

pub use forge::detect as detect_review;
pub use managed::{WorkspaceCleanupReport, WorkspaceReference};
pub use repos::{CheckoutIdentity, Repos, hex, home_dir, worktree_branch_from_title};
pub use vcs::{Vcs, VcsCommand, compose_command_path};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct VcsError(pub String);

impl VcsError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl From<std::io::Error> for VcsError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}
