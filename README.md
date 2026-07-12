# WebDAV Sync (Ωδ)

<img src="src-tauri/icons/icon.png" alt="WebDAV Sync icon" width="128" align="right"/>

Minimal, **safe** one-way sync from a local folder to a WebDAV cloud, with a
restorable trash folder on the cloud side. macOS-first, cross-platform via
Tauri 2 + SvelteKit on top of [rclone](https://rclone.org/).

> ⚠️ **Experimental.** Built quickly, not battle-tested. Use it on data you
> can afford to lose, keep an independent backup, and report anything
> surprising via issues.

> **Safety prime directive.** The app never permanently deletes files —
> local or cloud. The worst case is "moved to a `garbage/<timestamp>/…`
> folder on the cloud", which you can restore from in one click.

## Features

- One-way local → WebDAV upload (rclone copy / sync under the hood).
- Per-rule deletion behaviour:
  - **Safe** — local deletes are not propagated to the cloud.
  - **Trash** — the cloud copy is *moved* to a garbage folder when the local
    file disappears, restorable from inside the app.
- Per-rule scheduler (every N seconds) and live filesystem watcher
  (debounced 2 s).
- Per-rule **file filters** — either *ignore matching* files (e.g.
  `*.rrdata`, dotfiles) or *only sync matching* files (e.g. only `*.ARW`
  raws), using [rclone patterns](https://rclone.org/filtering/).
- Live rclone log streamed into a per-rule expandable panel during sync.
- **Freeze sync** (Amphetamine-style) — pause all scheduled and watched
  syncs for a stackable +1h (up to 24h); *Run now* still works and a
  catch-up sync runs when the freeze lifts. The tray icon shows a snowflake
  badge while frozen.
- macOS menu-bar tray icon (Show / Run all / Freeze / Resume / Quit),
  close-to-tray.
- "Start at login" toggle.
- Built-in remote folder browser when picking a path on the cloud.

## Install

### Prerequisites

- macOS 12+ (Linux & Windows should work — only macOS is regularly tested).
- [rclone](https://rclone.org/install/) on `PATH`. macOS:

  ```sh
  brew install rclone
  ```

- An rclone remote already configured (see below).

### Build from source

```sh
git clone <this-repo>
cd webdav-sync-app
npm install
npm run tauri build
```

The `.app` lands at:

```text
src-tauri/target/release/bundle/macos/WebDAV Sync.app
```

Drag it to `/Applications` and double-click to launch.

## Configure your WebDAV remote

The app **does not** store WebDAV credentials of its own — it lists the
remotes already configured in `~/.config/rclone/rclone.conf` and lets you
pick one when you create a rule. Set one up once:

```sh
rclone config create mycloud webdav \
  url=https://your-webdav-host \
  vendor=other \
  user=YOUR_USER \
  pass="$(rclone obscure YOUR_APP_PASSWORD)"

rclone lsd mycloud:    # smoke-test — should list your folders
```

Hints for common providers:

| Provider | `url` | `vendor` |
| --- | --- | --- |
| Mail.ru Cloud | `https://webdav.cloud.mail.ru` | `other` (use a Mail.ru *application password*, not your account password) |
| Nextcloud | `https://your-host/remote.php/dav/files/USERNAME/` | `nextcloud` |
| ownCloud | `https://your-host/remote.php/dav/files/USERNAME/` | `owncloud` |
| Other | (your URL) | `other` |

## Use the app

1. Launch **WebDAV Sync.app**. The first run shows an empty rules list.
2. Click **+ New rule** and fill in:
   - **Name** — any label.
   - **Local folder** — pick via the *Browse…* button.
   - **Remote** — your rclone remote (e.g. `mycloud`) from the dropdown.
   - **Remote path** — type or *Browse…* to pick from existing folders.
   - **Deletion behavior** — *Safe* (default) or *Trash*.
   - **Garbage path** (Trash mode only) — must be a sibling of *Remote
     path*, not nested inside it.
   - **Automation** — leave manual at first; later add an interval and/or
     enable *Watch*.
3. Click **Save**, drop a file in your local folder, click **Run now**.
4. Expand the **Live log** disclosure under the rule card to see rclone's
   output stream in real time.
5. To restore a deleted file (Trash-mode rules only): click **Restore…**,
   pick a file from the listed garbage entries, click **Restore**.

## File filters

Each rule can narrow *which* files it syncs, via the **File filters** section
of the rule editor. Pick a mode and list one
[rclone pattern](https://rclone.org/filtering/) per line:

- **Ignore matching** — sync everything *except* the listed patterns. Good
  for junk like `*.rrdata`, `.*` (dotfiles), or `node_modules/**`.
- **Only sync matching** — sync *only* the listed patterns and ignore the
  rest. Good for "just the RAWs": `*.ARW`.

Patterns match against each file's path: `*.ARW` (by extension), `.*`
(dotfiles), `node_modules/**` (a whole folder). Leave the box empty to sync
everything.

Filters only *narrow* what a rule touches — they never delete. A skipped
file is simply not uploaded, and in Trash mode any matching-but-skipped file
already on the cloud is left untouched.

## Freeze sync

Copying a batch of photos and want to reorganise them before they upload?
Click **Freeze sync 1h** — in the window header or the menu-bar tray — to
pause every scheduled and watched sync. Each click adds another hour
(stacking up to 24 h); the header shows a live countdown. **Run now** is
never blocked, so you can still push on demand.

While frozen, the tray icon wears a small snowflake badge. When the freeze
lifts — via **Resume** or when the timer runs out — any change made during
the freeze is uploaded in a single catch-up sync, so the cloud matches your
final, settled folder rather than every intermediate state. The freeze is
in-memory only: quitting the app clears it.

## Tray icon and autostart

The macOS menu-bar **Ωδ** icon shows menu items: *Show window*, *Run all
rules now*, *Freeze sync 1h*, *Resume sync*, *Quit*. While sync is frozen
the icon carries a small snowflake badge. Closing the window via the red
close button hides it and the app keeps running with the tray icon — only
**Quit** from the tray fully exits.

The "Start at login" header toggle registers a macOS LaunchAgent at
`~/Library/LaunchAgents/com.webdav-sync.app.plist`.

The app is **single-instance**: if macOS tries to launch it a second time —
e.g. the login LaunchAgent plus session restore both firing after a reboot —
the second launch just focuses the existing window and exits, so you never
end up with two tray icons or two sets of runners.

## Where things live

- Rules + per-rule counters: `~/Library/Application Support/com.webdav-sync.app/rules.json`
- rclone config (managed by rclone): `~/.config/rclone/rclone.conf`
- LaunchAgent (when autostart is on): `~/Library/LaunchAgents/com.webdav-sync.app.plist`

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| `failed to spawn rclone (No such file or directory)` | Install rclone (`brew install rclone`). The app looks in `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, then `$PATH`. Override with `WEBDAV_SYNC_RCLONE_BIN=/path/to/rclone open …`. |
| Window doesn't appear after close | Click the **Ωδ** tray icon → *Show window*. |
| Files keep re-uploading every run | Your WebDAV server probably reports a placeholder modtime. File an issue describing your provider; a per-rule "size-only" toggle is the planned fix. |
| `garbage_path must NOT be inside remote_path` | Use a sibling path, e.g. `Documents` and `Documents-garbage`. |

## License

MIT.
