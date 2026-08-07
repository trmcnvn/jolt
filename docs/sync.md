# Synchronization

Jolt uses three synchronization paths with different semantics:

1. a current-state workspace registry for devices, spaces, chats, and live session rows;
2. one Loro document per chat for transcript and durable commands;
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

Lexicographic order is therefore total. A field applies only when its incoming clock is strictly newer.

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

RegistryRoom writes table backups to R2 and exposes authenticated stats/rows/reset routes for operations. Destructive batches trigger an immediate backup. When a chat row becomes tombstoned, the room durably queues retirement of its SessionRoom plus deletion of its chat-scoped R2 attachments; failed cleanup retries by alarm. A reset is self-healing because clients reseed automatically.

## Session documents

### Schema

A session Loro document contains metadata, message entries, and append-only command entries. Text is held in `LoroText`; tool, input, and error parts use typed map fields. Message records larger than 256 KiB are split into continuations and joined during projection.

The Rust client and TypeScript edge use `loro-protocol` 0.3-compatible binary frames over WebSocket. Join sends the client's version vector, the room returns missing history or a snapshot, and both sides acknowledge update batches. Large updates fragment and reassemble at the protocol layer.

### SessionRoom

The per-chat Durable Object keeps:

- a current Loro snapshot;
- a buffered update log;
- a compact transcript catalog, byte-bounded historical pages, and a mutable tail projection;
- the latest working-copy diff manifest and references to immutable, byte-bounded patch pages;
- ephemeral presence;
- daily R2 backups.

Dirty update rows flush on a short cadence. Logical updates larger than Durable Object SQLite's row limit are split across continuation rows and reassembled before replay. When the update log reaches the configured threshold, the room folds it losslessly into the snapshot. Daily checkpoint-based trimming discards history beyond the three-day retention frontier while preserving current state. A joining client behind a shallow snapshot's retained frontier receives the full snapshot instead of an unusable partial diff.

The host engine keeps local snapshots and an LRU of open documents. `WatchTranscriptV2` opens with compact whole-session metadata and enough trailing pages to cover at least 64 messages, then sends sequenced deltas only for the mutable live page. Historical pages are fetched by opaque ID and cached under a device byte budget. The older full-reset watch remains only for client compatibility. Retiring a deleted chat closes its room, prevents stale clients from recreating backups, and removes `backup/<chatId>/latest.loro`.

Checkout diffs use the same projection shape without transcript-style line deltas. `WatchCheckoutDiffV2` is scoped to one chat checkout and sends a complete file/page manifest; expanded bodies load through `GetCheckoutDiffPage`. Pages are self-contained unified-patch fragments split at file, hunk, then line boundaries and addressed by SHA-256. The edge stores chat manifests in SessionRoom and deduplicates page bodies per user checkout in R2, deleting pages dropped by the latest checkout manifest. Files omitted by the bounded capture remain visible in the manifest with an explicit partial state. A device-local review can lease a working-copy catalog before annotating it; the host copies every referenced page into its pinned-diff store and serves that revision until the reviewing device deletes the draft and releases the lease. Pending comments themselves remain solely in the viewing device's `review-drafts.sqlite` and never participate in synchronization.

### Writer discipline

- Authorized clients submit only their own immutable command entries; the edge validates and idempotently appends them to canonical Loro.
- The chat host is the sole writer of transcript entries and command outcomes.
- The issuing composer may cancel only its own still-pending command.
- Full tool inputs remain in the host's local journal; only render-safe projections sync.

### Offline commands

Commands default to a 24-hour expiry. Mobile and remote viewers persist commands in a device-local outbox before submission; an edge acknowledgement means the command is durable in canonical Loro, while the client-minted transcript message ID acknowledges execution. The host evaluates processed-ID dedupe, expiry, and supersession before executing. Newer pending steer/interrupt entries supersede older entries of the same kind. Interrupts aimed at completed turns are also superseded.

The host stores command claims in SQLite before execution. This prioritizes at-most-once side effects after a crash; recovery marks interrupted run state and resumes from durable journal/doc information where supported.

## Device relay

Each online engine hosts one WebSocket in its DeviceRoom. Other engines and iOS clients connect as clients. Frames are:

```text
uleb128(header-length) + JSON header + payload
```

The header carries stream ID, frame kind, and optional destination/source device IDs. The payload is an ordinary RPC text frame or stream item, so relay calls use the same dispatcher as localhost and in-process calls.

DeviceRoom also stores durable chat nudges. A host receives them immediately when connected or on its next host join. The nudge only requests `open(chatId)`; the session document remains the source of truth for pending work.

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

- `/stats/:chatId`
- `/snapshot/:chatId`
- `/append/:chatId`
- `/registry/:orgId/stats`
- `/registry/:orgId/rows`
- `/registry/:orgId/reset`

## Source map

- Registry model and merge: `crates/doc/src/registry.rs`
- Registry client: `crates/sync/src/registry.rs`
- Registry DO: `edge/src/registry-room.ts`, `edge/src/registry-core.ts`
- Session schema and commands: `crates/doc/src/schema.rs`, `crates/doc/src/commands.rs`
- Loro room client: `crates/sync/src/room.rs`
- Session DO: `edge/src/session-room.ts`
- Device relay: `crates/rpc/src/device_room.rs`, `edge/src/device-room.ts`
- Engine document host: `crates/engine/src/doc_host.rs`
