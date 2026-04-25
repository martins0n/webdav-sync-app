use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

/// Locate the `rclone` binary. GUI apps launched from Finder have a stripped
/// `PATH` that excludes Homebrew's bin dir, so plain `Command::new("rclone")`
/// fails with ENOENT. We look in the usual install locations and fall back to
/// the unqualified name (PATH lookup) for unusual setups. The `WEBDAV_SYNC_RCLONE_BIN`
/// env var overrides everything for users with rclone installed elsewhere.
fn rclone_bin() -> &'static str {
    static BIN: OnceLock<String> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Ok(custom) = std::env::var("WEBDAV_SYNC_RCLONE_BIN") {
            if std::path::Path::new(&custom).exists() {
                return custom;
            }
        }
        for candidate in [
            "/opt/homebrew/bin/rclone", // Apple Silicon Homebrew
            "/usr/local/bin/rclone",    // Intel Homebrew / manual install
            "/usr/bin/rclone",          // Linux distro packages
        ] {
            if std::path::Path::new(candidate).exists() {
                return candidate.to_string();
            }
        }
        "rclone".into()
    })
}

/// Exhaustive allowlist of rclone subcommands the app is permitted to invoke.
///
/// SAFETY PRIME DIRECTIVE: this enum is the *only* place subcommands are named.
/// Adding a variant here is the only way to enable a new rclone action, and
/// destructive commands (`delete`, `deletefile`, `purge`, `rmdir`, `cleanup`,
/// `move` source-deleting) must NEVER be added.
#[derive(Debug, Clone, Copy)]
pub enum Subcommand {
    Copy,
    Sync,
    MoveTo,
    LsJson,
    ListRemotes,
}

impl Subcommand {
    pub fn as_arg(self) -> &'static str {
        match self {
            Subcommand::Copy => "copy",
            Subcommand::Sync => "sync",
            Subcommand::MoveTo => "moveto",
            Subcommand::LsJson => "lsjson",
            Subcommand::ListRemotes => "listremotes",
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RunCounts {
    /// Files newly uploaded or replaced on the remote.
    pub synced: u64,
    /// Files moved into the backup-dir (i.e. moved-to-garbage). Only non-zero in trash mode.
    pub moved_to_garbage: u64,
    /// Files hard-deleted by rclone. Should always be 0 for our subcommand set;
    /// non-zero indicates a safety violation and run_rule treats it as failure.
    pub hard_deleted: u64,
}

pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub counts: RunCounts,
}

pub fn run(sub: Subcommand, args: &[&str]) -> Result<RunOutput, String> {
    run_inner(sub, args, |_| {})
}

/// Like `run`, but invokes `on_line` for every stderr line as it arrives.
/// Used by the run-now path so the UI can show a live log.
pub fn run_streaming<F: FnMut(&str)>(
    sub: Subcommand,
    args: &[&str],
    on_line: F,
) -> Result<RunOutput, String> {
    run_inner(sub, args, on_line)
}

fn run_inner<F: FnMut(&str)>(
    sub: Subcommand,
    args: &[&str],
    mut on_line: F,
) -> Result<RunOutput, String> {
    // Periodic stats lines are useful only for transfer subcommands; for
    // listings they're noise that pollutes the stderr we return.
    let needs_stats = matches!(
        sub,
        Subcommand::Copy | Subcommand::Sync | Subcommand::MoveTo
    );

    let mut cmd = Command::new(rclone_bin());
    cmd.arg(sub.as_arg()).args(args).arg("-v");
    if needs_stats {
        cmd.args(["--stats=1s", "--stats-log-level=NOTICE"]);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn rclone ({}): {e}", rclone_bin()))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    // Drain stdout in a background thread so the child doesn't block on a
    // full pipe while we're reading stderr line by line on this thread.
    let stdout_handle = thread::spawn(move || {
        let mut s = String::new();
        BufReader::new(stdout).read_to_string(&mut s).ok();
        s
    });

    let mut stderr_acc = String::new();
    let reader = BufReader::new(stderr);
    for line_res in reader.lines() {
        match line_res {
            Ok(line) => {
                on_line(&line);
                stderr_acc.push_str(&line);
                stderr_acc.push('\n');
            }
            Err(_) => break,
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("rclone wait failed: {e}"))?;
    let stdout_str = stdout_handle.join().unwrap_or_default();

    let counts = parse_counts(&stderr_acc);
    Ok(RunOutput {
        stdout: stdout_str,
        stderr: stderr_acc,
        success: status.success(),
        counts,
    })
}

fn parse_counts(stderr: &str) -> RunCounts {
    let mut c = RunCounts::default();
    for line in stderr.lines() {
        // rclone -v prints one INFO line per file action.
        if line.contains(": Copied") {
            c.synced += 1;
        } else if line.contains(": Moved into backup dir") {
            c.moved_to_garbage += 1;
        } else if line.contains(": Deleted") && !line.contains(": Moved into backup dir") {
            c.hard_deleted += 1;
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_exactly_five_known_subcommands() {
        // Any future PR that adds a Subcommand variant must update this set
        // and consciously consider whether the new variant is safe.
        let all = [
            Subcommand::Copy,
            Subcommand::Sync,
            Subcommand::MoveTo,
            Subcommand::LsJson,
            Subcommand::ListRemotes,
        ];
        let names: Vec<&str> = all.iter().map(|s| s.as_arg()).collect();
        assert_eq!(names, vec!["copy", "sync", "moveto", "lsjson", "listremotes"]);

        let forbidden = ["delete", "deletefile", "purge", "rmdir", "cleanup", "move"];
        for f in forbidden {
            assert!(
                !names.contains(&f),
                "destructive subcommand `{f}` must never appear in allowlist"
            );
        }
    }

    #[test]
    fn parse_counts_handles_copy_output() {
        let stderr = "\
2026/04/25 19:03:59 INFO  : a.txt: Copied (new)
2026/04/25 19:03:59 INFO  : b.txt: Copied (replaced existing)
2026/04/25 19:03:59 INFO  : There was nothing to transfer";
        let c = parse_counts(stderr);
        assert_eq!(c.synced, 2);
        assert_eq!(c.moved_to_garbage, 0);
        assert_eq!(c.hard_deleted, 0);
    }

    #[test]
    fn parse_counts_handles_backup_dir() {
        let stderr = "\
2026/04/25 19:04:00 INFO  : a.txt: Moved (server-side)
2026/04/25 19:04:00 INFO  : a.txt: Moved into backup dir";
        let c = parse_counts(stderr);
        assert_eq!(c.synced, 0);
        assert_eq!(c.moved_to_garbage, 1);
        assert_eq!(c.hard_deleted, 0);
    }
}
