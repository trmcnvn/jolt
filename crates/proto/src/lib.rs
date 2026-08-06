//! jolt-proto — wire types shared by engine, UI, and RPC.
//!
//! Token usage stays out of synced conversation documents; host engines expose
//! device-local summaries over RPC instead.

pub mod agent;
pub mod diff;
pub mod entities;
pub mod motion;
pub mod secrets;
pub mod usage;
pub mod view;

pub use agent::*;
pub use diff::*;
pub use entities::*;
pub use secrets::*;
pub use usage::*;
