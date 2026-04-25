use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    Safe,
    Trash,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Stats {
    pub synced: u64,
    pub deleted: u64,
    pub restored: u64,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub local_path: String,
    pub remote: String,
    pub remote_path: String,
    pub delete_mode: DeleteMode,
    /// Remote path for the trash folder. MUST NOT be inside `remote_path` —
    /// that would cause `sync` to recursively re-trash files. Use a sibling
    /// path on the same remote, e.g. `remote_path` = "Documents",
    /// `garbage_path` = "Documents-garbage".
    pub garbage_path: String,
    /// Run automatically every N seconds. `None` or `Some(0)` disables the
    /// scheduler (manual `Run now` only).
    #[serde(default)]
    pub interval_seconds: Option<u64>,
    /// Watch `local_path` for FS changes and re-sync on change (debounced).
    #[serde(default)]
    pub watch: bool,
    /// Master switch. When false, neither the scheduler nor the watcher fires;
    /// `Run now` still works.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub stats: Stats,
    #[serde(default)]
    pub last_run_at: Option<String>,
    #[serde(default)]
    pub last_status: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Doc {
    rules: Vec<Rule>,
}

fn rules_path(app_data: &Path) -> PathBuf {
    app_data.join("rules.json")
}

pub fn load(app_data: &Path) -> Vec<Rule> {
    let p = rules_path(app_data);
    if !p.exists() {
        return vec![];
    }
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    serde_json::from_str::<Doc>(&text)
        .map(|d| d.rules)
        .unwrap_or_default()
}

pub fn save(app_data: &Path, rules: &[Rule]) -> std::io::Result<()> {
    std::fs::create_dir_all(app_data)?;
    let doc = Doc {
        rules: rules.to_vec(),
    };
    let json = serde_json::to_string_pretty(&doc)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // Atomic write: dump to a sibling tmp file, fsync, then rename over the
    // real one. A crash mid-write leaves the previous rules.json untouched
    // instead of producing a half-written file.
    let final_path = rules_path(app_data);
    let tmp_path = final_path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)
}
