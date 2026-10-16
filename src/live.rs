// A short log of what the MCP file tools did, so the GUI's Live tab can say which tool changed a
// file. It is written just before the change and only ever read back for a few seconds.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::Repository;
use serde::{Deserialize, Serialize};

const KEEP: usize = 200;
const ROTATE_AT: u64 = 64 * 1024;

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
  pub ts: i64,
  pub tool: String,
  pub path: String,
  #[serde(default)]
  pub agent: Option<String>,
}

pub fn now_ms() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_or(0, |d| d.as_millis() as i64)
}

fn log_path(git_dir: &Path) -> PathBuf {
  git_dir.join("mig").join("live.jsonl")
}

fn read_all(git_dir: &Path) -> Vec<Entry> {
  std::fs::read_to_string(log_path(git_dir))
    .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
    .unwrap_or_default()
}

// Best effort: a failure to log must never stop the file operation itself.
pub fn record(repo: &Repository, tool: &str, path: &str, agent: Option<&str>) {
  let file = log_path(repo.path());
  if let Some(dir) = file.parent() {
    let _ = std::fs::create_dir_all(dir);
  }
  if std::fs::metadata(&file).is_ok_and(|m| m.len() > ROTATE_AT) {
    let all = read_all(repo.path());
    let tail: Vec<String> = all
      .iter()
      .skip(all.len().saturating_sub(KEEP))
      .filter_map(|e| serde_json::to_string(e).ok())
      .collect();
    let _ = std::fs::write(&file, tail.join("\n") + "\n");
  }
  let entry = Entry {
    ts: now_ms(),
    tool: tool.to_string(),
    path: path.to_string(),
    agent: agent.map(str::to_string),
  };
  if let (Ok(line), Ok(mut f)) = (
    serde_json::to_string(&entry),
    std::fs::OpenOptions::new().create(true).append(true).open(&file),
  ) {
    let _ = writeln!(f, "{line}");
  }
}

// The newest entry for `path` (or a folder containing it) made within `within_ms`.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn recent(git_dir: &Path, path: &str, within_ms: i64) -> Option<Entry> {
  let now = now_ms();
  read_all(git_dir)
    .into_iter()
    .rev()
    .find(|e| now - e.ts <= within_ms && covers(e, path))
}

fn covers(e: &Entry, path: &str) -> bool {
  e.path == path || path.starts_with(&format!("{}/", e.path))
}

// The newest entry for `path` written by someone other than `me`.
pub fn recent_by_other(git_dir: &Path, path: &str, me: Option<&str>, within_ms: i64) -> Option<Entry> {
  let now = now_ms();
  read_all(git_dir)
    .into_iter()
    .rev()
    .take_while(|e| now - e.ts <= within_ms)
    .find(|e| covers(e, path) && e.agent.as_deref() != me)
}
