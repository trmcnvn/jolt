//! Terminal panel (feature-inventory §1.10): a `libghostty-vt`-backed
//! emulator fed by the engine's PTY stream over the generic RPC client.
//!
//! - [`emulator`] — pure Ghostty VT state machine (bytes in, grid out);
//! - [`view`] — cell palette, keystroke→bytes encoding, input coalescing, and
//!   the custom grid-painting element;
//! - [`panel`] — session-scoped tabs, subscriptions with reconnect backoff,
//!   drag-reorder, and the Cmd/Ctrl+` toggle action.
//!
//! Method names come from `jolt_rpc::methods` and wire types from
//! `jolt_proto` (`TerminalSession`, `TerminalEvent`) — the same contract the
//! engine serves (feature-inventory §2.1).

pub mod emulator;
pub mod panel;
pub mod view;
