//! Per-rule background task: scheduler + FS watcher.
//!
//! Each enabled rule that has either `interval_seconds` set or `watch = true`
//! gets a tokio task. The task triggers `run_rule_impl` on:
//!   - every interval tick, and/or
//!   - every debounced batch of `notify` events under `local_path`.
//!
//! Triggers serialize through the AppState mutex inside `run_rule_impl`, so
//! a watch event arriving mid-interval can't double-fire a sync. Cancellation
//! propagates via a watch::channel — dropping the handle stops the task.

use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::async_runtime;
use tokio::sync::{mpsc, watch};

use crate::{run_rule_impl, AppState, RunResult, Rule};

/// Debounce window for FS events. Bursts of file changes within this period
/// coalesce into a single sync trigger.
const WATCH_DEBOUNCE: Duration = Duration::from_secs(2);

pub type RunCallback = Arc<dyn Fn(&str, &Result<RunResult, String>) + Send + Sync>;

struct Handle {
    cancel: watch::Sender<bool>,
}

#[derive(Default)]
pub struct Runners {
    handles: Mutex<HashMap<String, Handle>>,
}

impl Runners {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stop any existing runner for this rule and start a fresh one if the rule
    /// is enabled and has a scheduler/watcher configured. No-op otherwise.
    pub fn restart_for(
        &self,
        state: Arc<AppState>,
        rule: &Rule,
        on_run: Option<RunCallback>,
    ) {
        self.stop_for(&rule.id);
        if !rule.enabled {
            return;
        }
        let interval = rule.interval_seconds.filter(|&s| s > 0);
        if interval.is_none() && !rule.watch {
            return;
        }
        let (cancel_tx, cancel_rx) = watch::channel(false);
        spawn_loop(
            rule.id.clone(),
            state,
            interval,
            if rule.watch {
                Some(PathBuf::from(&rule.local_path))
            } else {
                None
            },
            on_run,
            cancel_rx,
        );
        self.handles
            .lock()
            .unwrap()
            .insert(rule.id.clone(), Handle { cancel: cancel_tx });
    }

    pub fn stop_for(&self, rule_id: &str) {
        if let Some(h) = self.handles.lock().unwrap().remove(rule_id) {
            // Setting to true (or any change) wakes the runner; dropping the
            // sender also closes the channel and breaks the loop.
            let _ = h.cancel.send(true);
        }
    }

    pub fn stop_all(&self) {
        let mut map = self.handles.lock().unwrap();
        for (_, h) in map.drain() {
            let _ = h.cancel.send(true);
        }
    }
}

fn spawn_loop(
    rule_id: String,
    state: Arc<AppState>,
    interval: Option<u64>,
    watch_path: Option<PathBuf>,
    on_run: Option<RunCallback>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    async_runtime::spawn(async move {
        // FS watcher → mpsc → tokio select.
        let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<()>();
        let _debouncer = if let Some(path) = watch_path.as_ref() {
            let tx = fs_tx.clone();
            match new_debouncer(WATCH_DEBOUNCE, move |res: notify_debouncer_mini::DebounceEventResult| {
                if res.is_ok() {
                    let _ = tx.send(());
                }
            }) {
                Ok(mut deb) => {
                    if deb.watcher().watch(path, RecursiveMode::Recursive).is_err() {
                        eprintln!(
                            "runner[{rule_id}]: failed to watch {} (does it exist?)",
                            path.display()
                        );
                        None
                    } else {
                        Some(deb)
                    }
                }
                Err(e) => {
                    eprintln!("runner[{rule_id}]: debouncer init failed: {e}");
                    None
                }
            }
        } else {
            None
        };

        // Interval; if disabled, use a far-future tick that effectively never fires.
        let mut tick = tokio::time::interval(
            interval.map(Duration::from_secs).unwrap_or(Duration::from_secs(60 * 60 * 24 * 365)),
        );
        // First tick fires immediately by default — we don't want an immediate
        // sync on startup, so consume it.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;

        loop {
            tokio::select! {
                _ = tick.tick(), if interval.is_some() => {
                    fire(&rule_id, &state, &on_run);
                }
                Some(_) = fs_rx.recv() => {
                    fire(&rule_id, &state, &on_run);
                }
                _ = cancel_rx.changed() => {
                    break;
                }
            }
        }
    });
}

fn fire(rule_id: &str, state: &Arc<AppState>, on_run: &Option<RunCallback>) {
    let res = run_rule_impl(state, rule_id);
    if let Some(cb) = on_run {
        cb(rule_id, &res);
    }
}
