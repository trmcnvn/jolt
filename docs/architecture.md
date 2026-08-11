# Architecture

Jolt is a multi-device ADE for local coding-agent CLIs. The engine is the authority for machine-local capabilities; viewports render and control engines through one RPC contract. Cloudflare edge services synchronize durable shared state and relay device-to-device calls.

## Topology

```text
                         Cloudflare edge
                 ┌─────────────────────────────┐
                 │ Worker: auth and routing    │
                 │ RegistryRoom: workspace rows│
                 │ SessionHub: commands + views│
                 │ DeviceRoom: engine relay    │
                 │ R2: pages, attachments,     │
                 │     backups, and releases   │
                 └───────────┬─────────────────┘
                             │ TLS / WebSocket
              ┌──────────────┴──────────────┐
              │                             │
     ┌────────▼────────┐           ┌────────▼────────┐
     │ Engine: device A│           │ Engine: device B│
     │ agents/files/PTY│           │ agents/files/PTY│
     └────────┬────────┘           └────────┬────────┘
              │ typed RPC                   │ typed RPC
       ┌──────▼──────┐               ┌──────▼──────┐
       │ desktop gpui│               │ desktop gpui│
       └─────────────┘               └─────────────┘

             iOS joins RegistryRoom and SessionHub directly,
             and dials engines through DeviceRoom for live RPC.
```

## Processes

### Engine

The Rust engine:

- launches Claude Code, Codex, Pi, and test harness subprocesses;
- hosts an authenticated loopback MCP endpoint injected into supported live harness processes;
- owns local authentication, repositories, version-control commands, worktrees/workspaces, terminals, uploads, diffs, and usage;
- owns canonical SQLite sessions and executes commands only for chats assigned to its device;
- persists normalized transcript rows, command claims, run journals, settings, and identity-scoped telemetry;
- exposes the RPC service on localhost and through its device relay room.

A data directory has one engine owner, enforced by an OS-level lock.

### Desktop viewport

The gpui desktop app is a viewport over the RPC service. On startup it probes the configured localhost port:

- a responding engine becomes a remote local backend over WebSocket;
- otherwise the app creates an `EngineSupervisor`, uses the same JSON envelopes over an in-memory channel, and best-effort serves that engine on the localhost port.

The supervisor resolves the preferred scope from local configuration and the saved session behind the splash; authentication construction never probes Edge. It always owns a fully offline Local runtime and, while authenticated, a separate Account runtime. The scope stream withholds its initial frame until that route is ready, and runtime changes carry a generation plus a transition gate so Local content cannot paint briefly before Account. Switching the viewport to Local leaves Account runs, synchronization, and relay hosting alive in the background. Only Account is exposed through the device relay.

On macOS and Linux, the Devices setting can install the same headless engine as a per-user launchd/systemd service. A signed-out service starts Local without a network dependency. Changing the setting gracefully shuts down the current engine, waits for IPC and data-directory ownership to release, applies the service configuration, and relaunches the viewport so one process always owns the data directory.

### iOS viewport

The SwiftUI app maintains a local workspace-registry replica and a byte-bounded transcript page cache. It consumes SessionHub manifest/base/delta streams, submits typed commands through a durable device-local outbox, and uses relay RPC when an engine must touch a filesystem or CLI. It never retains canonical host session state.

### Edge

`edge/` is a TypeScript Cloudflare Worker with three Durable Object classes:

- **RegistryRoom:** current-state workspace rows and per-field last-write-wins merge.
- **SessionHub:** per-chat immutable host assignment, fenced command mailbox, bounded transcript projections, and backups; it has no CRDT or WASM runtime.
- **DeviceRoom:** one host socket per engine, client byte relay, durable nudges, and small latest-value sidecars.

The Worker verifies WorkOS JWTs or development bearers before stamping identity into Durable Object requests. It also performs WorkOS code exchange/refresh and serves content-addressed attachments and signed release metadata.

## Data model

### Workspace registry

The sidebar and session index use a replicated table with four row kinds:

- `devices`
- `spaces`
- `chats`
- `sessions`

A per-user RegistryRoom is authoritative for current rows. Clients retain an authoritative snapshot plus pending local operations, which are overlaid for optimistic reads and replayed after reconnect. Per-field hybrid logical clocks provide deterministic last-write-wins behavior.

This data is small scalar index state. Transcript content never enters the registry. Chat tombstones also enter a durable artifact-purge queue: the registry retires the chat's SessionHub and deletes its R2 backup and chat-scoped attachment prefix.

### Sessions

Every chat has normalized canonical state in its assigned host's `docs.sqlite3`: messages, typed parts, incremental text chunks, stable pages, commands, and publication metadata. The single transcript writer makes CRDT convergence unnecessary. Text chunks fold transactionally, while `textReveal` markers preserve the same semantic rendering boundaries. Synced tool projections still omit sensitive or bulky inputs not needed for rendering.

SessionHub is a command and projection plane, not another canonical document. It stores typed command current state, one manifest/live base, and small sequenced deltas. Sealed historical page JSON is SHA-256-addressed in R2. Unloaded pages remain estimated-height placeholders, so navigation and scrollbar range cover the complete conversation without decoding it. See [SessionHub session architecture](session-hub.md).

The desktop Changes pane follows the same bounded-projection principle. The host captures one checkout snapshot, builds a compact complete file manifest, and splits retained unified patch text into immutable content-addressed pages. The pane renders collapsed headers without parsing bodies, fetches pages only for expanded viewport ranges, and virtualizes file headers, hunk headers, notices, lines, pending review cards, and unloaded placeholders in one list. Its expanded view offers explicit unified and split layouts; split rows pair deletion/addition blocks while side-specific review ranges and cards remain anchored to old or new coordinates. Assistant turns additionally capture the complete non-ignored VCS tree before and after execution, preserving pre-existing working-copy changes while deriving an immutable net turn delta. To avoid attributing another concurrent session's writes, the published delta is restricted to paths reported by successful file tools. A turn that also used opaque, potentially mutating tools is labeled partial; a turn with no safely attributable paths publishes no diff. Git baselines live in a disposable alternate object store rather than adding unreachable objects to the user's repository. The compact manifest travels with the transcript entry; versioned content-addressed pages remain in the host's turn-diff store, are removed with their chat, and load through the same desktop viewer. Edge manifests are chat-authorized while page bodies are deduplicated per checkout. iOS renders only the inline turn manifest as a collapsed changed-files card; it has no diff pane and never requests patch pages.

Commit and Push are also checkout-scoped. Clients address a chat, the host resolves its canonical checkout, and one checkout mutex serializes Git/JJ mutations shared by every chat using it. Commit requests carry the exact diff catalog revision and selected file IDs; the host resolves those IDs to jailed paths, optionally generates a message from the retained patch, and revalidates before mutation. Git commits selected whole files and pushes its branch/upstream. JJ commits selected files into the completed `@-`, advances only Jolt-owned bookmarks, and pushes a `jolt/*` bookmark at `@-`, never mutable `@` or a user-owned bookmark. Action progress is a relay-forwardable stream; disconnected hosts do not queue publication work.

Review is a target-neutral, device-local lifecycle layered over reviewable surfaces rather than a diff transcript schema. A typed review draft owns a retained snapshot plus target-specific anchors; the initial diff adapter records old/new line coordinates and excerpts, while the model also reserves assistant-message text selectors. Draft bodies auto-save to local SQLite and never enter SessionHub or edge storage. Beginning a working-copy annotation leases that immutable diff revision, so later head updates produce a “newer changes available” notice instead of moving anchors. Sending formats all pending annotations and uses the composer's ordinary Run/Steer command path without replacing its visible draft or staged attachments; the local review clears only after command submission succeeds.

### Durable command plane

Run, shell, steer, interrupt, input-answer, and goal operations are typed SessionHub commands mirrored into host SQLite. The chat host:

1. claims a remote command under its current writer lease;
2. evaluates expiry and supersession rules;
3. marks the command in its local processed ledger **before** execution;
4. executes it at most once and resolves the mailbox row terminally.

Host-local commands persist and execute while offline, then reconcile by ID after reconnect. A DeviceRoom nudge wakes a cold host; command durability does not depend on the nudge.

### Machine-local state

The following stays on its owning device:

- agent/provider credential values;
- harness secret values;
- repository path registry and VCS selection;
- PTY processes and replay buffers;
- full run journals and stripped tool inputs;
- detailed token/cost usage;
- viewport layout, fonts, notifications, and keybindings.

Some local state is queried from a reachable device through RPC for display.

## Main flows

### Start a remote session

```text
viewport
  → Mutate createChat in a synced space
  → persist Run command in the local outbox
  → SessionHub validates and stores it idempotently
  → POST durable nudge to the host's DeviceRoom
  → host opens canonical SQLite state and claims the command
  → host launches the selected harness
  → normalized events fold into transcript chunks every ~120 ms
  → host publishes a bounded live-page delta
  → viewports apply sequenced transcript frames
```

### Live remote RPC

Filesystem browsing, model/ref discovery, terminals, account management, diffs, attachment reads, and updates can carry `targetDeviceId`. The local engine dials that device's DeviceRoom, wraps the normal RPC stream in a virtual socket, and forwards the unchanged method.

Secret methods are rejected by the host relay. Local identity, auth, and workspace watch/mutation methods do not honor `targetDeviceId` forwarding and are normally called on the directly connected engine; an iOS client can dial a host directly for the limited workspace mutation it needs.

## Storage layout

Default root: `~/.jolt`.

```text
~/.jolt/
  engine.lock
  session.json
  ui-settings.json
  themes/*.json              # installation-level paired custom palettes
  composer-defaults.json
  vcs-settings.json
  harness-secrets.json        # metadata only; values are in OS credentials
  repos.json
  repos/
  agent-accounts/
  logs/
  updates/
  scopes/local/current/
    local-scope-id
    device-id
    docs.sqlite3              # canonical normalized sessions + registry cache
    usage.sqlite
    journals/*.jsonl
    uploads/
  scopes/accounts/<org>/<user>/
    scope-layout-v1.json
    device-id
    docs.sqlite3              # canonical normalized sessions + registry cache
    usage.sqlite
    journals/*.jsonl
    uploads/
```

Git worktrees default to `~/.jolt/worktrees`; Jujutsu workspaces default to `~/.jolt/workspaces`.

Scope-isolated stores prevent Local data from entering edge synchronization and prevent a later WorkOS identity from reusing another account's cached documents. Scope and device identities use create-once atomic publication. Moving Local into Account prepares and rewrites a staged target while leaving the source immutable, then publishes the target and creates a fresh Local scope; a failed merge therefore keeps Local attachment references valid. Existing `orgs/<org>/<user>` stores are moved into this layout on first startup; the existing account device identity is preserved and a fresh Local scope is created.

## Rust workspace

| Path | Responsibility |
| --- | --- |
| `crates/proto` | Shared entities, agent events, usage, secrets, and pure view derivations |
| `crates/platform` | Login-shell process environment and suspend/wake detection |
| `crates/session-doc` | Semantic message/command types and transcript projection contracts |
| `crates/registry-model` | Workspace registry rows, HLC operations, and optimistic local state |
| `crates/store` | Canonical normalized SQLite sessions, registry cache, and processed-command ledger |
| `crates/sync` | SessionHub host client and workspace-registry network client |
| `crates/harness` | Common harness trait, controls, environment provider, and test mock |
| `crates/harness-{claude,codex,pi}` | Isolated production CLI adapters and protocol tests |
| `crates/mcp` | Loopback MCP host, bearer leases, tool schemas, and backend contract |
| `crates/vcs` | Device-local repositories, VCS commands, workspaces, and forge review lookup |
| `crates/terminal` | Engine-side PTY ownership, replay, and lifecycle |
| `crates/rpc` | Generic envelopes, clients/servers, and in-memory/WebSocket transports |
| `crates/api` | Product RPC methods, shared models, and typed unary/JSON-stream/binary-stream contracts |
| `crates/relay` | DeviceRoom frame codec, host relay, and peer link cache |
| `crates/engine` | Runtime assembly, sessions, document hosts, capability coordination, accounts, secrets, and usage |
| `crates/update` | Release checks, downloads, verification, swaps, and restart support |
| `crates/ui` | gpui desktop shell and views |
| `apps/jolt` | Binary, CLI auth, daemon management, and environment resolution |
| `apps/ios` | SwiftUI mobile viewport |
| `edge` | Worker, Durable Objects, R2 routes, and WorkOS integration |

## Design invariants

- A session has one immutable host device and one fenced transcript writer.
- A space fixes its owning device and folder.
- Canonical transcripts live in host SQLite; SessionHub carries typed commands and bounded projections.
- Command outcomes are host-written and locally claimed before execution.
- Viewports use the same RPC envelope in-process, over localhost, and through the relay.
- Secret values never cross the device relay.
- Usage telemetry remains device-local and outside synchronized documents.
- Live status and presence are freshness-gated; durable state remains independent of heartbeats.

See [Synchronization](sync.md), [RPC](rpc.md), and [Security and data](security.md) for protocol-level detail.
