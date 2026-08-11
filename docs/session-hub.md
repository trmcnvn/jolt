# SessionHub session architecture

SessionHub uses a fenced single-writer model. The assigned engine host owns canonical transcript state in SQLite. Other devices submit typed commands and read bounded transcript projections. The private run journal remains separate and is never published.

## Invariants

1. A chat has one immutable host device. A different device cannot acquire its writer lease; permanent host loss creates a new recovery-fork chat ID.
2. Only the assigned host writes transcript rows, projection bases/deltas, and command outcomes.
3. Commands are immutable by ID. Repeating the same canonical command is idempotent; reusing an ID with different content is a conflict.
4. Command delivery is `pending -> claimed -> terminal`. Terminal statuses are `applied`, `rejected`, `expired`, `superseded`, or `cancelled`.
5. The host claims remote commands before its local mark-before-execute ledger. A crash after that local mark never re-executes the command; a stranded row is terminally rejected with an explicit unknown outcome.
6. Host-local commands persist and execute from SQLite while offline. Reconnection submits the original command ID and reconciles its terminal result to SessionHub.
7. Transcript projection sequence is monotonic within one SessionHub. A viewer applies only `sequence == previous + 1`; otherwise it reconnects for a base plus retained deltas.
8. Sealed page bodies are immutable and SHA-256 addressed. SessionHub stores only the manifest, one bounded live-page base, and bounded deltas.
9. Divergent same-ID histories are never interleaved. Local-to-Account migration creates a deterministic `local-conflict-*` chat.

## Host SQLite

`{identity_dir}/docs.sqlite3` is the canonical host store. SQLite uses WAL, `synchronous=NORMAL`, foreign keys, and STRICT tables. Migrations `session-current-state-v1` and `session-projection-cache-v2` create:

```sql
CREATE TABLE session_chats (
    chat_id TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    next_message_ordinal INTEGER NOT NULL,
    next_command_ordinal INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE session_pages (
    chat_id TEXT NOT NULL,
    page_id TEXT NOT NULL,
    page_ordinal INTEGER NOT NULL,
    first_message_ordinal INTEGER NOT NULL,
    message_count INTEGER NOT NULL,
    estimated_bytes INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    sealed INTEGER NOT NULL,
    published_hash TEXT,
    content_hash TEXT,
    page_revision TEXT,
    PRIMARY KEY (chat_id, page_id),
    UNIQUE (chat_id, page_ordinal),
    FOREIGN KEY (chat_id) REFERENCES session_chats(chat_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE session_messages (
    chat_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    page_id TEXT NOT NULL,
    role TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    device_id TEXT NOT NULL,
    status TEXT,
    revision INTEGER NOT NULL,
    estimated_bytes INTEGER NOT NULL,
    PRIMARY KEY (chat_id, message_id),
    UNIQUE (chat_id, ordinal),
    FOREIGN KEY (chat_id, page_id) REFERENCES session_pages(chat_id, page_id)
) STRICT;

CREATE TABLE session_parts (
    chat_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    part_id TEXT NOT NULL,
    part_ordinal INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT,
    text_base TEXT,
    revision INTEGER NOT NULL,
    PRIMARY KEY (chat_id, message_id, part_id),
    UNIQUE (chat_id, message_id, part_ordinal),
    FOREIGN KEY (chat_id, message_id)
        REFERENCES session_messages(chat_id, message_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE session_text_chunks (
    chat_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    part_id TEXT NOT NULL,
    chunk_ordinal INTEGER NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (chat_id, message_id, part_id, chunk_ordinal),
    FOREIGN KEY (chat_id, message_id, part_id)
        REFERENCES session_parts(chat_id, message_id, part_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE session_turns (
    chat_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    page_id TEXT NOT NULL,
    prompt_preview TEXT NOT NULL,
    reply_message_id TEXT,
    reply_preview TEXT,
    PRIMARY KEY (chat_id, message_id),
    UNIQUE (chat_id, ordinal),
    FOREIGN KEY (chat_id) REFERENCES session_chats(chat_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE session_commands (
    chat_id TEXT NOT NULL,
    command_id TEXT NOT NULL,
    command_ordinal INTEGER NOT NULL,
    edge_seq INTEGER,
    payload_json TEXT NOT NULL,
    issued_by TEXT NOT NULL,
    issued_at INTEGER NOT NULL,
    based_on_json TEXT,
    expires_at INTEGER,
    status TEXT NOT NULL,
    resolution TEXT,
    delivery_state TEXT NOT NULL,
    claim_token TEXT,
    revision INTEGER NOT NULL,
    PRIMARY KEY (chat_id, command_id),
    UNIQUE (chat_id, command_ordinal),
    FOREIGN KEY (chat_id) REFERENCES session_chats(chat_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE session_sync (
    chat_id TEXT PRIMARY KEY,
    protocol_generation INTEGER NOT NULL,
    command_cursor INTEGER NOT NULL,
    projection_revision INTEGER NOT NULL,
    projection_change_revision INTEGER NOT NULL,
    last_published_local_revision INTEGER NOT NULL,
    projection_dirty INTEGER NOT NULL,
    FOREIGN KEY (chat_id) REFERENCES session_chats(chat_id) ON DELETE CASCADE
) STRICT;

```

Indexes cover page message order and pending command order. Streaming text appends into `session_text_chunks`; 64 chunks or 64 KiB fold atomically into `text_base`. Page assignment is stable: 32 logical messages or 384 KiB seals the current page, and the first message ID anchors the next page ID. Page hashes/revisions and turn previews are cached and invalidated transactionally, so a streaming tick reads only the bounded live page. Any sealed-page edit clears `published_hash`.

`protocol_generation = 2` means a full base was accepted by SessionHub. `projection_change_revision` advances only for transcript projection mutations (not command-ledger changes); an acknowledgement clears `projection_dirty` only if it covers that revision. On startup, the engine opens and seeds every locally hosted registry chat that has not reached generation 2, then releases inactive handles.

## SessionHub Durable Object

One `hub1/{chatId}` Durable Object contains only SQLite/current-state metadata:

- `meta`: owner, chat ID, immutable host device, writer lease, command/projection counters, backup state;
- `commands`: canonical envelope, server order, claim token, delivery state, outcome;
- `projection_deltas`: bounded deltas since the current base;
- `publications`: the latest 1,000 idempotency keys;
- small JSON blobs for `projection-manifest` and `projection-live-base`.

It imports no CRDT and loads no WebAssembly.

### HTTP

All routes are authenticated and scoped by user:

| Route | Method | Purpose |
|---|---:|---|
| `/hub/{chatId}/command` | POST | Submit an idempotent typed command |
| `/hub/{chatId}/commands?after={revision}` | GET | Page command current-state changes for reconnect reconciliation |
| `/hub/{chatId}/command/cancel` | POST | Issuing device cancels a still-pending command |
| `/hub/{chatId}/bootstrap` | GET | Manifest, live base, and retained sequenced deltas |
| `/hub/{chatId}/pages/{sha256}` | PUT/GET/HEAD | Verify/upload/read an immutable R2 page |
| `/hub/{chatId}/stats` | GET | Lease, sockets, projection budget, command lag, backup state |
| `/hub/{chatId}/retire` | POST | Close sockets and remove chat-scoped DO/R2 state |

Mobile compatibility aliases are `/command/{chatId}`, `/transcript/{chatId}/ws`, and `/transcript/{chatId}/page?id=...`.

### Permanent-host-loss recovery

Recovery never changes the source chat's host. The relay-forwardable engine RPC
`CreateRecoveryFork { sourceChatId, chatId, spaceId, targetDeviceId }` must target
the engine that owns `spaceId`, and `chatId` must be fresh. That engine downloads
the source Hub bootstrap and every SHA-256-addressed sealed page, verifies page
hashes, ranges, counts, IDs, and delta sequence, then imports the projected
messages into its local SQLite. Any abandoned streaming message becomes aborted.
The fork adds a visible provenance marker, starts with no commands, goals, native
harness continuation, or checkout state, and receives the target space's cwd and
immutable host. The source chat remains untouched.
Recovery must precede removal of the lost device: device deletion retires its
source Hubs and published projections.

A command envelope is:

```json
{
  "id": "uuid",
  "kind": "run",
  "payload": { "kind": "run", "request": {}, "messageId": "uuid" },
  "issuedBy": "device-id",
  "issuedAt": 0,
  "expiresAt": 0,
  "basedOn": { "turnId": "message-id", "frontier": null }
}
```

The edge validates every command variant before persistence. Submission returns `200` for a new or byte-semantically identical ID and `409` for an ID/content conflict. The host persists its command revision cursor only after applying a `/commands` page, so terminal outcomes and claims reconcile after any crash without sending the entire mailbox on every reconnect.

### Host WebSocket

Connect to `/hub/{chatId}/ws?role=host&device={deviceId}`. First assignment persists the device. A different device gets `409`; each accepted reconnect increments `lease` and closes the previous host socket.

Server opening frame:

```json
{
  "type": "hostState",
  "lease": 7,
  "projectionSequence": 10,
  "commandRevision": 12,
  "commands": []
}
```

Host requests carry `requestId` and the current `lease`:

- `publishBase { publishId, manifest, livePage? }`
- `publishDelta { publishId, pageId, basePageRevision, pageRevision, frame }`
- `claimCommand { commandId }`
- `resolveCommand { commandId, claimToken, status, resolution? }`

A stale lease is rejected. A delta whose page ID or `basePageRevision` does not match the accepted live state is not stored and returns `needBase`; this prevents reconnect races from poisoning the retained chain. Responses echo `requestId`; publication responses include sequence, duplicate, and `needBase`. SessionHub also requests a new base at 200 deltas or 512 KiB. The Rust client reconnects with exponential backoff and republishes a current full base after reconnect.

### Viewer WebSocket

Connect to `/hub/{chatId}/ws?role=viewer`. Opening delivery is:

1. `bootstrap` containing the base sequence, manifest, and bounded live page;
2. zero or more nested `delta` frames in sequence order.

```json
{
  "type": "delta",
  "sequence": 11,
  "delta": {
    "pageId": "message-id",
    "pageRevision": "revision",
    "frame": {
      "upsert": [],
      "append": [{ "entry": "id", "part": "id", "text": "...", "len": 10 }],
      "remove": [],
      "count": 1
    }
  }
}
```

Text append `len` is UTF-8 byte length. A count, anchor, part, byte-length, or sequence mismatch forces a reconnect instead of applying uncertain state.

## R2 and backups

Sealed page JSON is hashed before upload and stored at:

```text
transcript/{userId}/{chatId}/{sha256}
```

The Worker recomputes SHA-256 before accepting a PUT. The host records the accepted hash in `session_pages.published_hash`, avoiding repeat uploads after restart. The live page is bounded and remains in the DO because it changes during streaming.

A dirty SessionHub writes a daily recovery object to:

```text
hub-backup/{userId}/{chatId}/latest.json
```

It includes manifest/live base, retained deltas, command state, host assignment, and counters. Retirement deletes the backup and chat-scoped transcript pages.

## Local-to-Account moves

Equal histories retain one chat. Divergent same-ID histories copy Local state to a deterministic conflict chat and preserve both histories; no automatic transcript interleaving occurs.

## Source map

- Canonical SQLite storage: `crates/store/src/sessions.rs`
- Host integration: `crates/engine/src/doc_host.rs`
- Host protocol client: `crates/sync/src/hub.rs`
- Durable Object: `edge/src/session-hub.ts`
- Routes: `edge/src/index.ts`
- iOS projection/outbox: `apps/ios/Jolt/Sync/TranscriptProjectionClient.swift`, `SessionStore.swift`
- Private recovery journal: `crates/engine/src/run_journal.rs`
