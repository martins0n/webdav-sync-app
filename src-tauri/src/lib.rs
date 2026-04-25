mod rclone;
mod runner;
mod store;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

pub use rclone::{RunCounts, Subcommand};
pub use runner::Runners;
pub use store::{DeleteMode, Rule, Stats};

pub struct AppState {
    pub data_dir: PathBuf,
    lock: Mutex<()>,
    pub runners: Runners,
}

impl AppState {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            lock: Mutex::new(()),
            runners: Runners::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RunResult {
    pub synced: u64,
    pub moved_to_garbage: u64,
    pub hard_deleted: u64,
    pub success: bool,
    pub log_tail: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GarbageItem {
    pub path: String,
    pub size: i64,
    pub is_dir: bool,
    pub mod_time: String,
}

// ---------- impls (callable from integration tests) ----------

pub fn list_remotes_impl() -> Result<Vec<String>, String> {
    let out = rclone::run(Subcommand::ListRemotes, &[])?;
    if !out.success {
        return Err(out.stderr);
    }
    Ok(out
        .stdout
        .lines()
        .map(|l| l.trim().trim_end_matches(':').to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

pub fn list_rules_impl(state: &AppState) -> Vec<Rule> {
    let _g = state.lock.lock().unwrap();
    store::load(&state.data_dir)
}

pub fn save_rule_impl(state: &AppState, mut rule: Rule) -> Result<Rule, String> {
    if rule.local_path.trim().is_empty()
        || rule.remote.trim().is_empty()
        || rule.remote_path.trim().is_empty()
    {
        return Err("local_path, remote and remote_path are required".into());
    }
    if matches!(rule.delete_mode, DeleteMode::Trash) {
        if rule.garbage_path.trim().is_empty() {
            return Err("garbage_path is required when delete mode is `trash`".into());
        }
        if rule.garbage_path == rule.remote_path
            || rule
                .garbage_path
                .starts_with(&format!("{}/", rule.remote_path))
        {
            return Err(
                "garbage_path must NOT be inside remote_path (would cause recursion)".into(),
            );
        }
    }
    let _g = state.lock.lock().unwrap();
    let mut rules = store::load(&state.data_dir);
    if rule.id.is_empty() {
        rule.id = uuid::Uuid::new_v4().to_string();
        rules.push(rule.clone());
    } else {
        match rules.iter_mut().find(|r| r.id == rule.id) {
            Some(existing) => {
                rule.stats = existing.stats.clone();
                rule.last_run_at = existing.last_run_at.clone();
                rule.last_status = existing.last_status.clone();
                *existing = rule.clone();
            }
            None => rules.push(rule.clone()),
        }
    }
    store::save(&state.data_dir, &rules).map_err(|e| e.to_string())?;
    Ok(rule)
}

pub fn remove_rule_impl(state: &AppState, id: &str) -> Result<(), String> {
    // Metadata-only removal. No file ops on local or remote. The rule's
    // garbage_path on the remote is intentionally left intact.
    let _g = state.lock.lock().unwrap();
    let mut rules = store::load(&state.data_dir);
    rules.retain(|r| r.id != id);
    store::save(&state.data_dir, &rules).map_err(|e| e.to_string())
}

pub fn run_rule_impl(state: &AppState, id: &str) -> Result<RunResult, String> {
    run_rule_impl_with_log(state, id, |_| {})
}

pub fn run_rule_impl_with_log<F: FnMut(&str)>(
    state: &AppState,
    id: &str,
    mut on_log: F,
) -> Result<RunResult, String> {
    // Phase 1 — snapshot the rule under a brief lock. We do NOT hold the lock
    // while rclone runs (which can take seconds-to-minutes), otherwise every
    // other state-touching operation (list_rules, save_rule, frontend refresh,
    // the next iteration of Run-all) queues behind us and the UI feels frozen.
    let rule = {
        let _g = state.lock.lock().unwrap();
        store::load(&state.data_dir)
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| format!("rule {id} not found"))?
    };

    let dst = format!("{}:{}", rule.remote, rule.remote_path);
    let ts = Utc::now().format("%Y-%m-%d-%H%M%S").to_string();

    // Phase 2 — actually run rclone with no lock held.
    let out = match rule.delete_mode {
        DeleteMode::Safe => rclone::run_streaming(
            Subcommand::Copy,
            &[&rule.local_path, &dst],
            |line| on_log(line),
        )?,
        DeleteMode::Trash => {
            let backup = format!("{}:{}/{}", rule.remote, rule.garbage_path, ts);
            rclone::run_streaming(
                Subcommand::Sync,
                &[&rule.local_path, &dst, "--backup-dir", &backup],
                |line| on_log(line),
            )?
        }
    };

    let safety_violation = (out.counts.hard_deleted > 0).then(|| {
        format!(
            "SAFETY VIOLATION: rclone reported {} hard-deleted files; run rejected",
            out.counts.hard_deleted
        )
    });

    // Phase 3 — write stats / status back. Brief lock again. If the rule was
    // removed between phase 1 and phase 3 the row simply isn't there and we
    // silently drop the update.
    {
        let _g = state.lock.lock().unwrap();
        let mut rules = store::load(&state.data_dir);
        if let Some(r) = rules.iter_mut().find(|r| r.id == id) {
            if let Some(msg) = &safety_violation {
                r.last_status = Some(msg.clone());
            } else {
                r.stats.synced += out.counts.synced;
                r.stats.deleted += out.counts.moved_to_garbage;
                r.last_run_at = Some(Utc::now().to_rfc3339());
                r.last_status = Some(if out.success {
                    "ok".into()
                } else {
                    format!("rclone exited with error: {}", tail(&out.stderr, 400))
                });
            }
            store::save(&state.data_dir, &rules).map_err(|e| e.to_string())?;
        }
    }

    if let Some(msg) = safety_violation {
        return Err(msg);
    }

    Ok(RunResult {
        synced: out.counts.synced,
        moved_to_garbage: out.counts.moved_to_garbage,
        hard_deleted: out.counts.hard_deleted,
        success: out.success,
        log_tail: tail(&out.stderr, 800),
    })
}

pub fn list_remote_dirs_impl(remote: &str, path: &str) -> Result<Vec<String>, String> {
    let target = if path.is_empty() {
        format!("{remote}:")
    } else {
        format!("{remote}:{path}")
    };
    let out = rclone::run(Subcommand::LsJson, &["--dirs-only", &target])?;
    if !out.success {
        return Err(out.stderr);
    }
    #[derive(Deserialize)]
    struct Item {
        #[serde(rename = "Path")]
        path: String,
    }
    let items: Vec<Item> = serde_json::from_str(&out.stdout).map_err(|e| e.to_string())?;
    let mut paths: Vec<String> = items.into_iter().map(|i| i.path).collect();
    paths.sort();
    Ok(paths)
}

pub fn list_garbage_impl(state: &AppState, id: &str) -> Result<Vec<GarbageItem>, String> {
    // Brief lock to find the rule, then run rclone unlocked.
    let rule = {
        let _g = state.lock.lock().unwrap();
        store::load(&state.data_dir)
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| format!("rule {id} not found"))?
    };
    let target = format!("{}:{}", rule.remote, rule.garbage_path);
    let out = rclone::run(Subcommand::LsJson, &["--recursive", "--files-only", &target])?;
    if !out.success {
        if out.stderr.contains("directory not found") || out.stderr.contains("not found") {
            return Ok(vec![]);
        }
        return Err(out.stderr);
    }
    #[derive(Deserialize)]
    struct LsItem {
        #[serde(rename = "Path")]
        path: String,
        #[serde(rename = "Size")]
        size: i64,
        #[serde(rename = "IsDir")]
        is_dir: bool,
        #[serde(rename = "ModTime")]
        mod_time: String,
    }
    let items: Vec<LsItem> = serde_json::from_str(&out.stdout).map_err(|e| e.to_string())?;
    Ok(items
        .into_iter()
        .map(|i| GarbageItem {
            path: i.path,
            size: i.size,
            is_dir: i.is_dir,
            mod_time: i.mod_time,
        })
        .collect())
}

pub fn restore_file_impl(
    state: &AppState,
    id: &str,
    garbage_subpath: &str,
) -> Result<(), String> {
    // garbage_subpath looks like "<timestamp>/<original/relative/path>".
    let original = garbage_subpath
        .split_once('/')
        .map(|(_ts, rest)| rest)
        .ok_or("garbage_subpath must contain a timestamp prefix")?;

    // Phase 1 — snapshot the rule.
    let rule = {
        let _g = state.lock.lock().unwrap();
        store::load(&state.data_dir)
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| format!("rule {id} not found"))?
    };

    // Phase 2 — rclone moveto, no lock held.
    let src = format!("{}:{}/{}", rule.remote, rule.garbage_path, garbage_subpath);
    let dst = format!("{}:{}/{}", rule.remote, rule.remote_path, original);
    let out = rclone::run(Subcommand::MoveTo, &[&src, &dst])?;
    if !out.success {
        return Err(out.stderr);
    }

    // Phase 3 — bump the restored counter under a brief lock.
    {
        let _g = state.lock.lock().unwrap();
        let mut rules = store::load(&state.data_dir);
        if let Some(r) = rules.iter_mut().find(|r| r.id == id) {
            r.stats.restored += 1;
            store::save(&state.data_dir, &rules).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("…{}", &s[s.len() - max..])
    }
}

// ---------- Tauri command wrappers ----------

#[tauri::command]
fn list_remotes() -> Result<Vec<String>, String> {
    list_remotes_impl()
}

#[tauri::command]
fn list_rules(state: tauri::State<Arc<AppState>>) -> Vec<Rule> {
    list_rules_impl(state.as_ref())
}

#[tauri::command]
fn save_rule(
    app: AppHandle,
    state: tauri::State<Arc<AppState>>,
    rule: Rule,
) -> Result<Rule, String> {
    let saved = save_rule_impl(state.as_ref(), rule)?;
    let cb = make_run_callback(app);
    state
        .runners
        .restart_for(state.inner().clone(), &saved, Some(cb));
    Ok(saved)
}

#[tauri::command]
fn remove_rule(state: tauri::State<Arc<AppState>>, id: String) -> Result<(), String> {
    state.runners.stop_for(&id);
    remove_rule_impl(state.as_ref(), &id)
}

#[tauri::command]
async fn run_rule(
    app: AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    id: String,
) -> Result<(), String> {
    // Returns immediately. The actual sync runs on a blocking-IO thread so the
    // UI stays responsive and other rules can be triggered in parallel (they'll
    // serialize through the AppState lock, but the queue is non-blocking from
    // the frontend's perspective). Final outcome arrives via the `rule_run`
    // event; intermediate notice that work has started arrives via `rule_running`.
    let _ = app.emit("rule_running", serde_json::json!({ "id": &id }));
    let state_arc = state.inner().clone();
    let app_clone = app.clone();
    let id_clone = id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let app_for_log = app_clone.clone();
        let id_for_log = id_clone.clone();
        let res = run_rule_impl_with_log(&state_arc, &id_clone, move |line| {
            let _ = app_for_log.emit(
                "rule_log",
                serde_json::json!({ "id": &id_for_log, "line": line }),
            );
        });
        let _ = app_clone.emit("rule_run", run_payload(&id_clone, &res));
    });
    Ok(())
}

#[tauri::command]
fn list_remote_dirs(remote: String, path: String) -> Result<Vec<String>, String> {
    list_remote_dirs_impl(&remote, &path)
}

#[tauri::command]
fn list_garbage(
    state: tauri::State<Arc<AppState>>,
    id: String,
) -> Result<Vec<GarbageItem>, String> {
    list_garbage_impl(state.as_ref(), &id)
}

#[tauri::command]
fn restore_file(
    state: tauri::State<Arc<AppState>>,
    id: String,
    garbage_subpath: String,
) -> Result<(), String> {
    restore_file_impl(state.as_ref(), &id, &garbage_subpath)
}

fn run_payload(id: &str, res: &Result<RunResult, String>) -> serde_json::Value {
    match res {
        Ok(r) => serde_json::json!({"id": id, "result": r}),
        Err(e) => serde_json::json!({"id": id, "error": e}),
    }
}

fn make_run_callback(app: AppHandle) -> runner::RunCallback {
    Arc::new(move |id: &str, res: &Result<RunResult, String>| {
        let _ = app.emit("rule_run", run_payload(id, res));
    })
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn run_all_rules(app: &AppHandle) {
    let state: Arc<AppState> = app.state::<Arc<AppState>>().inner().clone();
    let app_clone = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        for rule in list_rules_impl(&state) {
            if !rule.enabled {
                continue;
            }
            let _ = app_clone.emit("rule_running", serde_json::json!({ "id": &rule.id }));
            let app_for_log = app_clone.clone();
            let id_for_log = rule.id.clone();
            let res = run_rule_impl_with_log(&state, &rule.id, move |line| {
                let _ = app_for_log.emit(
                    "rule_log",
                    serde_json::json!({ "id": &id_for_log, "line": line }),
                );
            });
            let _ = app_clone.emit("rule_run", run_payload(&rule.id, &res));
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // Closing the window hides it; the app keeps running with the tray icon.
        // The only way to fully quit is the tray menu's "Quit" item.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("app_data_dir: {e}"))?;
            std::fs::create_dir_all(&data_dir).ok();
            let state = Arc::new(AppState::new(data_dir));

            // Auto-start runners for already-saved enabled rules.
            let cb = make_run_callback(app.handle().clone());
            for rule in list_rules_impl(state.as_ref()) {
                state
                    .runners
                    .restart_for(state.clone(), &rule, Some(cb.clone()));
            }
            app.manage(state);

            // Tray icon + menu.
            let show_item =
                MenuItem::with_id(app, "show", "Show window", true, None::<&str>)?;
            let run_all_item =
                MenuItem::with_id(app, "run_all", "Run all rules now", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &show_item,
                    &PredefinedMenuItem::separator(app)?,
                    &run_all_item,
                    &PredefinedMenuItem::separator(app)?,
                    &quit_item,
                ],
            )?;

            // Monochrome template icon — macOS auto-tints it for light/dark menubar.
            let tray_icon = tauri::include_image!("icons/tray-icon-template.png");
            TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .icon_as_template(true)
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "run_all" => run_all_rules(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_remotes,
            list_remote_dirs,
            list_rules,
            save_rule,
            remove_rule,
            run_rule,
            list_garbage,
            restore_file,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
