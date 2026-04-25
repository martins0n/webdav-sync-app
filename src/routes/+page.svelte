<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import { listen } from "@tauri-apps/api/event";
  import {
    isEnabled as isAutostartEnabled,
    enable as enableAutostart,
    disable as disableAutostart,
  } from "@tauri-apps/plugin-autostart";
  import { onMount, onDestroy } from "svelte";

  type Mode = "safe" | "trash";

  type Stats = { synced: number; deleted: number; restored: number };

  type Rule = {
    id: string;
    name: string;
    local_path: string;
    remote: string;
    remote_path: string;
    delete_mode: Mode;
    garbage_path: string;
    interval_seconds: number | null;
    watch: boolean;
    enabled: boolean;
    stats: Stats;
    last_run_at: string | null;
    last_status: string | null;
  };

  type RunResult = {
    synced: number;
    moved_to_garbage: number;
    hard_deleted: number;
    success: boolean;
    log_tail: string;
  };

  type GarbageItem = { path: string; size: number; is_dir: boolean; mod_time: string };

  let rules = $state<Rule[]>([]);
  let remotes = $state<string[]>([]);
  let editing = $state<Rule | null>(null);
  let running = $state<Record<string, boolean>>({});
  let logs = $state<Record<string, string[]>>({});
  const LOG_BUFFER = 200;
  let autoStart = $state(false);
  let restoreFor = $state<Rule | null>(null);
  let garbage = $state<GarbageItem[]>([]);
  let error = $state<string>("");
  let browser = $state<{ target: "remote_path" | "garbage_path"; path: string; dirs: string[]; loading: boolean } | null>(null);

  function blank(): Rule {
    return {
      id: "",
      name: "",
      local_path: "",
      remote: remotes[0] ?? "",
      remote_path: "",
      delete_mode: "safe",
      garbage_path: "",
      interval_seconds: null,
      watch: false,
      enabled: true,
      stats: { synced: 0, deleted: 0, restored: 0 },
      last_run_at: null,
      last_status: null,
    };
  }

  async function refresh() {
    error = "";
    try {
      [rules, remotes] = await Promise.all([
        invoke<Rule[]>("list_rules"),
        invoke<string[]>("list_remotes"),
      ]);
    } catch (e) {
      error = String(e);
    }
  }

  async function pickFolder() {
    if (!editing) return;
    const dir = await openDialog({ directory: true, multiple: false });
    if (typeof dir === "string") editing.local_path = dir;
  }

  async function openRemoteBrowser(target: "remote_path" | "garbage_path") {
    if (!editing) return;
    const startPath = (target === "remote_path" ? editing.remote_path : editing.garbage_path) || "";
    browser = { target, path: startPath, dirs: [], loading: true };
    await loadRemoteDirs();
  }

  async function loadRemoteDirs() {
    if (!browser || !editing) return;
    browser.loading = true;
    try {
      browser.dirs = await invoke<string[]>("list_remote_dirs", {
        remote: editing.remote,
        path: browser.path,
      });
    } catch (e) {
      error = String(e);
      browser.dirs = [];
    } finally {
      browser.loading = false;
    }
  }

  function browserGoInto(child: string) {
    if (!browser) return;
    browser.path = browser.path ? `${browser.path}/${child}` : child;
    loadRemoteDirs();
  }

  function browserGoUp() {
    if (!browser) return;
    const i = browser.path.lastIndexOf("/");
    browser.path = i < 0 ? "" : browser.path.slice(0, i);
    loadRemoteDirs();
  }

  function browserSelect() {
    if (!browser || !editing) return;
    if (browser.target === "remote_path") editing.remote_path = browser.path;
    else editing.garbage_path = browser.path;
    browser = null;
  }

  async function save() {
    if (!editing) return;
    error = "";
    try {
      await invoke<Rule>("save_rule", { rule: editing });
      editing = null;
      await refresh();
    } catch (e) {
      error = String(e);
    }
  }

  async function run(rule: Rule) {
    running[rule.id] = true;
    error = "";
    try {
      // Fire-and-forget — outcome arrives via the rule_run event below.
      await invoke("run_rule", { id: rule.id });
    } catch (e) {
      error = String(e);
      running[rule.id] = false;
    }
  }

  async function remove(rule: Rule) {
    if (!confirm(`Remove rule "${rule.name}"?\n\nThis only removes the rule from the app. No files will be deleted, locally or in the cloud (the rule's garbage folder remains intact).`))
      return;
    try {
      await invoke("remove_rule", { id: rule.id });
      await refresh();
    } catch (e) {
      error = String(e);
    }
  }

  async function openRestore(rule: Rule) {
    restoreFor = rule;
    garbage = [];
    error = "";
    try {
      garbage = await invoke<GarbageItem[]>("list_garbage", { id: rule.id });
    } catch (e) {
      error = String(e);
    }
  }

  async function restore(item: GarbageItem) {
    if (!restoreFor) return;
    try {
      await invoke("restore_file", { id: restoreFor.id, garbageSubpath: item.path });
      garbage = await invoke<GarbageItem[]>("list_garbage", { id: restoreFor.id });
      await refresh();
    } catch (e) {
      error = String(e);
    }
  }

  function fmtSize(n: number) {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / 1024 / 1024).toFixed(1)} MB`;
  }

  let unlistenRun: (() => void) | undefined;
  let unlistenRunning: (() => void) | undefined;
  let unlistenLog: (() => void) | undefined;

  async function toggleAutoStart(e: Event) {
    const target = e.target as HTMLInputElement;
    const desired = target.checked;
    try {
      if (desired) await enableAutostart();
      else await disableAutostart();
      autoStart = desired;
    } catch (err) {
      error = String(err);
      target.checked = !desired;
    }
  }

  onMount(async () => {
    try {
      autoStart = await isAutostartEnabled();
    } catch {
      // plugin unavailable in this build — fine.
    }
    await refresh();
    unlistenRunning = await listen<{ id: string }>("rule_running", (e) => {
      running[e.payload.id] = true;
      // Reset the live log so a fresh run starts clean.
      logs[e.payload.id] = [];
    });
    unlistenLog = await listen<{ id: string; line: string }>("rule_log", (e) => {
      const { id, line } = e.payload;
      const next = [...(logs[id] ?? []), line];
      if (next.length > LOG_BUFFER) next.splice(0, next.length - LOG_BUFFER);
      logs[id] = next;
    });
    unlistenRun = await listen<{ id: string; result?: RunResult; error?: string }>(
      "rule_run",
      (e) => {
        running[e.payload.id] = false;
        refresh();
      },
    );
  });

  onDestroy(() => {
    unlistenRun?.();
    unlistenRunning?.();
    unlistenLog?.();
  });
</script>

<main>
  <header>
    <h1>WebDAV Sync</h1>
    <div class="header-actions">
      <label class="check header-toggle">
        <input type="checkbox" checked={autoStart} onchange={toggleAutoStart} />
        <span>Start at login</span>
      </label>
      <button onclick={() => (editing = blank())} disabled={remotes.length === 0}>
        + New rule
      </button>
    </div>
  </header>

  {#if remotes.length === 0}
    <div class="hint">
      No rclone remotes found. Open a terminal and run <code>rclone config</code> to create a WebDAV remote, then click reload.
      <button onclick={refresh}>Reload</button>
    </div>
  {/if}

  {#if error}
    <pre class="error">{error}</pre>
  {/if}

  {#if rules.length === 0 && remotes.length > 0}
    <p class="empty">No rules yet. Click <strong>+ New rule</strong> to create one.</p>
  {/if}

  {#each rules as rule (rule.id)}
    <article class="rule">
      <div class="rule-head">
        <div>
          <strong>{rule.name}</strong>
          <span class="mode mode-{rule.delete_mode}">{rule.delete_mode}</span>
          {#if !rule.enabled}<span class="mode mode-disabled">disabled</span>{/if}
          {#if rule.enabled && rule.interval_seconds}
            <span class="mode mode-auto">every {rule.interval_seconds}s</span>
          {/if}
          {#if rule.enabled && rule.watch}<span class="mode mode-auto">watching</span>{/if}
        </div>
        <div class="actions">
          <button onclick={() => run(rule)} disabled={!!running[rule.id]}>
            {#if running[rule.id]}<span class="spinner"></span>Running…{:else}Run now{/if}
          </button>
          <button onclick={() => (editing = { ...rule })}>Edit</button>
          {#if rule.delete_mode === "trash"}
            <button onclick={() => openRestore(rule)}>Restore…</button>
          {/if}
          <button class="danger" onclick={() => remove(rule)}>Remove</button>
        </div>
      </div>
      <div class="rule-body">
        <div><span class="lbl">Local</span> <code>{rule.local_path}</code></div>
        <div>
          <span class="lbl">Cloud</span>
          <code>{rule.remote}:{rule.remote_path}</code>
          {#if rule.delete_mode === "trash"}
            <span class="lbl">Garbage</span>
            <code>{rule.remote}:{rule.garbage_path}</code>
          {/if}
        </div>
        <div class="stats">
          <span>synced <strong>{rule.stats.synced}</strong></span>
          {#if rule.delete_mode === "trash"}
            <span>moved-to-garbage <strong>{rule.stats.deleted}</strong></span>
            <span>restored <strong>{rule.stats.restored}</strong></span>
          {/if}
          {#if rule.last_run_at}
            <span class="last">last run {new Date(rule.last_run_at).toLocaleString()}</span>
          {/if}
          {#if rule.last_status}
            <span class="status" class:ok={rule.last_status === "ok"}>
              {rule.last_status}
            </span>
          {/if}
        </div>
        {#if (logs[rule.id] ?? []).length > 0}
          <details class="rule-log-details">
            <summary>
              Live log
              <span class="muted">({(logs[rule.id] ?? []).length} lines{running[rule.id] ? ", streaming…" : ""})</span>
            </summary>
            <pre class="rule-log">{(logs[rule.id] ?? []).slice(-50).join("\n")}</pre>
          </details>
        {/if}
      </div>
    </article>
  {/each}

  {#if editing}
    <div class="modal-bg" role="presentation" onclick={() => (editing = null)}></div>
    <form class="modal" onsubmit={(e) => { e.preventDefault(); save(); }}>
      <h2>{editing.id ? "Edit rule" : "New rule"}</h2>

      <label>Name<input bind:value={editing.name} required /></label>

      <label>Local folder
        <div class="row">
          <input bind:value={editing.local_path} required />
          <button type="button" onclick={pickFolder}>Browse…</button>
        </div>
      </label>

      <label>Remote
        <select bind:value={editing.remote}>
          {#each remotes as r}<option value={r}>{r}:</option>{/each}
        </select>
      </label>

      <label>Remote path
        <div class="row">
          <input bind:value={editing.remote_path} placeholder="Documents" required />
          <button type="button" onclick={() => openRemoteBrowser("remote_path")}>Browse…</button>
        </div>
      </label>

      <fieldset>
        <legend>Deletion behavior</legend>
        <label class="radio">
          <input type="radio" bind:group={editing.delete_mode} value="safe" />
          <div>
            <strong>Safe</strong> — local deletes are <em>not</em> propagated; cloud copy is left untouched.
          </div>
        </label>
        <label class="radio">
          <input type="radio" bind:group={editing.delete_mode} value="trash" />
          <div>
            <strong>Trash</strong> — when a local file is deleted, the cloud copy is <em>moved</em> to a garbage folder for restore.
          </div>
        </label>
      </fieldset>

      {#if editing.delete_mode === "trash"}
        <label>Garbage path
          <div class="row">
            <input bind:value={editing.garbage_path} placeholder="Documents-garbage" required />
            <button type="button" onclick={() => openRemoteBrowser("garbage_path")}>Browse…</button>
          </div>
          <small>Must NOT be inside the remote path. Use a sibling, e.g. <code>{editing.remote_path || "X"}-garbage</code>.</small>
        </label>
      {:else}
        <input type="hidden" bind:value={editing.garbage_path} />
      {/if}

      <fieldset>
        <legend>Automation</legend>
        <label class="check">
          <input type="checkbox" bind:checked={editing.enabled} />
          <span>Enabled — schedule and watcher run only when this is on. <em>Run now</em> always works.</span>
        </label>
        <label>Run every (seconds, leave empty for manual only)
          <input
            type="number"
            min="0"
            placeholder="e.g. 300 for every 5 minutes"
            value={editing.interval_seconds ?? ""}
            oninput={(e) => {
              const v = (e.target as HTMLInputElement).value;
              editing!.interval_seconds = v === "" ? null : Math.max(0, parseInt(v, 10) || 0);
            }}
          />
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={editing.watch} />
          <span>Watch the local folder and re-sync on file changes (debounced 2s).</span>
        </label>
      </fieldset>

      <div class="row right">
        <button type="button" onclick={() => (editing = null)}>Cancel</button>
        <button type="submit">Save</button>
      </div>
    </form>
  {/if}

  {#if browser && editing}
    <div class="modal-bg" role="presentation" onclick={() => (browser = null)}></div>
    <div class="modal">
      <h2>Browse {editing.remote}:</h2>
      <div class="crumbs">
        <code>{editing.remote}:{browser.path || "/"}</code>
      </div>
      <div class="row">
        <button type="button" onclick={browserGoUp} disabled={!browser.path}>↑ Up</button>
        <button type="button" onclick={browserSelect}>
          Use <code>{browser.path || "(root)"}</code>
        </button>
        <span style="flex:1"></span>
        <button type="button" onclick={() => (browser = null)}>Cancel</button>
      </div>
      {#if browser.loading}
        <p class="empty">Loading…</p>
      {:else if browser.dirs.length === 0}
        <p class="empty">No subfolders here. Click “Use …” to select this path, or type a name into the field manually to create a new one on first sync.</p>
      {:else}
        <ul class="dirs">
          {#each browser.dirs as d}
            <li>
              <button type="button" onclick={() => browserGoInto(d)}>📁 {d}</button>
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}

  {#if restoreFor}
    <div class="modal-bg" role="presentation" onclick={() => (restoreFor = null)}></div>
    <div class="modal">
      <h2>Restore from garbage — {restoreFor.name}</h2>
      {#if garbage.length === 0}
        <p>Nothing in garbage.</p>
      {:else}
        <table>
          <thead><tr><th>Path</th><th>Size</th><th>Modified</th><th></th></tr></thead>
          <tbody>
            {#each garbage as item}
              <tr>
                <td><code>{item.path}</code></td>
                <td>{fmtSize(item.size)}</td>
                <td>{new Date(item.mod_time).toLocaleString()}</td>
                <td><button onclick={() => restore(item)}>Restore</button></td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
      <div class="row right">
        <button onclick={() => (restoreFor = null)}>Close</button>
      </div>
    </div>
  {/if}
</main>

<style>
  :global(body) {
    margin: 0;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
    background: #f6f6f7;
    color: #1a1a1a;
  }
  main { max-width: 880px; margin: 0 auto; padding: 24px; }
  header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 16px; }
  h1 { margin: 0; font-size: 22px; }
  h2 { margin: 0 0 12px; font-size: 18px; }
  button { padding: 6px 12px; border: 1px solid #c8c8cc; background: #fff; border-radius: 6px; cursor: pointer; font-size: 13px; }
  button:disabled { opacity: 0.5; cursor: default; }
  button:hover:not(:disabled) { background: #efeff2; }
  button.danger { color: #b00020; border-color: #e3b3bb; }
  input, select { padding: 6px 8px; border: 1px solid #c8c8cc; border-radius: 6px; font-size: 13px; width: 100%; box-sizing: border-box; background: #fff; }
  label { display: block; margin: 10px 0; font-size: 13px; color: #555; }
  label > input, label > select { margin-top: 4px; }
  small { display: block; color: #666; margin-top: 4px; }
  code { background: #ececef; padding: 1px 5px; border-radius: 4px; font-size: 12px; }
  .row { display: flex; gap: 8px; align-items: center; }
  .row.right { justify-content: flex-end; margin-top: 16px; }
  .hint { padding: 12px; background: #fff8e1; border: 1px solid #f4d35e; border-radius: 6px; margin-bottom: 16px; display: flex; gap: 12px; align-items: center; }
  .empty { color: #888; }
  .error { background: #fdecea; border: 1px solid #f5c2bd; padding: 10px; border-radius: 6px; white-space: pre-wrap; font-family: ui-monospace, monospace; font-size: 12px; }
  .rule { background: #fff; border: 1px solid #e2e2e7; border-radius: 8px; padding: 14px; margin-bottom: 12px; }
  .rule-head { display: flex; justify-content: space-between; align-items: center; margin-bottom: 8px; }
  .actions { display: flex; gap: 6px; flex-wrap: wrap; }
  .rule-body > div { margin: 4px 0; font-size: 13px; }
  .lbl { display: inline-block; min-width: 60px; color: #888; font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em; }
  .stats { display: flex; gap: 14px; flex-wrap: wrap; margin-top: 8px; font-size: 12px; color: #555; }
  .stats strong { color: #1a1a1a; }
  .stats .status { color: #b00020; }
  .stats .status.ok { color: #2e7d32; }
  .stats .last { color: #888; }
  .mode { display: inline-block; padding: 1px 7px; margin-left: 8px; border-radius: 10px; font-size: 11px; font-weight: 500; vertical-align: 1px; }
  .mode-safe { background: #e3f2fd; color: #1565c0; }
  .mode-trash { background: #fff3e0; color: #ef6c00; }
  .mode-disabled { background: #ececef; color: #777; }
  .mode-auto { background: #e8f5e9; color: #2e7d32; }
  .check { display: flex; gap: 8px; align-items: flex-start; }
  .check input { width: auto; margin-top: 3px; }
  .header-actions { display: flex; gap: 14px; align-items: center; }
  .header-toggle { font-size: 12px; color: #555; gap: 6px; }
  .header-toggle input { margin-top: 0; }
  .crumbs { padding: 8px 10px; background: #f0f0f3; border-radius: 6px; margin-bottom: 8px; word-break: break-all; }
  ul.dirs { list-style: none; padding: 0; margin: 12px 0 0; max-height: 360px; overflow: auto; border: 1px solid #e2e2e7; border-radius: 6px; }
  ul.dirs li { border-bottom: 1px solid #f0f0f3; }
  ul.dirs li:last-child { border-bottom: 0; }
  ul.dirs button { width: 100%; text-align: left; border: 0; border-radius: 0; background: transparent; padding: 8px 12px; cursor: pointer; }
  ul.dirs button:hover { background: #f5f5f7; }
  .spinner {
    display: inline-block;
    width: 11px;
    height: 11px;
    border: 2px solid currentColor;
    border-top-color: transparent;
    border-radius: 50%;
    margin-right: 6px;
    animation: spin 0.8s linear infinite;
    vertical-align: -1px;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
  .rule-log-details { margin-top: 10px; font-size: 12px; }
  .rule-log-details summary { cursor: pointer; color: #555; }
  .rule-log-details .muted { color: #999; font-weight: 400; }
  .rule-log {
    background: #1f1f23; color: #eaeaea;
    padding: 10px 12px; border-radius: 6px;
    margin-top: 8px;
    max-height: 280px; overflow: auto;
    font-family: ui-monospace, monospace; font-size: 11.5px;
    white-space: pre-wrap; word-break: break-all;
  }
  fieldset { border: 1px solid #d8d8de; border-radius: 6px; padding: 8px 12px; margin: 12px 0; }
  legend { font-size: 13px; color: #555; padding: 0 4px; }
  .radio { display: flex; gap: 10px; align-items: flex-start; cursor: pointer; }
  .radio input { width: auto; margin-top: 3px; }
  details { margin-top: 16px; }
  details pre { background: #1f1f23; color: #eaeaea; padding: 10px; border-radius: 6px; max-height: 300px; overflow: auto; font-size: 11.5px; }
  .modal-bg { position: fixed; inset: 0; background: rgba(0,0,0,0.4); z-index: 1; }
  .modal { position: fixed; left: 50%; top: 50%; transform: translate(-50%, -50%); width: min(560px, 92vw); max-height: 86vh; overflow: auto; background: #fff; padding: 20px; border-radius: 10px; box-shadow: 0 10px 40px rgba(0,0,0,0.25); z-index: 2; }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th, td { text-align: left; padding: 6px 8px; border-bottom: 1px solid #eee; }
  th { font-weight: 500; color: #666; font-size: 12px; }
</style>
