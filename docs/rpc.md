# RPC

Jolt uses one RPC envelope for desktop-to-engine IPC, in-process UI calls, and device-relayed engine calls.

## Framing

WebSocket transports send one JSON object per text message. Byte transports use newline-delimited JSON records.

Client request:

```json
{"id": 1, "method": "ListModels", "params": {"harness": "pi"}}
```

Client cancellation:

```json
{"id": 1, "cancel": true, "params": null}
```

Unary response:

```json
{"id": 1, "ok": {}}
```

Error response:

```json
{"id": 1, "err": "human-readable failure"}
```

Stream response:

```json
{"id": 1, "item": {}}
{"id": 1, "item": {}}
{"id": 1, "done": true}
```

Bulk client payloads use a versioned binary unary request: `JRPB`, version, opcode, request ID, method/params lengths, UTF-8 method, JSON params, then raw bytes. Server binary streams use the same magic/version with a stream-item opcode and request ID. Limits are 8 MiB per application frame, 1 MiB per binary payload, 64 KiB of binary params, and 256 bytes per method name.

Every transport reaches the same `RpcService::handle(method, params)` or `handle_binary(method, params, bytes)` dispatcher. In-process mode uses the same serialized envelopes and binary codecs.

## Transports

- **In-memory:** bounded text/binary frame channels between embedded desktop UI and engine supervisor.
- **Local IPC:** WebSocket at `ws://127.0.0.1:<JOLT_IPC_PORT>`.
- **Device relay:** virtual sockets tunneled through a device's Durable Object room.

Local IPC binds loopback and is authenticated by the local machine trust boundary rather than a separate IPC credential. Device-relay headers and payloads are capped at 4 KiB and 8 MiB respectively. A client advertises compressed-response support with the optional `z` header field; the host then zlib-compresses JSON responses of at least 16 KiB as `rpc-zlib`. Older clients and edges continue using uncompressed `rpc` frames.

## Device routing

A forwardable method may include:

```json
{"targetDeviceId": "device-id"}
```

If the ID differs from the connected engine, that engine dials the target's DeviceRoom and forwards the method unchanged. Stream replies are proxied item by item. Failed links are invalidated so the next call redials.

The following do not honor `targetDeviceId` forwarding and are normally handled by the directly connected engine:

- Jolt authentication and organization setup;
- `LocalDevice`;
- workspace registry watches/mutations;
- harness secret list/upsert/delete.

The host relay itself explicitly rejects harness-secret methods. Other non-forwardable methods can be called only by a client that opened a direct virtual socket to that host; iOS uses that path for its limited host workspace mutation.

## Method surface

### Harnesses and conversation

| Method | Reply | Remote target | Purpose |
| --- | --- | --- | --- |
| `ListHarnesses` | unary | yes | Static harness descriptors without forcing CLI discovery |
| `WatchHarnessUpdates` | stream | yes | Device-local installed/latest versions and maintenance state |
| `CheckHarnessUpdates` | unary | yes | Trigger an immediate background release check |
| `ApplyHarnessUpdate` | unary | yes | Start a typed, user-approved harness update |
| `ListModels` | unary | yes | Models from one installed harness |
| `ListCommands` | unary | yes | Jolt composer commands for the target session context |
| `QueueCommand` | unary | yes | Append a durable run/queue/bash/steer/interrupt/respond-input command |
| `CancelQueuedPrompt` | unary | no | Cancel a queue item still pending on its issuing device |
| `WatchQueuedPrompts` | stream | no | Pending queued turns from the locally synced chat doc |
| `WatchTranscriptV2` | stream | yes | Compact whole-session manifest + trailing pages, then sequenced live-page deltas |
| `GetTranscriptPage` | unary | yes | Fetch one historical page by opaque page ID |
| `SearchTranscript` | unary | yes | Search one transcript and return message/page anchors |
| `ExtractQuestions` | unary | yes | Extract answerable questions from one completed assistant message |
| `WatchChatUsage` | stream | yes | Current chat usage from its host ledger |
| `UsageBreakdown` | unary | yes | 7/30/90-day local usage summary |

### Workspace and sync

| Method | Reply | Remote target | Purpose |
| --- | --- | --- | --- |
| `WatchChats` | stream | no | Active-chat bootstrap followed by changed-row upsert/remove deltas |
| `QueryChats` | unary | no | Cursor-paged archived rows and server-side title search |
| `WatchDevices` | stream | no | Current device rows |
| `WatchSessions` | stream | no | Live-status bootstrap followed by changed-row upsert/remove deltas |
| `WatchSpaces` | stream | no | Current space rows |
| `WatchThemes` / `ListThemes` | stream / unary | no | Account-registry copies of installation-level custom theme files |
| `UpsertThemes` / `DeleteTheme` | unary | no | Reconcile custom theme files without syncing active appearance settings |
| `Mutate` | unary | no | Create/rename/delete spaces and chats; config, checkout, archive, seen, device updates |
| `RegenerateChatTitle` | unary | yes | Replace a session name from its first prompt using the host harness's economy model |
| `ProbeSync` | unary | no | Ask workspace/open chat clients to verify room liveness |
| `SyncStatus` | unary | no | Per-room push/ack/rejoin/probe/resync diagnostics |
| `LocalDevice` | unary | no | Identity of the directly connected engine |

`Mutate` operations are tagged by `op`. Current operations include `createChat`, `createSpace`, `renameSpace`, `deleteSpace`, `renameChat`, `setChatBranch`, `setChatCwd`, `setChatActivity`, `setChatHost`, `setChatPinned`, `setChatArchived`, `setChatConfig`, `deleteChat`, `renameDevice`, and `markChatSeen`.

### Authentication

| Method | Reply | Purpose |
| --- | --- | --- |
| `AuthStatus` | stream | Signed-out, setup-required, or signed-in state |
| `SignIn` | unary | Start headed browser/loopback sign-in |
| `SignInHeadless` | unary | Start paste-code sign-in |
| `CompleteSignIn` | unary | Submit a paste code |
| `SignOut` | unary | Remove the current saved session |
| `ListOrgs` | unary | Read memberships during automatic setup |
| `EnsurePersonalOrg` | unary | Adopt the sole membership or create `Personal` |

### Local and Account scopes

| Method | Reply | Purpose |
| --- | --- | --- |
| `ScopeStatus` | stream | Active scope, account availability, and pending Local merge state |
| `SwitchScope` | unary | Route the local viewport to Local or Account without stopping either runtime |
| `ResolveAccountLink` | unary | Keep non-empty Local data separate or move it into the signed-in account |

These methods are local IPC only. DeviceRoom relay traffic is permanently routed to Account.

### Repositories and files

| Method | Reply | Remote target |
| --- | --- | --- |
| `ListRepos`, `AddRepo`, `CloneRepo`, `CreateRepo` | unary | yes |
| `ListBranches`, `ListRefs`, `SwitchRef` | unary | yes |
| `GetCheckoutReview` | unary | yes |
| `ListFolders`, `SearchFiles` | unary | yes |
| `CreateWorktree`, `DeleteWorktree` | unary | yes |
| `VcsSettings`, `SetVcsBackend` | unary | yes |
| `WatchCheckoutDiffV2` | stream | yes |
| `GetCheckoutDiffPage` | unary | yes |
| `GetTurnDiffPage` | unary | yes |
| `PinDiffDocument`, `ReleaseDiffDocument` | unary | yes |
| `GetReviewDraft`, `PutReviewDraft`, `DeleteReviewDraft` | unary | no (direct device only) |

`WatchCheckoutDiffV2` is checkout-specific by `chatId`. It opens with an atomic compact manifest; later frames replace only that manifest. Expanded file bodies load as immutable, SHA-256-addressed raw-patch pages through `GetCheckoutDiffPage`. Sequence or catalog mismatch causes a fresh bootstrap. `GetTurnDiffPage` loads an immutable page captured for one assistant transcript entry, addressed by chat, assistant message, catalog revision, and page ID. `PinDiffDocument` durably retains a working-copy revision while a review draft references it; release removes that lease. Review-draft RPCs persist typed, pending annotations only in the directly connected device's SQLite store and reject relay access.

`GetCheckoutReview` resolves a chat checkout on its host and returns the open provider-neutral code review when a supported forge adapter can authenticate. The initial GitHub adapter uses that device's authenticated `gh` CLI. Unsupported forges, unavailable provider tooling, missing authentication, and no matching review all return no review.

File search roots are resolved from synced chat/space rows and verified against the owning repository checkout before walking. Results contain paths only, never file contents.

### Terminals

| Method | Reply | Remote target |
| --- | --- | --- |
| `OpenTerminal` | unary | yes |
| `TerminalSettings`, `SetTerminalCommand` | unary | yes |
| `SubscribeTerminalV2` | binary stream | yes |
| `WriteTerminal`, `ResizeTerminal`, `CloseTerminal` | unary | yes |

`SubscribeTerminalV2` carries versioned binary items with a compact event header, monotonically increasing sequence, and raw PTY bytes without base64. `afterSeq` resumes from the bounded 1 MiB raw-byte replay window.

The launch command is persisted on each engine device. `OpenTerminal` uses that device’s command when the request does not supply an explicit override.

### Agent accounts

| Method | Reply | Remote target |
| --- | --- | --- |
| `ListAgentAccounts` | unary | yes |
| `ActivateAgentAccount`, `ForgetAgentAccount` | unary | yes |
| `StartAgentLogin`, `CompleteAgentLogin` | unary | yes |
| `PollAgentLogin`, `CancelAgentLogin` | unary | yes |

These operate on the selected device's Claude Code or Codex credentials.

### Harness secrets

| Method | Reply | Remote target |
| --- | --- | --- |
| `ListHarnessSecrets` | unary | **no** |
| `UpsertHarnessSecret` | unary | **no** |
| `DeleteHarnessSecret` | unary | **no** |

Responses contain metadata only. Values are accepted over direct local IPC/in-process transport for writes, are rejected by the host relay, and are never returned.

### Attachments and updates

| Method | Reply | Remote target |
| --- | --- | --- |
| `GetTransportCapabilities` | unary | yes |
| `UploadBinaryChunk` | binary unary | yes |
| `UploadChunk` | unary | yes (legacy fallback) |
| `UploadCommit`, `ReadAttachmentChunk` | unary | yes |
| `WatchHarnessUpdates` | stream | yes |
| `CheckHarnessUpdates`, `ApplyHarnessUpdate` | unary | yes |
| `UpdateStatus` | stream | yes |
| `ApplyUpdate` | unary | yes |

Uploads are staged and committed on the chat's host device. Clients probe `GetTransportCapabilities`, send raw binary chunks when supported, and retain `UploadChunk` as a rolling-upgrade fallback for older hosts. Host assembly and edge mirroring stream from disk rather than materializing a whole-file base64 buffer. Attachment reads are path-jailed by the host implementation.

## Durable command payloads

`QueueCommand` accepts `{chatId, command}`. The command is a tagged payload:

- `run {request, messageId}`
- `hiddenPrompt {request}`
- `queue {request, messageId}`
- `resumeQueue`
- `bash {command, excludeFromContext, cwd, messageId}`
- `steer {prompt, messageId?}`
- `interrupt`
- `respondInput {requestId, answers}`
- `goal {operation}` where operation is create, edit, pause, resume, or clear; mutations after create include `goalId` and `expectedRevision`

A `RunRequest` carries prompt, concrete model/reasoning/options, cwd, sandbox, approval choice, optional harness resume ID, and host-staged attachment paths. Queue commands remain pending while a turn is active, drain together in FIFO order at the next clean turn boundary, and pause after interruption or error until `resumeQueue` is issued.

## Stream behavior

A subscription receiver drop sends cancellation to the server. Bounded channels apply backpressure instead of allowing an unbounded slow-consumer queue.

Watch streams generally emit the current value first. `WatchChats` opens with active rows only, then emits changed chat rows and removals; archived history and title matches load through cursor-paged `QueryChats`. `WatchSessions` similarly opens with merged live-status rows and then emits only changed rows and removals. `WatchTranscriptV2` opens atomically with a compact manifest and enough trailing pages to cover at least 64 messages, then emits sequenced deltas for the mutable live page. A sequence/page mismatch resubscribes for another tail-sized bootstrap. Historical pages come from `GetTranscriptPage`; opaque IDs and page revisions make cached pages safe across reconnects. `SearchTranscript` searches the authoritative document on its host and returns page-backed message anchors, so selecting a cold result loads only its containing page.

## Contract guidance

- Treat `params` from a process/network boundary as untrusted JSON.
- Use tagged enums for mutually exclusive payloads.
- Reject unknown methods and behavior-changing enum tags.
- Add a method to the forwardable and stream-method lists explicitly; routing is deny-by-default.
- Never make a secret-bearing method relay-forwardable.

## Source map

- Product contracts and method constants: `crates/api/src/`
- Generic wire envelopes/codecs: `crates/rpc/src/lib.rs`
- Client: `crates/rpc/src/client.rs`
- Server: `crates/rpc/src/server.rs`
- Engine dispatch and routing: `crates/engine/src/rpc.rs`
- Relay transport: `crates/relay/src/lib.rs`
- Shared wire types: `crates/proto/src/`
