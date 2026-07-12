//! End-to-end integration tests against a real WebDAV server.
//!
//! Requires:
//!   1. A running docker container exposing WebDAV at http://localhost:8081
//!      (user: test / pass: test). Bring it up with:
//!        docker run -d --name wsa-dav -p 8081:80 \
//!          -e USERNAME=test -e PASSWORD=test bytemark/webdav
//!   2. An rclone remote named `dav:` pointing at it. Configure with:
//!        rclone config create dav webdav url http://localhost:8081 \
//!          vendor other user test pass "$(rclone obscure test)"
//!
//! Tests skip gracefully (printing a notice) if the `dav:` remote is missing.
//! Each test uses a UUID-suffixed remote path so reruns don't interfere.

use std::path::Path;
use std::process::Command;
use webdav_sync_app_lib::*;

const REMOTE: &str = "dav";

fn dav_available() -> bool {
    list_remotes_impl()
        .map(|rs| rs.iter().any(|r| r == REMOTE))
        .unwrap_or(false)
}

fn rclone_ls(target: &str) -> String {
    Command::new("rclone")
        .arg("ls")
        .arg(target)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn unique_suffix() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

fn make_rule(local: &Path, suffix: &str, mode: DeleteMode) -> Rule {
    Rule {
        id: String::new(),
        name: format!("e2e-{suffix}"),
        local_path: local.to_string_lossy().into_owned(),
        remote: REMOTE.into(),
        remote_path: format!("e2e-{suffix}-live"),
        delete_mode: mode,
        garbage_path: format!("e2e-{suffix}-garbage"),
        interval_seconds: None,
        watch: false,
        filters: vec![],
        filter_mode: FilterMode::Exclude,
        enabled: true,
        stats: Stats::default(),
        last_run_at: None,
        last_status: None,
    }
}

fn rss_kb() -> u64 {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .expect("ps failed");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

macro_rules! skip_if_no_dav {
    () => {
        if !dav_available() {
            eprintln!(
                "SKIPPING: rclone remote `{REMOTE}:` not configured. \
                 Start docker container and run `rclone config` first."
            );
            return;
        }
    };
}

#[test]
fn req_1_local_file_uploads_to_cloud() {
    skip_if_no_dav!();
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    std::fs::write(local.path().join("hello.txt"), "world").unwrap();
    let rule =
        save_rule_impl(&state, make_rule(local.path(), &sfx, DeleteMode::Safe)).unwrap();

    let res = run_rule_impl(&state, &rule.id).unwrap();
    assert!(res.success, "rclone failed: {}", res.log_tail);
    assert_eq!(res.synced, 1);
    assert_eq!(res.hard_deleted, 0);

    let listed = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(
        listed.contains("hello.txt"),
        "remote listing missing file: {listed}"
    );
}

#[test]
fn req_2_local_delete_does_not_delete_cloud() {
    skip_if_no_dav!();
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    std::fs::write(local.path().join("keep.txt"), "data").unwrap();
    let rule =
        save_rule_impl(&state, make_rule(local.path(), &sfx, DeleteMode::Safe)).unwrap();
    run_rule_impl(&state, &rule.id).unwrap();

    // delete LOCAL file, not the cloud copy
    std::fs::remove_file(local.path().join("keep.txt")).unwrap();
    let res = run_rule_impl(&state, &rule.id).unwrap();
    assert!(res.success);
    assert_eq!(res.hard_deleted, 0);

    let listed = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(
        listed.contains("keep.txt"),
        "Req 2 violated — file gone from cloud after local delete: {listed}"
    );
}

#[test]
fn req_3_trash_mode_moves_to_garbage_then_restores() {
    skip_if_no_dav!();
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    std::fs::write(local.path().join("a.txt"), "AAAA").unwrap();
    std::fs::write(local.path().join("b.txt"), "BBBB").unwrap();
    let rule =
        save_rule_impl(&state, make_rule(local.path(), &sfx, DeleteMode::Trash)).unwrap();

    let r1 = run_rule_impl(&state, &rule.id).unwrap();
    assert!(r1.success);
    assert_eq!(r1.synced, 2);

    std::fs::remove_file(local.path().join("a.txt")).unwrap();
    let r2 = run_rule_impl(&state, &rule.id).unwrap();
    assert!(r2.success);
    assert_eq!(
        r2.moved_to_garbage, 1,
        "expected 1 file moved to garbage, got {}",
        r2.moved_to_garbage
    );
    assert_eq!(r2.hard_deleted, 0, "Safety violated: hard_deleted > 0");

    let live = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(!live.contains("a.txt"), "a.txt should be gone from live");
    assert!(live.contains("b.txt"), "b.txt should still be in live");

    let garbage = list_garbage_impl(&state, &rule.id).unwrap();
    let item = garbage
        .iter()
        .find(|g| g.path.ends_with("a.txt"))
        .unwrap_or_else(|| panic!("a.txt missing from garbage listing: {garbage:?}"));

    restore_file_impl(&state, &rule.id, &item.path).unwrap();
    let live_after = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(
        live_after.contains("a.txt"),
        "restore failed; live listing: {live_after}"
    );

    let rules = list_rules_impl(&state);
    let r = rules.iter().find(|r| r.id == rule.id).unwrap();
    assert_eq!(r.stats.synced, 2, "synced counter wrong");
    assert_eq!(r.stats.deleted, 1, "deleted (moved-to-garbage) counter wrong");
    assert_eq!(r.stats.restored, 1, "restored counter wrong");
}

#[test]
fn req_5_remove_rule_is_metadata_only() {
    skip_if_no_dav!();
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    std::fs::write(local.path().join("survivor.txt"), "x").unwrap();
    let rule =
        save_rule_impl(&state, make_rule(local.path(), &sfx, DeleteMode::Safe)).unwrap();
    run_rule_impl(&state, &rule.id).unwrap();

    let before = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(before.contains("survivor.txt"));

    remove_rule_impl(&state, &rule.id).unwrap();
    assert!(list_rules_impl(&state).is_empty());

    // Local file still here.
    assert!(
        local.path().join("survivor.txt").exists(),
        "Local file deleted by remove_rule!"
    );
    // Remote file still here.
    let after = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(
        after.contains("survivor.txt"),
        "Remote file deleted by remove_rule!"
    );
}

#[test]
fn req_6_exclude_filters_skip_matching_files() {
    skip_if_no_dav!();
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    std::fs::write(local.path().join("keep.txt"), "keep").unwrap();
    std::fs::write(local.path().join("skip.rrdata"), "nope").unwrap();
    std::fs::write(local.path().join(".hidden"), "nope").unwrap();

    let mut rule = make_rule(local.path(), &sfx, DeleteMode::Safe);
    rule.filter_mode = FilterMode::Exclude;
    rule.filters = vec!["*.rrdata".into(), ".*".into()];
    let rule = save_rule_impl(&state, rule).unwrap();

    let res = run_rule_impl(&state, &rule.id).unwrap();
    assert!(res.success, "rclone failed: {}", res.log_tail);
    assert_eq!(res.synced, 1, "only keep.txt should upload, got {}", res.synced);

    let listed = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(listed.contains("keep.txt"), "keep.txt missing: {listed}");
    assert!(
        !listed.contains("skip.rrdata"),
        "excluded *.rrdata file was uploaded: {listed}"
    );
    assert!(
        !listed.contains(".hidden"),
        "excluded dotfile was uploaded: {listed}"
    );
}

#[test]
fn req_7_include_filters_sync_only_matching() {
    skip_if_no_dav!();
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    std::fs::write(local.path().join("photo.ARW"), "raw").unwrap();
    std::fs::write(local.path().join("note.txt"), "text").unwrap();

    let mut rule = make_rule(local.path(), &sfx, DeleteMode::Safe);
    rule.filter_mode = FilterMode::Include;
    rule.filters = vec!["*.ARW".into()];
    let rule = save_rule_impl(&state, rule).unwrap();

    let res = run_rule_impl(&state, &rule.id).unwrap();
    assert!(res.success, "rclone failed: {}", res.log_tail);
    assert_eq!(res.synced, 1, "only the .ARW file should upload, got {}", res.synced);

    let listed = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(listed.contains("photo.ARW"), "photo.ARW missing: {listed}");
    assert!(
        !listed.contains("note.txt"),
        "non-included note.txt was uploaded despite include-only filter: {listed}"
    );
}

#[test]
fn save_rule_normalizes_filters() {
    // No dav needed — pure metadata normalization.
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    let mut rule = make_rule(Path::new("/tmp"), "x", DeleteMode::Safe);
    rule.filters = vec!["  *.rrdata  ".into(), "".into(), "   ".into(), ".*".into()];
    let saved = save_rule_impl(&state, rule).unwrap();
    assert_eq!(
        saved.filters,
        vec!["*.rrdata".to_string(), ".*".to_string()],
        "blank lines should be dropped and patterns trimmed"
    );
}

#[test]
fn save_rule_rejects_garbage_inside_remote_path() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    let mut bad = make_rule(Path::new("/tmp"), "x", DeleteMode::Trash);
    bad.remote_path = "live".into();
    bad.garbage_path = "live/garbage".into();
    let err = save_rule_impl(&state, bad).unwrap_err();
    assert!(
        err.contains("recursion") || err.contains("inside"),
        "expected rejection, got: {err}"
    );
}

#[test]
fn restore_does_not_overwrite_a_current_file_at_dst() {
    if !dav_available() {
        eprintln!("SKIPPING restore_does_not_overwrite: dav: not configured");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    let rule = save_rule_impl(&state, make_rule(local.path(), &sfx, DeleteMode::Trash)).unwrap();

    // 1. Upload v1, then delete locally and re-sync — v1 lives in garbage now.
    std::fs::write(local.path().join("a.txt"), "old version content").unwrap();
    run_rule_impl(&state, &rule.id).unwrap();
    std::fs::remove_file(local.path().join("a.txt")).unwrap();
    run_rule_impl(&state, &rule.id).unwrap();

    // 2. Create a different file with the same name, sync — v2 lives at the canonical path.
    std::fs::write(local.path().join("a.txt"), "new").unwrap();
    run_rule_impl(&state, &rule.id).unwrap();

    // 3. Restore v1 from garbage. It MUST NOT clobber v2.
    let garbage = list_garbage_impl(&state, &rule.id).unwrap();
    let v1 = garbage
        .iter()
        .find(|g| g.path.ends_with("a.txt"))
        .expect("v1 missing from garbage");
    restore_file_impl(&state, &rule.id, &v1.path).unwrap();

    let listing = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    let lines: Vec<&str> = listing.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "expected 2 files at canonical path (v2 + restored v1), got: {listing}"
    );
    assert!(
        lines.iter().any(|l| l.ends_with("a.txt") && l.contains(" 3 ")),
        "v2 (3 bytes) was overwritten! {listing}"
    );
    assert!(
        lines.iter().any(|l| l.contains("a.txt.restored-")),
        "no .restored- sibling for v1: {listing}"
    );
}

#[test]
fn freeze_extend_stacks_and_clear_resets() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());

    assert_eq!(freeze_status_impl(&state), 0, "starts unfrozen");

    let u1 = freeze_extend_impl(&state, 60);
    assert!(u1 > 0, "freeze should be active after extend");
    assert_eq!(freeze_status_impl(&state), u1);

    // A second +60 stacks on the remaining time rather than resetting it.
    let u2 = freeze_extend_impl(&state, 60);
    assert_eq!(u2, u1 + 60 * 60_000, "second +1h should stack on the first");

    assert_eq!(freeze_clear_impl(&state), 0, "clear returns 0");
    assert_eq!(freeze_status_impl(&state), 0, "status is 0 after clear");
}

#[test]
fn freeze_extend_is_capped_at_24h() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    // 100 hours of +1h must not push the resume time past ~24h out.
    let mut last = 0;
    for _ in 0..100 {
        last = freeze_extend_impl(&state, 60);
    }
    let horizon = last - chrono::Utc::now().timestamp_millis();
    assert!(
        horizon <= 24 * 60 * 60 * 1000 + 1000,
        "freeze horizon {horizon} ms exceeds the 24h cap"
    );
}

#[test]
fn save_rule_rejects_garbage_equal_to_remote_path() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::new(data.path().to_path_buf());
    let mut bad = make_rule(Path::new("/tmp"), "x", DeleteMode::Trash);
    bad.remote_path = "same".into();
    bad.garbage_path = "same".into();
    save_rule_impl(&state, bad).unwrap_err();
}

// -------- scheduler / watcher tests --------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_fires_on_interval() {
    if !dav_available() {
        eprintln!("SKIPPING scheduler_fires_on_interval: dav: not configured");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(AppState::new(data.path().to_path_buf()));
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    let mut rule = make_rule(local.path(), &sfx, DeleteMode::Safe);
    rule.interval_seconds = Some(1);
    let saved = save_rule_impl(&state, rule).unwrap();

    // Drop a file and then start the runner. First user-visible tick fires
    // 1 second after start; with 4 seconds budget docker emulation has time.
    std::fs::write(local.path().join("scheduled.txt"), "tick").unwrap();
    state.runners.restart_for(state.clone(), &saved, None);

    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    state.runners.stop_for(&saved.id);

    let listed = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(
        listed.contains("scheduled.txt"),
        "scheduler did not upload file within 4s: {listed}"
    );
    let stats = list_rules_impl(&state)
        .into_iter()
        .find(|r| r.id == saved.id)
        .unwrap()
        .stats;
    assert!(stats.synced >= 1, "synced counter not bumped: {stats:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_fires_on_local_change() {
    if !dav_available() {
        eprintln!("SKIPPING watcher_fires_on_local_change: dav: not configured");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(AppState::new(data.path().to_path_buf()));
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    let mut rule = make_rule(local.path(), &sfx, DeleteMode::Safe);
    rule.watch = true;
    let saved = save_rule_impl(&state, rule).unwrap();

    state.runners.restart_for(state.clone(), &saved, None);
    // Let watcher initialize before changing files.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    std::fs::write(local.path().join("watched.txt"), "fs-event").unwrap();

    // Debounce window is 2s; sync runs after that. 6s budget is generous.
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    state.runners.stop_for(&saved.id);

    let listed = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(
        listed.contains("watched.txt"),
        "watcher did not upload file within 6s: {listed}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabled_false_disables_automation_but_run_now_still_works() {
    if !dav_available() {
        eprintln!("SKIPPING enabled_false test: dav: not configured");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(AppState::new(data.path().to_path_buf()));
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    let mut rule = make_rule(local.path(), &sfx, DeleteMode::Safe);
    rule.interval_seconds = Some(1);
    rule.watch = true;
    rule.enabled = false; // master switch off
    let saved = save_rule_impl(&state, rule).unwrap();
    state.runners.restart_for(state.clone(), &saved, None);

    std::fs::write(local.path().join("must-not-upload.txt"), "x").unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    state.runners.stop_for(&saved.id);

    let listed = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(
        !listed.contains("must-not-upload.txt"),
        "disabled rule auto-uploaded a file: {listed}"
    );

    // But manual Run now still works.
    let res = run_rule_impl(&state, &saved.id).unwrap();
    assert!(res.success);
    let listed2 = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(listed2.contains("must-not-upload.txt"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_suppresses_scheduler_then_catches_up_on_resume() {
    if !dav_available() {
        eprintln!("SKIPPING freeze_suppresses_scheduler...: dav: not configured");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(AppState::new(data.path().to_path_buf()));
    let local = tempfile::tempdir().unwrap();
    let sfx = unique_suffix();

    let mut rule = make_rule(local.path(), &sfx, DeleteMode::Safe);
    rule.interval_seconds = Some(1);
    let saved = save_rule_impl(&state, rule).unwrap();

    // Freeze, then start the runner and drop a file. Scheduler ticks fire but
    // must be suppressed for the duration of the freeze.
    freeze_extend_impl(&state, 60);
    std::fs::write(local.path().join("frozen.txt"), "x").unwrap();
    state.runners.restart_for(state.clone(), &saved, None);

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let listed = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(
        !listed.contains("frozen.txt"),
        "freeze did not suppress the scheduled sync: {listed}"
    );

    // Resume → the suppressed trigger catches up and uploads promptly.
    freeze_clear_impl(&state);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    state.runners.stop_for(&saved.id);

    let listed2 = rclone_ls(&format!("{REMOTE}:e2e-{sfx}-live"));
    assert!(
        listed2.contains("frozen.txt"),
        "catch-up after resume did not upload the file: {listed2}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_runners_are_resource_efficient() {
    // No dav: needed: runners idle without firing syncs (long interval, no file
    // changes), so they don't call rclone.
    let data = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(AppState::new(data.path().to_path_buf()));

    // Spin up 10 rules: each watching a unique tempdir with a 1-hour interval.
    // None will fire during the test.
    let mut locals = Vec::new();
    let mut ids = Vec::new();
    for i in 0..10 {
        let local = tempfile::tempdir().unwrap();
        let mut rule = make_rule(local.path(), &format!("idle-{i}"), DeleteMode::Safe);
        rule.interval_seconds = Some(3600);
        rule.watch = true;
        rule.remote = "nonexistent".into(); // never resolved in this test
        rule.remote_path = format!("idle-{i}");
        rule.garbage_path = format!("idle-{i}-garbage");
        let saved = save_rule_impl(&state, rule).unwrap();
        state.runners.restart_for(state.clone(), &saved, None);
        ids.push(saved.id);
        locals.push(local);
    }

    let baseline = rss_kb();
    let t0 = std::time::Instant::now();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let elapsed = t0.elapsed();
    let after = rss_kb();

    // Stop all runners.
    for id in &ids {
        state.runners.stop_for(id);
    }

    // 1) Sleep should take ~3s real time. If a runner busy-loops, the test
    //    runtime may starve and sleep stretches significantly.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "3s sleep took {elapsed:?} — possible busy loop"
    );
    // 2) RSS should not balloon while idle. 10 runners + their watchers and
    //    debouncers. 50 MB headroom over baseline is generous; tighter than
    //    that is implementation-specific and would be flaky.
    let growth = after.saturating_sub(baseline);
    assert!(
        growth < 50 * 1024,
        "idle RSS grew by {growth} KB (baseline {baseline} KB → {after} KB)"
    );
    eprintln!(
        "idle resource check: baseline={baseline} KB after={after} KB growth={growth} KB elapsed={elapsed:?}"
    );
}
