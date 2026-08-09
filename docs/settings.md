# Settings

Open settings from the user menu or with `Cmd+,` on macOS / `Ctrl+,` elsewhere. Unless noted, preferences stay on the current device.

## Pages

| Page | What it controls | Scope |
| --- | --- | --- |
| **Appearance** | System/light/dark mode, paired themes, custom colors, and typography | Local viewport setting; custom theme files are installation-level |
| **Notifications** | In-app toasts versus operating-system notifications | Local viewport setting |
| **Hotkeys** | Rebindable app commands with conflict detection and reset controls | Local viewport setting |
| **Accounts** | Claude Code and Codex login slots, activation, removal, and provider quota meters | Selected engine device |
| **Secrets** | Write-only environment secrets scoped to Claude Code, Codex, and/or Pi | Local engine only |
| **Devices** | Background availability plus registered-device presence, version, rename/removal, and copyable IDs | Service setting is local; device rows sync |
| **Version control** | Active Git or Jujutsu command-line backend and executable status | Selected engine device |
| **Terminal** | Command used when a new terminal tab opens | Selected engine device |

The sidebar groups these pages as **Preferences** (Appearance, Notifications, Hotkeys), **Agents** (Accounts, Secrets), and **System** (Devices, Version control, Terminal). Settings opens to Appearance by default.

Removing a device tombstones its spaces and threads. Synced R2 backups and attachments are purged asynchronously; folders and other local files on that machine are unaffected. If Jolt later starts there again, it registers as an empty device.

### Devices

Enable **Keep this device available** to install and start Jolt's per-user background engine. The app restarts to hand engine ownership over safely. On macOS this uses a launchd LaunchAgent; on Linux it uses a systemd user service. Disabling it restarts Jolt with an embedded engine instead. The setting affects only the device where it is changed.

Reachable remote engine devices report their Jolt release status on this page. Managed installations with an available release show an explicit **Update** action; offline devices disable it, and unmanaged installations show **Manual update**. The row tracks waiting, restart, verification, and failure states. Background checks never install a release automatically.

### Accounts

Account switching changes credentials on the selected device, not across the fleet. Jolt supports account-slot management for Claude Code and Codex. Pi provider authentication stays in Pi and is configured with Pi's `/login`.

Quota meters reflect provider/CLI rate-limit windows. They turn warning at 80% and critical at 95%, with reset time when reported. These meters are separate from Jolt's thread token ledger.

### Secrets

Secret values are never returned after creation. The page lists only saved metadata; **Add secret** opens a focused form for the label, environment variable, value, and permitted harnesses. Metadata is stored in `harness-secrets.json`; values stay in the operating-system credential store and are injected only into selected harness child processes. Secret RPC methods are unavailable through the device relay.

See [Security and data](security.md).

### Version control

Jolt prefers Jujutsu 0.43 or newer when available and no backend choice has been saved, then falls back to Git. The selection is per engine device. A device switcher at the top of the page lets a viewport configure a reachable remote engine.

### Terminal

Leave **Launch command** blank to open the default interactive login shell. A custom command runs through the login shell in the thread directory and applies only to terminals opened after the change. The command is stored on the selected engine device; use the device switcher to configure each reachable device independently.

### Appearance

- **System:** follow OS light/dark changes.
- **Light:** always use the selected light palette.
- **Dark:** always use the selected dark palette.

Jolt ships three paired themes: Jolt, Catppuccin (Latte/Mocha), and Rosé Pine (Dawn/Rosé Pine). Each theme card exposes its light and dark variants directly, shows where the family is currently used, and can apply both variants together. **Customize** opens the theme editor; customizing a built-in creates an installation-level theme file containing complete light and dark palettes. The editor previews changes live and accepts `#RRGGBB` or `#RRGGBBAA` colors.

Font pickers enumerate installed fonts. The composer family applies only to the prompt box. Code font applies to code blocks, diffs, and hotkey chips; terminal font applies to the PTY grid.

### Notifications

Jolt defaults to in-app toasts. Enabling **System notifications** sends app-wide harness updates, Claude/Codex quota warnings, Jolt update notices, and app-wide errors through the OS instead.

## Default hotkeys

`Mod` means Command on macOS and Control elsewhere. Every app command hotkey is listed on the Hotkeys page and can be rebound. Standard text editing and control navigation keys follow platform conventions except that an open thread reserves `Mod+Shift+Up/Down` for transcript navigation by default.

| Action | Default |
| --- | --- |
| New thread | `Mod+N` |
| Clear input | `Mod+C` |
| Previous transcript prompt | `Mod+Shift+Up` |
| Next transcript prompt | `Mod+Shift+Down` |
| Open settings | `Mod+,` |
| Open spaces dropdown | `Mod+Shift+K` |
| Add space | `Mod+K` |
| Search threads | `Mod+Shift+F` |
| Toggle left sidebar | `Mod+E` |
| Toggle Changes pane | `Mod+B` |
| Toggle terminal | ``Mod+` `` |
| New terminal tab (terminal focused) | `Mod+T` |
| Close terminal tab (terminal focused) | `Mod+Shift+W` |
| Select sidebar threads 1–9 | `Mod+1` through `Mod+9` |

On macOS, the page also includes the native app and window hotkeys:

| Action | Default |
| --- | --- |
| Quit Jolt | `Cmd+Q` |
| Hide Jolt | `Cmd+H` |
| Hide other applications | `Option+Cmd+H` |
| Minimize window | `Cmd+M` |
| Close window | `Cmd+W` |

`Cmd+W` closes the current window. Closing a terminal tab remains a separate `Mod+Shift+W` action while the terminal is focused. Close remains an explicit thread context-menu action and is unavailable while a run is active. `Mod+1` through `Mod+9` select the corresponding row in the active, filtered Threads sidebar order. Transcript prompt navigation follows the complete message rail, loading historical pages when needed, and works even when the visual rail is hidden on a narrow layout. Developer builds also expose the Performance HUD hotkey (`Mod+Shift+F12`).

The Hotkeys page groups commands into collapsible Thread actions, Thread switching, Navigation & layout, App & window, and Developer sections. Thread switching starts collapsed; the other available sections start expanded. The page records key combinations, detects duplicate assignments, resets one action, or restores defaults.

## User menu

The user menu includes:

- **Settings**
- **Usage breakdown** — 7-, 30-, or 90-day summaries merged from reachable devices
- **Check for update** or the current update state
- **Sign out**

Current-thread context and token usage remains beside the composer’s separate model and traits controls rather than in settings.

## Persistence

Desktop viewport settings are stored in `{data_dir}/ui-settings.json`. They include pane sizes, sidebar state, the per-scope space filter and last active space, notification mode, keymap, appearance, selected light/dark theme IDs, and font families and sizes.

The device-specific terminal launch command is stored in `{data_dir}/terminal-settings.json` on each engine device.

Custom themes are stored individually in `{data_dir}/themes/<uuid>.json`. These files are global to the Jolt installation rather than Local/Account data scopes, and remain available after switching scope or signing out. While signed in, Jolt opportunistically reconciles only these files through the account registry; appearance mode, selected light/dark themes, and typography remain device-local. If the same theme was changed incompatibly on two hosts, Jolt retains the registry version and creates a named conflict copy of the local version instead of discarding either palette.

New-thread composer defaults are stored separately in `{data_dir}/composer-defaults.json`. Corrupt or missing settings files fall back to defaults; numeric layout values are clamped on load.

The default data directory is `~/.jolt`. Set `JOLT_DATA_DIR` to isolate another installation.
