# Using Jolt

Jolt separates where work is displayed from where it runs. A desktop window or iPhone can control a session, but the agent process, terminal, repository, and files remain on the session's host device.

## Core concepts

### Devices

A device is one Jolt engine installation with a stable local ID. Its engine owns:

- agent CLI processes and credentials;
- local folders, repositories, worktrees, and Jujutsu workspaces;
- PTYs and working-tree diffs;
- local transcript snapshots, run journals, and usage records.

Presence indicates recent contact. Jolt also checks the device relay before remote calls, so a stale presence row does not make an offline engine look usable forever.

### Spaces

A space is a synced `(device, folder)` pair. Spaces are the top-level unit in the desktop sidebar. A space fixes the host device and base folder for sessions created inside it.

The owning engine detects whether the folder is under the selected version-control backend and stamps checkout metadata into the workspace registry. Renaming a space changes only its display name. Deleting one tombstones the space and its chat/session index rows; transcript documents are separate.

### Sessions

A session is one durable conversation attached to a space. Its row records the host device, folder, checkout, harness configuration, title, activity, and seen state. The transcript and command queue live in a separate Loro document. After the first completed exchange, an untitled session is named asynchronously with an economy-tier model; a user rename always wins, and Jolt-created worktree branches can be renamed with the generated title.

The desktop shows a global, attention-sorted session list and horizontal tabs within the selected space. Closing a tab archives the session. Archived sessions can be restored from **Settings → Archived sessions**.

## The desktop shell

- **Left sidebar:** spaces, global sessions, add-space action, and user menu.
- **Session tabs:** open sessions in the selected space; tabs can be reordered locally.
- **Conversation:** virtualized transcript with Markdown, code highlighting, grouped tools, input requests, errors, attachments, and a message rail on wide layouts.
- **Composer:** prompt input, harness/model controls, checkout controls, attachments, context usage, and send/steer/stop state.
- **Terminal panel:** session-scoped PTY tabs hosted on the session's device.
- **Changes pane:** the latest working-copy patch for the session checkout.

Sidebar, Changes, and terminal dimensions are resizable. Panel open state is per session; dimensions and sidebar state persist on the local device.

## Creating a session

The selected space supplies the target device and base folder. Before the first send, Jolt lets you choose:

- harness, model, reasoning level, and harness-specific model options;
- the current checkout;
- an existing worktree/workspace for the selected ref; or
- a fresh isolated worktree/workspace created on send.

Harness identity is fixed after creation. Model, reasoning, options, and ref can be changed for later turns. Harness-native resume IDs are reused only when the working directory still matches, because CLI session stores are directory-scoped.

## Composer controls

### Send, steer, and stop

- **Send:** starts a turn while idle.
- **Steer:** when a steerable run is active and the input is non-empty, delivers guidance at the harness's next supported boundary.
- **Stop:** when a run is active and the input is empty, queues an interrupt.
- **Enter:** submit.
- **Shift+Enter:** insert a newline.

Prompts appear optimistically with a client-minted message ID. If delivery fails, Jolt removes the echo and restores the prompt and attachments to the draft.

### File mentions

Type `@` at a token boundary to search files and directories inside the current session checkout. In a new session, search is rooted in the selected space or existing worktree. Jolt stores the selection as a `jolt-file:` Markdown link and renders it as an atomic file chip.

Only paths verified inside the resolved checkout are returned. Mentioning a file does not upload its contents; the agent reads it through its own tools if needed.

### Slash commands

Type `/` as the first message token to open Jolt's command completion menu.

| Command | Behavior |
| --- | --- |
| `/answer` | Extract answerable questions from the latest completed assistant response and present them one at a time. Answers are compiled into the next turn. |
| `/bro` | Ask the active harness to restate the latest assistant response plainly and concisely. |

These are Jolt commands, not a passthrough to an agent CLI's interactive command parser.

### Shell commands

A leading bang runs a shell command on the session's host device without starting a normal agent turn:

```text
!cargo test
!!pwd
```

- `!command` includes the output in the harness context.
- `!!command` keeps the output local to the Jolt transcript.
- Three or more leading bangs are treated as ordinary prompt text.

Pi handles this through its native RPC bash command. Other harnesses use Jolt's host-side shell fallback. Attachments cannot be sent with a shell command.

### Attachments

Paste, drop, or pick images. Jolt stages them on the session's host device, persists local-file references in the prompt, and sends inline image blocks to harnesses that support them. Sent images render as thumbnails and can be opened in a lightbox.

If upload fails, the files return to the session draft. Attachment values are not placed in the workspace registry.

### Questions from agents

When a harness requests structured input, the composer becomes a paged question panel. Single-choice questions can auto-advance; multi-choice and typed answers require explicit submission. Number keys select visible options. The request remains answerable across transient sync or run-state changes until it is resolved.

## Transcript and status

Jolt derives the visible transcript from the session document. Consecutive tools are grouped and can be folded. Streaming text is committed in small increments and rendered with a paint-only fade without changing layout.

Session indicators are derived from live status plus the synced seen marker:

- **Working**
- **Awaiting input**
- **Errored**
- **Completed but unseen**
- **Idle**

Live status is freshness-gated so a crashed engine cannot leave a permanent Working indicator. Opening a session updates its synced seen marker on every device. When a harness compacts its context, Jolt syncs the ephemeral compacting flag and shows **Compacting context…** without writing a transcript part.

## Terminal and changes

Terminal tabs are PTYs on the host device. Detaching or hiding the panel does not close the shell. Output has a bounded replay window so a viewport can reconnect and continue from a sequence number. Drag to select cells, double-click words, or triple-click lines; copy with `Cmd+C` on macOS or `Ctrl+Shift+C` elsewhere.

The Changes pane shows the latest bounded working-copy diff for the checkout. It supports per-file folding, additions/deletions, syntax highlighting, binary markers, and a partial-snapshot notice when the patch reaches its size cap.

## Usage

The `$` glyph in the composer footer shows current-session prompt, output, cache, context, model, and reported cost data. Its context warning changes at 70% and 90% of the model window.

Open **Usage breakdown** from the user menu for 7-, 30-, or 90-day activity grouped by harness, model, and space. Usage is recorded on each host device and merged from reachable devices for display; it is not synced in conversation documents.

Provider account quota meters are separate. Claude Code and Codex account cards warn at 80% and 95% of their reported rate-limit windows.

## Offline behavior

Prompts, steering, interrupts, and input answers are durable session-document commands. A remote viewport can queue work while the host is disconnected; the command remains pending until the host reconnects, joins the document, and drains it. A durable device-room nudge makes cold hosts open the relevant document without keeping every chat resident.

Filesystem RPCs, terminals, account changes, and live attachment upload require the target engine to be reachable.
