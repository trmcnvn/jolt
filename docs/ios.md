# Jolt for iOS

The iOS app is a native SwiftUI viewport onto the same Jolt account. It syncs workspace rows and a tail-first transcript projection from the edge, then sends durable commands to the computer running each session's engine and agent CLI.

Jolt's native mobile apps follow one [mobile feature parity policy](mobile-parity.md). iOS is the behavioral reference while Android is being ported; after Android scaffolding starts, new mobile capabilities are implemented and released for both apps together rather than advancing either platform independently.

## Requirements and build

The project requires Xcode 26 or newer and the iOS 26 SDK.

```bash
cd apps/ios
xcodebuild -project Jolt.xcodeproj -scheme Jolt \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build
```

Or open `apps/ios/Jolt.xcodeproj` and run the shared `Jolt` scheme.

Swift Package Manager resolves:

- `loro-swift` 1.13.x for compatibility fixtures and document tooling;
- `swift-markdown` for GFM-compatible transcript rendering.

## Sign in

The app uses the same WorkOS account as desktop Jolt. It opens the Jolt authorization page, exchanges the callback code through the edge, adopts the sole organization membership or creates `Personal`, and stores access/refresh tokens in Keychain.

Development mode can connect to an edge running with `AUTH_MODE=dev`, where the bearer is `userId@orgId`.

## What the mobile viewport can do

- View attention-sorted sessions across all devices, filtered by a searchable space picker.
- Open a space as a filtered deep link when navigating from external context.
- Add a space by choosing a device and browsing that remote engine's folders.
- Ask a space's host device to start Claude Code, Codex, or Pi; no harness runs on the phone.
- Discover harnesses, models, and reasoning levels from the target device.
- Use the host's configured Git or Jujutsu backend to choose a ref/revision, reuse a worktree/workspace, or create an isolated one.
- Stream transcripts with Markdown, code highlighting, grouped tools, immutable per-turn changed-file summaries, errors, and input requests.
- Send, steer, stop, and answer structured questions.
- Paste images or attach them from Photos on the first or any later message; uploads are chunked to the host device.
- Search the host checkout with `@` and send file/directory mentions as cross-platform `jolt-file:` links.
- Use `/answer`, `/bro`, `!command`, and `!!command` from the composer.
- Change model, reasoning, and ref for later turns.
- Archive sessions with a left swipe and delete them with a confirmed right swipe.
- Use context menus to copy transcript text and code.

The app registers the iPhone as an iOS viewer in the account device list, publishes presence, shows host-device online state, and warns when a run is queued for an offline host.

## Mobile translations

| Desktop | iOS |
| --- | --- |
| Searchable sidebar space filter and session list | Sessions-first Home screen with a searchable space filter |
| Device-local horizontal tabs | Native navigation stack; no mobile tab strip |
| Explicit Archive and Delete actions | Swipe left to archive; swipe right to delete with confirmation |
| Harness/model popover | Model and effort sheets |
| Global New Session canvas + space picker | Global New Session route + searchable space sheet |
| Add-space palette | Device tabs and remote folder browser |
| gpui virtual list | SwiftUI `LazyVStack` with stable row IDs and estimated unloaded-page placeholders |
| Hover actions | Context menus |
| Turn Changes card with historical diff opening | Collapsed Changes card and expandable file list only |

Engine and desktop devices provide terminals, working-copy diffs, historical diff opening, agent account switching, harness secrets, and desktop settings.

## Synchronization

The phone uses three paths:

1. **Workspace registry:** JSON WebSocket protocol to the per-user `reg1` registry room for devices, spaces, chats, and session status.
2. **Transcript projection:** an edge WebSocket opens with a compact whole-session manifest and trailing pages; older byte-bounded pages load over authenticated HTTP as scrolling reaches them.
3. **Device relay:** binary device-room frames carrying RPC when folder browsing, file-mention search, model/ref discovery, worktree creation, or host file upload requires a live engine.

Run, steer, interrupt, and input-answer operations are written to a device-local durable outbox, submitted idempotently to the edge, and appended there to the canonical Loro command ledger. The phone then posts a device nudge so a cold host opens the document. If either network or host is offline, the command remains queued.

Workspace registry, transcript manifests/pages, and pending commands are cached on disk for instant reopen and offline history. Transcript page files use a byte-budgeted LRU. Signing out clears identity-scoped caches. WorkOS tokens remain in Keychain.

## Demo and test modes

Launch with `-demo` to load an offline data set. Useful options include:

```text
-demo -route chat:<id> -stream
-demo -route space:<id>
-demo -sheet newsession
-demo -sheet newspace
```

The project also includes benchmark and edge/relay E2E runners behind launch arguments used by development rigs.

## Source map

- App state and authentication: `apps/ios/Jolt/App/`, `apps/ios/Jolt/Auth/`
- Workspace registry: `apps/ios/Jolt/Sync/Registry*.swift`
- Session sync: `apps/ios/Jolt/Sync/TranscriptProjectionClient.swift`, `SessionStore.swift`
- Relay RPC: `apps/ios/Jolt/Sync/DeviceRelayClient.swift`
- Composer and attachments: `apps/ios/Jolt/Composer/`
- Transcript and Markdown: `apps/ios/Jolt/Transcript/`, `apps/ios/Jolt/Markdown/`
