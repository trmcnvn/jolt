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

Every transport reaches the same `RpcService::handle(method, params)` dispatcher. In-process mode also serializes and deserializes the RPC envelopes.

## Transports

- **In-memory:** bounded string channels between embedded desktop UI and engine supervisor.
- **Local IPC:** WebSocket at `ws://127.0.0.1:<JOLT_IPC_PORT>`.
- **Device relay:** virtual sockets tunneled through a device's Durable Object room.

Local IPC binds loopback and is authenticated by the local machine trust boundary rather than a separate IPC credential.

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
| `ListModels` | unary | yes | Models from one installed harness |
| `ListCommands` | unary | yes | Jolt composer commands for the target session context |
| `QueueCommand` | unary | yes | Append a durable run/queue/bash/steer/interrupt/respond-input command |
| `CancelQueuedPrompt` | unary | no | Cancel a queue item still pending on its issuing device |
| `WatchQueuedPrompts` | stream | no | Pending queued turns from the locally synced chat doc |
| `WatchTranscriptV2` | stream | yes | Compact whole-session manifest + trailing pages, then sequenced live-page deltas |
| `GetTranscriptPage` | unary | yes | Fetch one historical page by opaque page ID |
| `WatchDocMessages` | stream | yes | Compatibility stream for older clients: initial full reset, then entry/text deltas |
| `ExtractQuestions` | unary | yes | Extract answerable questions from one completed assistant message |
| `WatchChatUsage` | stream | yes | Current chat usage from its host ledger |
| `UsageBreakdown` | unary | yes | 7/30/90-day local usage summary |

### Workspace and sync

| Method | Reply | Remote target | Purpose |
| --- | --- | --- | --- |
| `WatchChats` | stream | no | Current chat rows |
| `WatchDevices` | stream | no | Current device rows |
| `WatchSessions` | stream | no | Local + registry live session rows |
| `WatchSpaces` | stream | no | Current space rows |
| `WatchThemes` / `ListThemes` | stream / unary | no | Account-registry copies of installation-level custom theme files |
| `UpsertThemes` / `DeleteTheme` | unary | no | Reconcile custom theme files without syncing active appearance settings |
| `Mutate` | unary | no | Create/rename/delete spaces and chats; config, checkout, archive, seen, device updates |
| `ProbeSync` | unary | no | Ask workspace/open chat clients to verify room liveness |
| `SyncStatus` | unary | no | Per-room push/ack/rejoin/probe/resync diagnostics |
| `LocalDevice` | unary | no | Identity of the directly connected engine |

`Mutate` operations are tagged by `op`. Current operations include `createChat`, `createSpace`, `renameSpace`, `deleteSpace`, `renameChat`, `setChatBranch`, `setChatCwd`, `setChatActivity`, `setChatHost`, `setChatArchived`, `setChatConfig`, `deleteChat`, `renameDevice`, and `markChatSeen`.

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
| `ListFolders`, `SearchFiles` | unary | yes |
| `CreateWorktree`, `DeleteWorktree` | unary | yes |
| `VcsSettings`, `SetVcsBackend` | unary | yes |
| `WatchCheckoutDiffV2` | stream | yes |
| `GetCheckoutDiffPage` | unary | yes |
| `GetTurnDiffPage` | unary | yes |

`WatchCheckoutDiffV2` is checkout-specific by `chatId`. It opens with an atomic compact manifest; later frames replace only that manifest. Expanded file bodies load as immutable, SHA-256-addressed raw-patch pages through `GetCheckoutDiffPage`. Sequence or catalog mismatch causes a fresh bootstrap. `GetTurnDiffPage` loads an immutable page captured for one assistant transcript entry, addressed by chat, assistant message, catalog revision, and page ID.

File search roots are resolved from synced chat/space rows and verified against the owning repository checkout before walking. Results contain paths only, never file contents.

### Terminals

| Method | Reply | Remote target |
| --- | --- | --- |
| `OpenTerminal` | unary | yes |
| `SubscribeTerminal` | stream | yes |
| `WriteTerminal`, `ResizeTerminal`, `CloseTerminal` | unary | yes |

Terminal output is base64 raw PTY data with monotonically increasing sequence numbers. `afterSeq` resumes from the bounded replay window.

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
| `UploadChunk`, `UploadCommit` | unary | yes |
| `ReadAttachmentChunk` | unary | yes |
| `UpdateStatus` | stream | yes |
| `ApplyUpdate` | unary | yes |

Uploads are staged and committed on the chat's host device. Attachment reads are path-jailed by the host implementation.

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

Watch streams generally emit the current value first. `WatchTranscriptV2` opens atomically with a compact manifest and enough trailing pages to cover at least 64 messages, then emits sequenced deltas for the mutable live page. A sequence/page mismatch resubscribes for another tail-sized bootstrap. Historical pages come from `GetTranscriptPage`; opaque IDs and page revisions make cached pages safe across reconnects.

`WatchDocMessages` remains as a compatibility surface for older clients. It emits a full reset and then compact transcript delta frames.

## Compatibility guidance

- Treat `params` from a process/network boundary as untrusted JSON.
- Use tagged enums for mutually exclusive payloads.
- Default additive fields where mixed-version devices need compatibility.
- Ignore unknown fields where the decoder policy permits it, but reject unknown methods.
- Add a method to the forwardable and stream-method lists explicitly; routing is deny-by-default.
- Never make a secret-bearing method relay-forwardable.

## Source map

- Contract and method constants: `crates/rpc/src/lib.rs`
- Client: `crates/rpc/src/client.rs`
- Server: `crates/rpc/src/server.rs`
- Engine dispatch and routing: `crates/engine/src/rpc.rs`
- Relay transport: `crates/rpc/src/device_room.rs`
- Shared wire types: `crates/proto/src/`
