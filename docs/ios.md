# Jolt for iOS

The iOS app is a native SwiftUI viewport onto the same Jolt account. It syncs workspace rows and session documents directly with the edge and sends durable commands to the computer running each session's engine and agent CLI.

## Requirements and build

The project requires Xcode 26 or newer and the iOS 26 SDK.

```bash
cd apps/ios
xcodebuild -project Jolt.xcodeproj -scheme Jolt \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build
```

Or open `apps/ios/Jolt.xcodeproj` and run the shared `Jolt` scheme.

Swift Package Manager resolves:

- `loro-swift` 1.13.x for session documents;
- `swift-markdown` for GFM-compatible transcript rendering.

## Sign in

The app uses the same WorkOS account as desktop Jolt. It opens the Jolt authorization page, exchanges the callback code through the edge, adopts the sole organization membership or creates `Personal`, and stores access/refresh tokens in Keychain.

Development mode can connect to an edge running with `AUTH_MODE=dev`, where the bearer is `userId@orgId`.

## What the mobile viewport can do

- View attention-sorted sessions across all devices.
- Browse spaces and their sessions.
- Add a space by choosing a device and browsing that remote engine's folders.
- Start Claude Code, Codex, or Pi sessions on a space's host device.
- Discover models and reasoning levels from the target device.
- Choose a ref, reuse a worktree, or create a new worktree for a session.
- Stream transcripts with Markdown, code highlighting, grouped tools, errors, and input requests.
- Send, steer, stop, and answer structured questions.
- Attach images from Photos; uploads are chunked to the host device.
- Change model, reasoning, and ref for later turns.
- Archive sessions with a swipe.
- Use context menus to copy transcript text and code.

The app shows device online state and warns when a run is queued for an offline host.

## Mobile translations

| Desktop | iOS |
| --- | --- |
| Sidebar spaces and global session list | Home screen sections |
| Horizontal session tabs | Space detail session list |
| Close tab to archive | Swipe to archive |
| Harness/model popover | Model and effort sheets |
| Add-space palette | Device tabs and remote folder browser |
| gpui virtual list | SwiftUI `LazyVStack` with stable row IDs |
| Hover actions | Context menus |

Engine and desktop devices provide terminals, working-copy diffs, agent account switching, harness secrets, and desktop settings.

## Synchronization

The phone uses three paths:

1. **Workspace registry:** JSON WebSocket protocol to the per-user `reg1` registry room for devices, spaces, chats, and session status.
2. **Session documents:** Loro protocol 0.3 to each chat room for transcript projection and command appends.
3. **Device relay:** binary device-room frames carrying RPC when folder browsing, model/ref discovery, worktree creation, or host file upload requires a live engine.

Run, steer, interrupt, and input-answer operations are appended to the session's durable command ledger. The phone then posts a device nudge so a cold host opens the document. If the host is offline, the command remains queued.

Workspace registry and Loro snapshots are cached on disk for fast reopen. Signing out clears identity-scoped document caches. WorkOS tokens remain in Keychain.

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
- Session sync: `apps/ios/Jolt/Sync/RoomClient.swift`, `SessionStore.swift`
- Relay RPC: `apps/ios/Jolt/Sync/DeviceRelayClient.swift`
- Composer and attachments: `apps/ios/Jolt/Composer/`
- Transcript and Markdown: `apps/ios/Jolt/Transcript/`, `apps/ios/Jolt/Markdown/`
