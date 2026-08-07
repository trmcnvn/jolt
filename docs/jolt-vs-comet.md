# How Jolt differs from Comet

Jolt is an ADE forked from [Comet](https://github.com/zeronsh/comet) and retains its core model: local engines run coding-agent CLIs, Loro documents carry durable conversations and commands, Cloudflare Durable Objects synchronize account data, and desktop and iOS apps act as viewports.

This page lists Jolt's deliberate additions and product changes rather than every inherited feature. The comparison was reviewed against Comet `main` at [`2b0dc843`](https://github.com/zeronsh/comet/commit/2b0dc843d940), the upstream revision available at the time. Features ported from Comet are not presented as Jolt differences.

## Product and workflow differences

| Area | Comet | Jolt |
| --- | --- | --- |
| Product identity | `comet` binaries, app IDs, paths, endpoints, and branding | Renamed throughout to `jolt`, including desktop, iOS, crates, packaging, edge services, and `~/.jolt` runtime paths |
| Desktop account requirement | Starts behind the WorkOS sign-in and organization flow | Always provides a fully offline **Local** scope; sign-in adds a separate **Account** scope for sync, remote control, and iOS |
| Organization model | Exposes organization creation or selection | Uses one hidden organization per user: adopt the sole membership or create `Personal`; multiple memberships produce an explicit setup error |
| Agent harnesses | Claude Code and Codex | Adds Pi through its bidirectional RPC mode, with provider-qualified dynamic models, exact thinking levels, steering, resume, extension UI, native bash, tool-access choices, and project-resource trust decisions |
| Version control | Git repositories and worktrees | Adds Jujutsu 0.43+ repositories, revisions, workspaces, diffs, and per-device VCS settings; Jujutsu is preferred when no backend has been selected |
| Session navigation | A selected space owns the visible tabs; closing a tab archives its session | Spaces are a searchable sidebar filter over an attention-sorted session list; open tabs are device-local and can span spaces; closing a tab does not archive the synced session |
| Composer helpers | Normal prompts, steering, structured input, attachments, and file mentions | Adds `!`/`!!` host shell commands plus `/answer`, `/bro`, and `/goal` app commands on desktop and iOS |
| Follow-up work | Durable run commands can wait for the host | Adds an explicit cancellable FIFO follow-up-turn queue, queue pause/resume after interruption or failure, and guarded continuation after harness context compaction |
| Long-running goals | No native goal lifecycle | Adds harness-neutral goals with objectives, optional token budgets, usage accounting, edit/pause/resume/clear controls, and hidden continuation turns across Claude Code, Codex, and Pi |
| Usage | Provider quota meters; granular token usage is intentionally excluded | Adds current-session context, prompt/output/cache token and reported-cost details, plus device-local SQLite history grouped by harness, model, and space and merged from reachable devices |
| Harness secrets | No dedicated harness environment-secret store | Stores values in the OS credential store, keeps only labels and scopes in local metadata, injects values only into selected harness subprocesses, and never relays or syncs values |
| Notifications | Optional completion and input-request chimes | Uses silent in-app toasts by default, with an opt-in switch to OS notifications for app-wide events; Jolt has no notification sounds |
| Appearance | System, Light, and Dark appearance modes | Adds independently selected light and dark themes, Jolt/Catppuccin/Rosé Pine pairs, a custom palette editor, opportunistic custom-theme file sync, and independent interface, prompt, code, and terminal typography sizes |
| iOS workflow | Space-section navigation backed by complete per-session Loro replicas | Uses a sessions-first searchable space filter and global New Session route; adds host file mentions, shell/app commands, later-turn image attachments, goal management, paged transcripts, and a persistent command outbox |
| Desktop Markdown | Markdown, tables, task lists, code highlighting, and selectable text | Also renders inline/display math and `math`, `latex`, `tex`, and `mermaid` fences locally without a web renderer |

## Data-plane and scaling differences

### Local and Account runtimes

Jolt keeps Local and Account data in separate scope directories. Local never receives an edge token or joins registry, session, or device rooms. Switching the viewport to Local does not stop Account-hosted runs or synchronization in the background, and signing out returns the viewport to Local. When a user signs in with existing Local work, Jolt can keep it separate or move its synchronized session data into the account while explaining which machine-local data stays behind.

### Paged transcripts

Comet viewports replicate complete session documents. Jolt keeps the canonical full Loro document in the host engine and edge, but viewports consume a tail-first projection:

- a compact manifest representing the whole conversation;
- at least 64 trailing messages on open;
- byte-bounded historical pages loaded by opaque ID;
- sequenced live-page deltas; and
- estimated-height placeholders, so the message rail and scrollbar still represent unloaded history.

Desktop retains a bounded UI page window. iOS no longer joins complete per-session Loro rooms; it keeps a byte-budgeted page cache and submits commands through a persistent, idempotent outbox that the edge appends to canonical Loro.

### Paged and per-turn diffs

Comet's Changes pane watches one bounded latest working-copy patch. Jolt replaces that viewport contract with a compact complete file manifest and immutable, content-addressed patch pages loaded only for expanded visible ranges.

Jolt also snapshots the non-ignored VCS tree around each assistant turn. A completed turn can show an `N changed files +A −D` card whose immutable net delta opens in the same Changes pane, without folding unrelated pre-existing working-copy changes into that turn.

### Cleanup and device-local data

Jolt adds durable cleanup for deleted chats and removed devices, including session-room retirement and synchronized backup/attachment deletion. Detailed usage, secret values, VCS choice, tabs, layout, active themes, and typography remain device-local; only the explicitly documented account data and custom-theme files synchronize.

## Shared upstream improvements

Jolt continues to port relevant Comet work. For example, both projects include the large-session update-log fix, Codex `danger-full-access`/no-approval operation, appearance-aware terminal ANSI colors, transcript fade correction, and terminal mouse selection and copy. Those shared behaviors are intentionally absent from the difference table.

## Compatibility

Jolt's renamed services and runtime configuration, expanded schemas and RPC surface, separate scope layout, transcript/diff projections, and edge routes are developed as one Jolt stack. Do not mix Jolt and Comet clients, engines, or edge deployments unless a specific compatibility path is documented.
