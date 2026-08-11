//! Session-document limits and timing constants.
//! These are starting points; re-measure with real heavy sessions.

/// Max bytes for a single message entry before continuation splitting.
pub const MSG_INLINE_MAX: usize = 256 * 1024;
/// Host commits streamed assistant segments into the doc at this cadence (ms).
pub const STREAM_COMMIT_MS: u64 = 120;
/// Byte budget for the in-memory doc LRU on device backends.
pub const DOC_LRU_BYTE_BUDGET: usize = 80 * 1024 * 1024;
/// Terminal output batching cadence (ms).
pub const TERMINAL_OUTPUT_BATCH_MS: u64 = 12;
/// Default TTL for durable commands.
pub const COMMAND_DEFAULT_TTL_MS: i64 = 24 * 60 * 60 * 1000;
