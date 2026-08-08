import os

/// Sync must never fail silently. Visible in Console.app and `log stream`
/// under the Jolt iOS sync category.
let roomLog = Logger(subsystem: "dev.trmcnvn.jolt.ios", category: "sync")
