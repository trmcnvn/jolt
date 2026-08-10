# Synchronization

Jolt uses three synchronization paths with different semantics:

1. a current-state workspace registry for devices, spaces, chats, and live session rows;
2. one fenced SessionHub per chat for typed commands and bounded transcript projections;
3. one relay room per engine for live RPC and durable wake-up nudges.

Each path uses persistence and conflict semantics suited to its data.

## Workspace registry

### Topology

```text
RegistryDoc A ─ RegistryClient ─┐
                               ├─ RegistryRoom DO: reg1/<org>/<user>
RegistryDoc B ─ RegistryClient ─┘
```

RegistryRoom is authoritative. Its SQLite table stores current rows and tombstones.

Each client persists:

- authoritative rows last received from the room;
- a monotonic server cursor;
- pending local operation batches;
- its hybrid logical clock state.

Reads overlay pending operations on authoritative rows, so local mutations appear immediately while offline.

### Row and operation shape

```text
Row {
  kind, id, seq, deleted, delHlc?, fields, clocks
}

Op {
  kind, id,
  op: upsert | update | delete,
  set?, hlc, clocks?
}
```

Rows use the `devices`, `spaces`, `chats`, `sessions`, and `themes` kinds. Theme rows carry opaque versioned custom-theme files; each installation keeps its own on-disk copy, while active selections and other appearance settings remain device-local. Revisioned deletion markers prevent stale hosts from restoring removed themes, and irreconcilable edits are retained under separate IDs as named conflict copies. Each field has an independent clock. Setting a field to JSON `null` removes the value while retaining the clocked write.

Hybrid logical clocks are fixed-width strings:

```text
<13-digit epoch-ms>-<6-digit counter>-<device-id>
```

Lexicographic order is therefore total. A field applies only when its incoming clock is strictly newer. One identity exception is enforced identically in Rust, TypeScript, and Swift: a live chat's existing `deviceId` cannot change; recovery uses a fresh chat ID.

- `upsert` creates rows and can revive a tombstone when newer than its delete clock.
- `update` never creates or revives a row.
- `delete` creates or advances a tombstone when causally newer than the row.
- Reapplying an operation is a no-op by strict clock comparison.

### Wire protocol

Client to server:

| Frame | Purpose |
| --- | --- |
| `hello {cursor, device}` | Establish a session and request full or delta state |
| `push {batch, ops}` | Submit one idempotent operation batch |
| `presence {at}` | Publish ephemeral device presence |
| `probe` | Require a protocol-level liveness response |

Server to client:

| Frame | Purpose |
| --- | --- |
| `state {seq, full, rows, gcFloor, presence}` | Initial full table or `seq > cursor` delta |
| `rows {seq, rows}` | Broadcast complete merged rows touched by a batch |
| `ack {batch, seq, applied}` | Retire a pending batch |
| `presence {device, at}` | Broadcast recent presence |
| `probe-ok {seq}` | Prove the DO actor, not only the socket runtime, responded |

### Recovery

- Reconnect re-pushes unacknowledged batches.
- A cursor older than `gcFloor` receives a full state.
- Tombstones are retained for 30 days before daily GC advances `gcFloor`.
- If a client cursor is ahead of a reset room, the client keeps its rows and reseeds the server with their preserved per-field clocks.
- A full resync also reseeds local-only rows.
- Registry snapshots are stored under the identity-scoped `docs.sqlite3`, so offline restarts keep rows and pending writes.

RegistryRoom writes table backups to R2 and exposes authenticated stats/rows/reset routes for operations. Destructive batches trigger an immediate backup. When a chat row becomes tombstoned, the room durably queues retirement of its SessionHub (and legacy SessionRoom during cutover) plus deletion of chat-scoped R2 artifacts; failed cleanup retries by alarm. A reset is self-healing because clients reseed automatically.

## Sessions

The assigned host is the only canonical transcript writer. Normalized messages, parts, incremental text chunks, page metadata, typed commands, and synchronization markers live in its identity-scoped `docs.sqlite3`. The host assignment is immutable because cwd, checkout, and harness state are machine-local; permanent loss creates a recovery-fork chat rather than silently changing writers.

SessionHub is a wasm-free Durable Object with:

- an immutable host device and monotonically increasing writer lease;
- idempotent typed command current state and server ordering;
- one compact transcript manifest and bounded live-page base;
- a bounded sequence of live-page deltas;
- daily JSON backup metadata.

Sealed transcript pages are immutable SHA-256-addressed JSON in R2. Viewers receive a base sequence followed by contiguous nested delta frames; any gap or delta tripwire failure reconnects for a new base. The host republishes a full base after reconnect and whenever the delta budget reaches 200 rows or 512 KiB.

### Writer and command discipline

- Authorized viewers submit immutable typed commands by client-minted ID. Same ID/same canonical envelope is idempotent; same ID/different content is `409`.
- Command delivery is `pending -> claimed -> terminal`; only the current host lease may claim and resolve.
- The issuing composer may cancel only its own still-pending command.
- The host claims remote commands before its local mark-before-execute ledger, then evaluates expiry and supersession.
- Full tool inputs remain in the private host run journal; only render-safe transcript projections publish.

Commands default to a 24-hour expiry. Mobile viewers persist them in an outbox before submission. Host-local commands persist and execute while offline, then submit and reconcile the same ID after reconnect. A command left pending after a crash but already present in the processed ledger is terminally rejected with an explicit unknown outcome rather than re-executed or left stuck.

Checkout diffs retain the same bounded manifest/page shape. SessionHub stores only the latest diff manifest and sequence; immutable hash-verified patch pages remain deduplicated in R2 by checkout. Pending review comments remain solely in the viewing device's `review-drafts.sqlite`.

The exact SQLite DDL, HTTP/WebSocket frames, R2 keys, importer, rollback process, and cutover gates are specified in [SessionHub session architecture](session-hub.md).

## Device relay

Each online engine hosts one WebSocket in its DeviceRoom. Other engines and iOS clients connect as clients. Frames are:

```text
uleb128(header-length) + JSON header + payload
```

The header carries stream ID, frame kind, and optional destination/source device IDs. The payload is an ordinary RPC text frame or stream item, so relay calls use the same dispatcher as localhost and in-process calls.

DeviceRoom also stores durable chat nudges. A host receives them immediately when connected or on its next host join. The nudge only requests `open(chatId)`; SessionHub plus host SQLite remain authoritative for pending work.

## Presence and liveness

Transport ping/pong proves only that a WebSocket runtime is alive. Jolt additionally uses protocol-level probes and deadlines. A room that remains quiet beyond its probe threshold is torn down and redialed when it fails to answer.

Workspace presence uses periodic device beats and a freshness window. Session Working state is separately freshness-gated from the synced status timestamp. Foreground/focus events ask open clients to verify liveness immediately when they have been protocol-quiet long enough.

## Diagnostics

Run:

```bash
jolt sync
```

Inspect `connected`, push/ack ages, rejoins, probes, full resyncs, disconnects, and rejected writes before changing data or restarting services.

Relevant edge endpoints are authenticated and intended for diagnostics/repair:

- `/hub/:chatId/stats`
- `/hub/:chatId/bootstrap`
- `/registry/:orgId/stats`
- `/registry/:orgId/rows`
- `/registry/:orgId/reset`

## Source map

- Registry model and merge: `crates/registry-model/src/model.rs`
- Registry client: `crates/sync/src/registry.rs`
- Registry DO: `edge/src/registry-room.ts`, `edge/src/registry-core.ts`
- Canonical session store: `crates/store/src/sessions.rs`
- Semantic commands/projections: `crates/session-doc/src/commands.rs`, `transcript_page.rs`
- SessionHub client: `crates/sync/src/hub.rs`
- SessionHub DO: `edge/src/session-hub.ts`
- Device relay: `crates/relay/src/lib.rs`, `edge/src/device-room.ts`
- Engine document host: `crates/engine/src/doc_host.rs`
