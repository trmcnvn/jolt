# Settings

Open settings from the user menu or with `Cmd+,` on macOS / `Ctrl+,` elsewhere. Unless noted, preferences stay on the current device.

## Pages

| Page | What it controls | Scope |
| --- | --- | --- |
| **Devices** | Registered devices, presence, version, rename, and copyable device ID | Device rows sync; UI state is local |
| **Accounts** | Claude Code and Codex login slots, activation, removal, and provider quota meters | Selected engine device |
| **Secrets** | Write-only environment secrets scoped to Claude Code, Codex, and/or Pi | Local engine only |
| **Version control** | Active Git or Jujutsu command-line backend and executable status | Selected engine device |
| **Terminal** | Command used when a new terminal tab opens | Local viewport setting |
| **Appearance** | System/light/dark mode and UI, code, and terminal fonts | Local viewport setting |
| **Notifications** | In-app toasts versus operating-system notifications | Local viewport setting |
| **Shortcuts** | Rebindable app actions with conflict detection and reset controls | Local viewport setting |
| **Archived sessions** | Browse and restore archived chats | Synced chat rows |

### Accounts

Account switching changes credentials on the selected device, not across the fleet. Jolt supports account-slot management for Claude Code and Codex. Pi provider authentication stays in Pi and is configured with Pi's `/login`.

Quota meters reflect provider/CLI rate-limit windows. They turn warning at 80% and critical at 95%, with reset time when reported. These meters are separate from Jolt's session token ledger.

### Secrets

Secret values are never returned after creation. Metadata is stored in `harness-secrets.json`; values stay in the operating-system credential store and are injected only into selected harness child processes. Secret RPC methods are unavailable through the device relay.

See [Security and data](security.md).

### Version control

Jolt prefers Jujutsu 0.43 or newer when available and no backend choice has been saved, then falls back to Git. The selection is per engine device. A device switcher at the top of the page lets a viewport configure a reachable remote engine.

### Terminal

Leave **Launch command** blank to open the default interactive login shell. A custom command runs through the login shell in the session directory and applies only to terminals opened after the change.

### Appearance

- **System:** follow OS light/dark changes.
- **Light:** always use Jolt's light palette.
- **Dark:** always use Jolt's dark palette.

Font pickers enumerate installed fonts. Code font applies to code blocks, diffs, and shortcut chips; terminal font applies to the PTY grid.

### Notifications

Jolt defaults to in-app toasts. Enabling **System notifications** sends app-wide harness updates, Claude/Codex quota warnings, Jolt update notices, and app-wide errors through the OS instead.

## Default shortcuts

`Mod` means Command on macOS and Control elsewhere.

| Action | Default |
| --- | --- |
| New session | `Mod+N` |
| Clear input | `Mod+C` |
| Archive current session | `Mod+W` |
| Open settings | `Mod+,` |
| Add space | `Mod+K` |
| Toggle left sidebar | `Mod+E` |
| Toggle Changes pane | `Mod+B` |
| Toggle terminal | ``Mod+` `` |

The shortcuts page records key combinations, detects duplicate assignments, resets one action, or restores defaults. Fixed native macOS menu shortcuts such as `Cmd+Q`, `Cmd+H`, and `Cmd+M` are not part of this customizable map.

Session tabs also support `Mod+1` through `Mod+9` for direct selection.

## User menu

The user menu includes:

- **Settings**
- **Usage breakdown** — 7-, 30-, or 90-day summaries merged from reachable devices
- **Check for update** or the current update state
- **Sign out**

Current-session context and token usage remains in the composer footer rather than settings.

## Persistence

Desktop viewport settings are stored in `{data_dir}/ui-settings.json`. They include pane sizes, sidebar state, selected space, local tab/space ordering, notification mode, keymap, appearance, fonts, and terminal command.

New-session composer defaults are stored separately in `{data_dir}/composer-defaults.json`. Corrupt or missing settings files fall back to defaults; numeric layout values are clamped on load.

The default data directory is `~/.jolt`. Set `JOLT_DATA_DIR` to isolate another installation.
