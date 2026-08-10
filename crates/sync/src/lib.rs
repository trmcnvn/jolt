//! Jolt edge synchronization clients.
//!
//! [`SessionHubClient`] is the active host command/projection protocol and
//! [`RegistryClient`] synchronizes current workspace rows. [`RoomClient`] is
//! retained only for rollback/import verification until SessionRoom removal is approved.

mod hub;
pub mod registry;
mod room;

pub use hub::{
    HubCommand, HubDeliveryState, PublishResult, SessionHubClient, SessionHubEvent, SessionHubStats,
};
pub use registry::{RegistryClient, RegistryEvent, RegistryTuning};
pub use room::{
    RoomClient, RoomEvent, RoomStatsSnapshot, RoomTuning, StaticUrl, SyncError, UrlProvider,
};
