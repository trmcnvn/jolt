# Using Jolt

Jolt separates where work is displayed from where it runs. A desktop window or iPhone can control a thread, but the agent process, terminal, repository, and files remain on the thread's host device.

## Core concepts

### Local and Account

Desktop Jolt always has a Local scope that never synchronizes. Signing in adds an Account scope for cross-device synchronization, remote control, and iOS. **Switch to Local** changes only the desktop viewport, so Account threads can keep running and syncing in the background. Local and Account have separate spaces, threads, device identities, journals, usage, and uploads; harness credentials and Jolt secrets remain device-local. Switching between Local and Account always opens the New Thread page.

Moving non-empty Local data into an account requires explicit approval. After the move, Jolt creates a fresh blank Local scope. Keeping data Local means those threads are unavailable remotely and on iOS.

### Devices

A device is a signed-in Jolt installation with a stable local ID. Engine hosts own:

- agent CLI processes and credentials;
- local folders, repositories, worktrees, and Jujutsu workspaces;
- PTYs and working-tree diffs;
- local transcript snapshots, run journals, and usage records.

iOS installations are registered viewer devices: they appear in **Settings → Devices** and publish presence, but they cannot host spaces, harnesses, or device RPCs. Presence indicates recent contact. Jolt also checks the device relay before remote calls, so a stale presence row does not make an offline engine look usable forever. Removing an engine device cascades through its spaces and threads without deleting folders or other local files; viewer devices own no spaces.

### Spaces

A space is a synced `(device, folder)` pair. Spaces filter the desktop thread list and provide the host device and base folder for new threads; they are execution context rather than the navigation spine.

The owning engine detects whether the folder is under the selected version-control backend and stamps checkout metadata into the workspace registry. Renaming a space changes only its display name. Deleting one tombstones the space and its chat/session index rows. Any chat deletion also retires its edge transcript room and asynchronously purges its R2 backup and attachments.

### Threads

A thread is one durable conversation attached to a space. Its row records the immutable host device, folder, checkout, harness configuration, title, activity, pin, and seen state. Canonical transcript state lives in host SQLite; SessionHub carries its typed command mailbox and bounded viewer projection. After the first completed exchange, an untitled thread is named asynchronously with an economy-tier model; a user rename always wins, and Jolt-created worktree branches can be renamed with the generated title. The thread-row context menu can regenerate an existing name through the same model path.

The desktop shows pinned threads first, then the remaining threads by recency. Pins sync across viewports and remain subject to the selected space filter. Threads are selected directly from this active sidebar order; `Mod+1` through `Mod+9` select its first nine rows, and holding `Mod` replaces those rows' timestamps with their configured shortcuts. Closing moves a thread into the compact **Closed** section below active threads without clearing its pin; an active run must be stopped first. Jolt also closes inactive threads automatically after three days, while live runs and active goals stay open. Selecting a closed row opens it without reopening it. Sending a message or using its hover reopen control returns it to the active list and its pinned bucket; reopening starts a fresh three-day inactivity window. Closed rows load automatically as the sidebar approaches the end, and title search covers active and closed threads.

## The desktop shell

- **Left sidebar:** searchable space filter (`Mod+Shift+K`), filtered active and closed threads, title search (`Mod+Shift+F`), add-space action, and user menu.
- **Thread header:** identifies the selected thread and its `space @ device` provenance while keeping Commit/Push, New Thread, terminal, and Changes controls available.
- **Conversation:** virtualized transcript with Markdown, code highlighting, grouped tools, input requests, errors, attachments, and a message rail on wide layouts. `Mod+Shift+Up/Down` moves between user prompts, including unloaded history.
- **Composer:** prompt input, separate model and traits controls, checkout controls, attachments, context usage, and send/steer/stop state.
- **Terminal panel:** thread-scoped PTY tabs hosted on the thread's device.
- **Changes pane:** the latest working-copy patch for the thread checkout.

Sidebar, Changes, and terminal dimensions are resizable. Panel open state is per thread; dimensions and sidebar state persist on the local device.

## Creating a thread

A new thread defaults to the sidebar's filtered space, then the last active valid space. The canvas space picker can change the target device and base folder before sending. Jolt also lets you choose:

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

Type `@` at a token boundary to search files and directories inside the current thread checkout. In a new thread, search is rooted in the selected space or existing worktree. Jolt stores the selection as a `jolt-file:` Markdown link and renders it as an atomic file chip.

Only paths verified inside the resolved checkout are returned. Mentioning a file does not upload its contents; the agent reads it through its own tools if needed.

### Slash commands

Type `/` as the first message token to open Jolt's command completion menu.

| Command | Behavior |
| --- | --- |
| `/answer` | Extract answerable questions from the latest completed assistant response and present them one at a time. Answers are compiled into the next turn. |
| `/bro` | Ask the active harness to restate the latest assistant response plainly and concisely. |
| `/goal` | Open the goal manager. |

Create, edit, budget, pause, resume, and clear goals through the goal manager; there are no `/goal` subcommands. Opening it from the new-thread canvas creates the thread when the goal is submitted. The composer shows objective, status, and token usage while a goal exists. Goals work with Claude Code, Codex, and Pi; Jolt injects the goal contract and schedules hidden continuation turns rather than relying on a harness-specific command.

These are Jolt commands, not a passthrough to an agent CLI's interactive command parser.

### Shell commands

A leading bang runs a shell command on the thread's host device without starting a normal agent turn:

```text
!cargo test
!!pwd
```

- `!command` includes the output in the harness context.
- `!!command` keeps the output local to the Jolt transcript.
- Three or more leading bangs are treated as ordinary prompt text.

Pi handles this through its native RPC bash command. Other harnesses use Jolt's host-side shell fallback. Attachments cannot be sent with a shell command.

### Attachments

Paste, drop, or pick images. Jolt stages them on the thread's host device, persists local-file references in the prompt, and sends inline image blocks to harnesses that support them. Sent images render as thumbnails and can be opened in a lightbox.

If upload fails, the files return to the thread draft. Attachment values are not placed in the workspace registry.

### Questions from agents

When a harness requests structured input—or an agent calls Jolt's `request_answers` MCP tool—the composer becomes a paged question panel. Single-choice questions can auto-advance; multi-choice and typed answers require explicit submission. Number keys select visible options. The request remains answerable across transient sync or run-state changes until it is resolved, and the answers return directly to the waiting agent tool call.

`/answer` remains available for assistant responses that asked questions only in prose: it extracts those questions and compiles the completed answer pages into the next turn.

## Transcript and status

Jolt derives the visible transcript from normalized host state and its bounded projection. Consecutive tools are grouped and can be folded. Assistant text is durably synced in small increments but appears only when the harness completes a semantic message chunk; tools and status remain live. Interrupted or failed runs reveal any preserved partial text.

Thread indicators are derived from live status plus the synced seen marker:

- **Working**
- **Awaiting input**
- **Errored**
- **Completed but unseen**
- **Idle**

Live status is freshness-gated so a crashed engine cannot leave a permanent Working indicator. Opening a thread updates its synced seen marker on every device. When a harness compacts its context, Jolt syncs the ephemeral compacting flag and shows **Compacting context…** without writing a transcript part.

## Terminal and changes

Terminal tabs are PTYs on the host device. Detaching or hiding the panel does not close the shell. Output has a bounded replay window so a viewport can reconnect and continue from a sequence number. Drag to select cells, double-click words, or triple-click lines; copy with `Cmd+C` on macOS or `Ctrl+Shift+C` elsewhere.

The Changes pane shows the latest bounded working-copy diff for the checkout. It supports per-file folding, additions/deletions, syntax highlighting, binary markers, and a partial-snapshot notice when the patch reaches its size cap. Click a textual diff line to add pending review feedback; Shift-click another line in the same file to extend the range. Comments auto-save only on this device. Once annotation begins, the reviewed revision remains fixed and a newer working copy is reported without moving existing anchors. **Send feedback** groups every pending comment into one ordinary user message while preserving text and attachments already staged in the composer.

The thread header’s version-control split control derives its primary action from the same concrete checkout: **Commit**, **Commit & push**, or **Push**. Commit opens a whole-file selector and accepts an optional message; leaving it blank generates one with the thread harness’s economy model. Actions run on the checkout host, are disabled while an agent is active in the same checkout, reject stale reviewed revisions, and never force-push. Git pushes the current branch and establishes its upstream on first publication. Jujutsu commits selected files while leaving the remainder in the new `@`; Push leaves `@` untouched and publishes the completed `@-` through a Jolt-owned `jolt/*` bookmark rather than moving a user bookmark.

Completed assistant turns that changed files also show a collapsed `N changed files +A −D` card in the transcript. Expanding it lists files; on desktop, selecting a file or **Open diff** opens that immutable turn delta in the Changes pane. iOS shows the summary and file list without diff opening. Successful edit/write chips are replaced by the card, while failed mutation chips remain visible.

## Usage

The context wheel beside the composer’s model and traits controls shows current-thread prompt, output, cache, context, model, and estimated API-equivalent cost data. Costs are estimates, not subscription charges; Codex Fast uses the corresponding API Priority rates rather than ChatGPT credit multipliers. Its context warning changes at 70% and 90% of the model window.

Open **Usage breakdown** from the user menu for 7-, 30-, or 90-day activity grouped by device, harness, model, and space. Usage is recorded on each host device and merged from reachable devices for display; it is not synced in conversation documents.

Provider account quota meters are separate. Claude Code and Codex account cards warn at 80% and 95% of their reported rate-limit windows.

## Offline behavior

Prompts, steering, interrupts, and input answers are durable thread-document commands. A remote viewport can queue work while the host is disconnected; the command remains pending until the host reconnects, joins the document, and drains it. A durable device-room nudge makes cold hosts open the relevant document without keeping every chat resident.

Filesystem RPCs, terminals, account changes, and live attachment upload require the target engine to be reachable.
