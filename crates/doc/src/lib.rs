//! jolt-doc — session document schemas and the workspace registry model.
//!
//! The schema shape (container names, part maps with LoroText bodies, command entries)
//! is shared with the edge tail materializer and TypeScript peers.
//!
//! Load-bearing invariant: message parts are a
//! LoroList of part maps whose text bodies live in **LoroText** — streaming appends RLE-merge at
//! ~1.03x oplog overhead, whereas rewriting whole part values costs ~125x.

pub mod commands;
pub mod constants;
pub mod parts;
pub mod registry;
pub mod schema;
pub mod transcript_delta;
pub mod transcript_page;

pub use commands::*;
pub use constants::*;
pub use parts::*;
pub use registry::*;
pub use schema::*;
pub use transcript_delta::*;
pub use transcript_page::*;
