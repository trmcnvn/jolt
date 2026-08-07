# Settings

Open settings from the user menu or with `Cmd+,` on macOS / `Ctrl+,` elsewhere. Unless noted, preferences stay on the current device.

## Pages

| Page | What it controls | Scope |
| --- | --- | --- |
| **Devices** | Registered devices, presence, version, rename/removal, and copyable device ID | Device rows sync; UI state is local |
| **Accounts** | Claude Code and Codex login slots, activation, removal, and provider quota meters | Selected engine device |
| **Secrets** | Write-only environment secrets scoped to Claude Code, Codex, and/or Pi | Local engine only |
| **Version control** | Active Git or Jujutsu command-line backend and executable status | Selected engine device |
| **Terminal** | Command used when a new terminal tab opens | Local viewport setting |
| **Appearance** | System/light/dark mode, paired themes, custom colors, and typography | Local viewport setting; custom theme files are installation-level |
| **Notifications** | In-app toasts versus operating-system notifications | Local viewport setting |
| **Hotkeys** | Rebindable app commands with conflict detection and reset controls | Local viewport setting |

Removing a device tombstones its spaces and sessions. Synced R2 backups and attachments are purged asynchronously; folders and other local files on that machine are unaffected. If Jolt later starts there again, it registers as an empty device.

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
- **Light:** always use the selected light palette.
- **Dark:** always use the selected dark palette.

Jolt ships three paired themes: Jolt, Catppuccin (Latte/Mocha), and Rosé Pine (Dawn/Rosé Pine). Light and dark variants can be selected independently, or both halves of a family can be selected together. Customizing a built-in creates an installation-level theme file containing complete light and dark palettes. The modal editor previews changes live and accepts `#RRGGBB` or `#RRGGBBAA` colors.

Font pickers enumerate installed fonts. The prompt family applies only to the chat composer. Independent size selectors control interface/prose, the prompt box, code blocks/diffs, and the terminal grid. Code font applies to code blocks, diffs, and hotkey chips; terminal font applies to the PTY grid.

### Notifications

Jolt defaults to in-app toasts. Enabling **System notifications** sends app-wide harness updates, Claude/Codex quota warnings, Jolt update notices, and app-wide errors through the OS instead.

## Default hotkeys

`Mod` means Command on macOS and Control elsewhere. Every app command hotkey is listed on the Hotkeys page and can be rebound. Standard text editing and control navigation keys follow platform conventions except that an open session reserves `Mod+Shift+Up/Down` for transcript navigation by default.

| Action | Default |
| --- | --- |
| New session | `Mod+N` |
| Clear input | `Mod+C` |
| Close current tab | `Mod+W` |
| Previous transcript prompt | `Mod+Shift+Up` |
| Next transcript prompt | `Mod+Shift+Down` |
| Open settings | `Mod+,` |
| Open spaces dropdown | `Mod+Shift+K` |
| Add space | `Mod+K` |
| Search sessions | `Mod+Shift+F` |
| Toggle left sidebar | `Mod+E` |
| Toggle Changes pane | `Mod+B` |
| Toggle terminal | ``Mod+` `` |
| New terminal tab (terminal focused) | `Mod+T` |
| Close terminal tab (terminal focused) | `Mod+Shift+W` |
| Select tabs 1–8 | `Mod+1` through `Mod+8` |
| Select last tab | `Mod+9` |

On macOS, the page also includes the native app and window hotkeys:

| Action | Default |
| --- | --- |
| Quit Jolt | `Cmd+Q` |
| Hide Jolt | `Cmd+H` |
| Hide other applications | `Option+Cmd+H` |
| Minimize window | `Cmd+M` |
| Close window | `Cmd+W` |

`Cmd+W` intentionally serves both Close current tab and Close window. In a focused terminal pane it closes only the active terminal tab; elsewhere in chat mode it closes only the device-local session tab. On an empty new-session canvas it does nothing. In Settings it falls through to the native window action. Archive remains an explicit session context-menu action. Transcript prompt navigation follows the complete message rail, loading historical pages when needed, and works even when the visual rail is hidden on a narrow layout. Developer builds also expose the Performance HUD hotkey (`Mod+Shift+F12`).

The Hotkeys page groups commands into collapsible Session actions, Tab switching, Navigation & layout, App & window, and Developer sections. Tab switching starts collapsed; the other available sections start expanded. The page records key combinations, detects duplicate assignments, resets one action, or restores defaults.

## User menu

The user menu includes:

- **Settings**
- **Archived sessions** — search and restore archived chats across devices
- **Usage breakdown** — 7-, 30-, or 90-day summaries merged from reachable devices
- **Check for update** or the current update state
- **Sign out**

Current-session context and token usage remains in the composer footer rather than settings.

## Persistence

Desktop viewport settings are stored in `{data_dir}/ui-settings.json`. They include pane sizes, sidebar state, the space filter, device-local open tab order and active tab, notification mode, keymap, appearance, selected light/dark theme IDs, font families and sizes, and terminal command.

Custom themes are stored individually in `{data_dir}/themes/<uuid>.json`. These files are global to the Jolt installation rather than Local/Account data scopes, and remain available after switching scope or signing out. While signed in, Jolt opportunistically reconciles only these files through the account registry; appearance mode, selected light/dark themes, and typography remain device-local. If the same theme was changed incompatibly on two hosts, Jolt retains the registry version and creates a named conflict copy of the local version instead of discarding either palette.

New-session composer defaults are stored separately in `{data_dir}/composer-defaults.json`. Corrupt or missing settings files fall back to defaults; numeric layout values are clamped on load.

The default data directory is `~/.jolt`. Set `JOLT_DATA_DIR` to isolate another installation.
