# Agent guide for webdav-sync-app

A small Tauri 2 + SvelteKit + Rust app that wraps `rclone` for safe one-way
local → WebDAV sync. Designed to be readable and modifiable by a human;
keep it that way.

## Safety prime directive (non-negotiable)

The app must never permanently delete user files — local or cloud. Concretely:

- **Never** add a code path that invokes `rclone delete`, `deletefile`,
  `purge`, `rmdir`, `cleanup`, or any non-`moveto` form of `move`. The
  `Subcommand` enum in `src-tauri/src/rclone.rs` is the **only** place
  subcommand names exist; adding a destructive variant is forbidden.
- **Never** add a UI affordance for "Empty garbage", "Permanent delete",
  or two-way mirror sync.
- **Never** widen `remove_rule` from metadata-only to file-touching.
- The unit test `allowlist_is_exactly_five_known_subcommands` enforces (1).
  Don't weaken it.

If a feature seems to require destructive ops, push back on the requirement
first.

## File map

```
src/routes/+page.svelte               All UI: rules list, rule editor, browser modal,
                                      restore modal, per-rule live log, autostart toggle.
src/routes/+layout.ts                 SvelteKit SPA marker (ssr=false).
src-tauri/src/lib.rs                  AppState; all `*_impl` functions and Tauri command
                                      wrappers; tray + autostart wiring; run_payload helpers.
src-tauri/src/store.rs                Rule struct + DeleteMode + Stats. JSON load/save in
                                      app_data_dir/rules.json.
src-tauri/src/rclone.rs               Subcommand allowlist enum, run / run_streaming,
                                      parse_counts, rclone_bin() lookup.
src-tauri/src/runner.rs               Per-rule tokio task: scheduler interval +
                                      notify-debouncer-mini watcher, cancellable via watch::Sender.
src-tauri/src/main.rs                 5-line binary entry — calls webdav_sync_app_lib::run().
src-tauri/tests/e2e.rs                Integration tests against a local docker WebDAV +
                                      tokio runner tests + idle-resource budget test.
src-tauri/icons/                      App + tray icons. SVG sources kept at
                                      /tmp/wsa-icon.svg and /tmp/wsa-tray.svg (regenerate
                                      with rsvg-convert + iconutil; see git log for the script).
src-tauri/Cargo.toml                  Pinned: tauri 2 (`tray-icon` feature), plugin-dialog,
                                      plugin-autostart, notify 6, notify-debouncer-mini 0.4,
                                      tokio (sync, time, rt, macros).
src-tauri/tauri.conf.json             Bundle config; identifier com.webdav-sync.app.
src-tauri/capabilities/default.json   `core:default`, `dialog:allow-open`, `autostart:default`.
package.json                          npm only (no pnpm/yarn). Tauri CLI + plugin-dialog +
                                      plugin-autostart + svelte-kit.
```

## Conventions

- **Minimal code over abstraction.** No SQLite, no keychain, no plugin
  unless it's load-bearing. JSON file > database. CLI subprocess >
  reimplemented protocol.
- **Tauri commands split into `*_impl` + thin `#[tauri::command]` wrapper.**
  The `*_impl` is the testable, sync, no-Tauri version that takes
  `&AppState`. The wrapper handles `tauri::State<'_, Arc<AppState>>` and
  event emission.
- **`AppState` is always wrapped in `Arc`** because runner tokio tasks
  outlive the request that started them. Tauri commands take
  `tauri::State<'_, Arc<AppState>>` and call `state.inner().clone()` to
  spawn.
- **`run_rule` is async / fire-and-forget.** It schedules `spawn_blocking`
  and emits `rule_running` / `rule_log` (per stderr line) / `rule_run`
  (final). The frontend never awaits sync completion through `invoke` —
  only via events.
- **Comments only when WHY isn't obvious** (e.g. why the runner does
  `tick.tick().await` once at startup). Don't write comments that
  paraphrase the code.
- **No emojis in source files.**
- **Don't add documentation files unless asked.** README.md and CLAUDE.md
  exist; expand them, don't create new ones.

## Architecture brief

```
        Frontend (Svelte 5 runes) -invoke()-> Tauri commands
                ^                                  |
                |                                  v
              listen()                         run_rule_impl_with_log
                |                                  |
                |          spawn_blocking          v
                +---- rule_running -------- rclone::run_streaming (subprocess)
                +---- rule_log (per stderr line) --|
                +---- rule_run (final summary) ----+

  AppState { data_dir, lock: Mutex<()>, runners: Runners }
                                          |
                                          v
                Runners (HashMap<rule.id, Handle { cancel: watch::Sender<bool> }>)
                                          |
                                          v
                          per-rule tauri::async_runtime::spawn:
                            select! {
                              tick.tick(), if interval.is_some()
                              fs_rx.recv(),                     // notify-debouncer-mini
                              cancel_rx.changed(),              // teardown
                            }
```

The `state.lock` mutex serialises every JSON read/write and every rclone
spawn — by design. Two manual *Run now* clicks queue rather than racing.

## Adding a new Tauri command

1. Write a pure `pub fn foo_impl(state: &AppState, …) -> Result<…, String>`
   in `lib.rs`.
2. Write a `#[tauri::command]` wrapper that takes
   `tauri::State<'_, Arc<AppState>>` and forwards. If it touches rclone,
   accept `app: AppHandle` so events can be emitted.
3. Add the wrapper to `invoke_handler![…]` inside `run()`.
4. Call from `+page.svelte` via `invoke<ReturnType>("foo", { snake_case_args })`.
5. Add an integration test in `tests/e2e.rs` that exercises the impl
   directly (it's `pub`, so tests can call it without going through Tauri).

## Adding a new rclone subcommand

1. Confirm it's read-only or move-only — never destructive. If destructive, **stop**.
2. Add a variant to `Subcommand` enum in `src-tauri/src/rclone.rs`. Update
   `as_arg()`.
3. Update `allowlist_is_exactly_five_known_subcommands` test to include it
   (and assert the still-forbidden names remain absent).
4. Confirm `run_inner` does the right thing for the new variant — in
   particular the `needs_stats` matcher (only Copy/Sync/MoveTo get the
   periodic stats flags).
5. Use it from `lib.rs` via `rclone::run` (collect-then-return) or
   `rclone::run_streaming` (line-by-line callback).

## Running tests

E2e tests need a local WebDAV server and an rclone remote called `dav:`:

```sh
docker run -d --name wsa-dav -p 8081:80 -e USERNAME=test -e PASSWORD=test bytemark/webdav
rclone config create dav webdav url=http://localhost:8081 vendor=other user=test pass="$(rclone obscure test)"
cd src-tauri && cargo test
```

The 6 e2e tests skip gracefully if `dav:` isn't configured (they print
`SKIPPING:` to stderr but pass). Runner-bearing tests use
`#[tokio::test(flavor = "multi_thread")]` and don't require `dav:`.

Current test count: **3 unit + 10 e2e = 13 total**, ~7 s runtime when `dav:`
is up.

## Build & verify

```sh
npm run check                                   # svelte-check
cd src-tauri && cargo test                      # 13 tests, must all pass
cd .. && npm run tauri build                    # produces .app + .dmg
"src-tauri/target/release/bundle/macos/WebDAV Sync.app/Contents/MacOS/webdav-sync-app"
                                                # smoke-launch and check stdout/stderr empty
```

Idle resource budget for the live app: **~80–90 MB RSS, 0% CPU**. 10 idle
runners must add < 50 MB RSS (in practice ~200 KB). The integration test
`idle_runners_are_resource_efficient` enforces both — if you blow these,
you've added polling.

## Common pitfalls

- **`tauri::include_image!` paths are relative to the crate root**
  (`src-tauri/`), not the source file. Use `"icons/foo.png"` not
  `"../icons/foo.png"`.
- **`Command::new("rclone")` fails for Finder-launched apps** because GUI
  apps inherit a stripped `PATH` (no `/opt/homebrew/bin`). `rclone_bin()`
  does explicit lookup; don't bypass it.
- **`garbage_path` must NOT be a child of `remote_path`.** `save_rule_impl`
  rejects this. `rclone sync --backup-dir` would otherwise re-trash already
  trashed files. Two e2e tests guard this.
- **Don't add `--size-only` blanket-style.** Mail.ru WebDAV reports a
  placeholder modtime which causes spurious re-uploads, but `--size-only`
  silently skips same-byte-count edits → silent data loss. If a per-server
  toggle is needed, make it per-rule with a clear UI affordance.
- **Use `tauri::async_runtime::spawn`, not `tokio::spawn`** for tasks that
  should also work inside Tauri's `setup` hook (where no tokio runtime is
  current). Tests using `#[tokio::test(flavor = "multi_thread")]` work
  with either.
- **Removing a rule must not touch files.** Part of the safety prime
  directive; the e2e test `req_5_remove_rule_is_metadata_only` enforces it.
- **The `state.lock` mutex is not just for `rules.json`** — it also
  serialises rclone spawns. Don't try to "optimise" by allowing parallel
  rclone runs; if you need that, add a per-rule lock instead of touching
  the global one.

## Out of scope (don't add)

- Live byte-progress percent (we already stream stderr; that's enough).
- "Empty garbage", purge, two-way mirror, permanent-delete UI.
- SQLite, OS-keychain plugins, system-tray notifications, autostart-minimised.
- Sidecar-bundled rclone (we require a system rclone — keeps the bundle
  small and avoids platform-specific binaries in the repo).
- Generic `--checksum` / `--size-only` / `--update` toggles unless added
  per-rule with a safety analysis.

## When in doubt

Read the existing test in `tests/e2e.rs` that covers the closest behaviour.
Add a new test before adding the feature.
