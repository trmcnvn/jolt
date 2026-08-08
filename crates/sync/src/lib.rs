//! jolt-sync — Loro and workspace-registry room clients over WebSocket against the TS edge,
//! plus ephemeral presence and reconnect/liveness handling.
//!
//! - [`RoomClient`]: joins a SessionRoom DO room (`wss://…/session/{chatId}/ws?token=`),
//!   backfills via version-vector diff, pushes local commits, imports remote updates,
//!   reassembles/produces fragments, relays `%EPH` presence, and reconnects with
//!   exponential backoff. Wire format is the official `loro-protocol` crate — byte-identical
//!   to the npm package the edge imports.

pub mod registry;
mod room;

pub use registry::{RegistryClient, RegistryEvent, RegistryTuning};
pub use room::{
    RoomClient, RoomEvent, RoomStatsSnapshot, RoomTuning, StaticUrl, SyncError, UrlProvider,
};
