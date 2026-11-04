// `mig lock`: takes Claude Code's own command tools (Bash, PowerShell, Monitor) away in one
// repository, so that commands go through mig's run tools and their checks. It edits the
// repository's Claude Code settings and remembers exactly what it added, so `mig unlock` can take
// that back out and leave everything else alone.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use git2::Repository;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const DENY: &[&str] = &[
  "Bash",
  "PowerShell",
  "Monitor",
  "Edit(.claude/**)",
  "Edit(.mcp.json)",
  "Edit(.mig.toml)",
];

fn allow_for(server: &str) -> Vec<String> {
  ["run", "run_script", "run_check"]
    .iter()
    .map(|t| format!("mcp__{server}__{t}"))
    .collect()
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Record {
  // Settings file, relative to the repository.
  file: String,
  server: String,
  // The file did not exist before.
  created: bool,
  deny: Vec<String>,
  allow: Vec<String>,
}

fn record_path(repo: &Repository) -> PathBuf {
  repo.path().join("mig").join("lock.json")
}

fn read_record(repo: &Repository) -> Option<Record> {
  serde_json::from_str(&std::fs::read_to_string(record_path(repo)).ok()?).ok()
}

fn settings_file(root: &Path, shared: bool) -> (String, PathBuf) {
  let rel = if shared {
    ".claude/settings.json"
  } else {
    ".claude/settings.local.json"
  };
  (rel.to_string(), root.join(rel))
}

fn load(path: &Path) -> Result<Value> {
  if !path.exists() {
    return Ok(json!({}));
  }
  let text = std::fs::read_to_string(path)?;
  if text.trim().is_empty() {
    return Ok(json!({}));
  }
  serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON; fix it first", path.display()))
}

fn save(path: &Path, v: &Value) -> Result<()> {
  if let Some(d) = path.parent() {
    std::fs::create_dir_all(d)?;
  }
  let tmp = path.with_extension("json.tmp");
  std::fs::write(&tmp, serde_json::to_string_pretty(v)? + "\n")?;
  std::fs::rename(tmp, path)?;
  Ok(())
}

fn list_mut<'a>(v: &'a mut Value, key: &str) -> Result<&'a mut Vec<Value>> {
  let perms = v
    .as_object_mut()
    .ok_or_else(|| anyhow!("the settings file is not a JSON object"))?
    .entry("permissions")
    .or_insert_with(|| json!({}));
  let list = perms
    .as_object_mut()
    .ok_or_else(|| anyhow!("\"permissions\" in the settings file is not an object"))?
    .entry(key)
    .or_insert_with(|| json!([]));
  list
    .as_array_mut()
    .ok_or_else(|| anyhow!("permissions.{key} is not a list"))
}

fn has(list: &[Value], entry: &str) -> bool {
  list.iter().any(|x| x.as_str() == Some(entry))
}

// Keeps the settings file out of git: it is personal.
fn keep_local_file_out_of_git(repo: &Repository, rel: &str) {
  if repo.is_path_ignored(rel).unwrap_or(true) {
    return;
  }
  let exclude = repo.path().join("info").join("exclude");
  if let Some(d) = exclude.parent() {
    let _ = std::fs::create_dir_all(d);
  }
  let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
  if !text.is_empty() && !text.ends_with('\n') {
    text.push('\n');
  }
  text.push_str(rel);
  text.push('\n');
  let _ = std::fs::write(exclude, text);
}

pub struct Locked {
  pub file: String,
  pub added: usize,
}

pub fn lock(repo: &Repository, server: &str, shared: bool) -> Result<Locked> {
  let root = repo
    .workdir()
    .ok_or_else(|| anyhow!("this repository has no working folder"))?;
  if let Some(r) = read_record(repo) {
    bail!(
      "already locked through {} (run `mig unlock` first to change it)",
      r.file
    );
  }
  if server.is_empty()
    || !server
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
  {
    bail!("the MCP server name may only have letters, digits, _ and -");
  }
  let (rel, path) = settings_file(root, shared);
  let created = !path.exists();
  let mut v = load(&path)?;
  let mut rec = Record {
    file: rel.clone(),
    server: server.to_string(),
    created,
    ..Default::default()
  };
  {
    let deny = list_mut(&mut v, "deny")?;
    for d in DENY {
      if !has(deny, d) {
        deny.push(json!(d));
        rec.deny.push(d.to_string());
      }
    }
  }
  {
    let allow = list_mut(&mut v, "allow")?;
    for a in allow_for(server) {
      if !has(allow, &a) {
        allow.push(json!(a));
        rec.allow.push(a);
      }
    }
  }
  save(&path, &v)?;
  if !shared {
    keep_local_file_out_of_git(repo, &rel);
  }
  let rec_path = record_path(repo);
  if let Some(d) = rec_path.parent() {
    std::fs::create_dir_all(d)?;
  }
  std::fs::write(rec_path, serde_json::to_string_pretty(&rec)?)?;
  Ok(Locked {
    file: rel,
    added: rec.deny.len() + rec.allow.len(),
  })
}

pub fn unlock(repo: &Repository) -> Result<String> {
  let root = repo
    .workdir()
    .ok_or_else(|| anyhow!("this repository has no working folder"))?;
  let rec = read_record(repo).ok_or_else(|| anyhow!("mig has not locked this repository"))?;
  let path = root.join(&rec.file);
  if path.exists() {
    let mut v = load(&path)?;
    for (key, entries) in [("deny", &rec.deny), ("allow", &rec.allow)] {
      if let Ok(list) = list_mut(&mut v, key) {
        list.retain(|x| !entries.iter().any(|e| x.as_str() == Some(e)));
      }
    }
    // Drop what became empty.
    if let Some(perms) = v.get_mut("permissions").and_then(Value::as_object_mut) {
      for k in ["deny", "allow"] {
        if perms.get(k).and_then(Value::as_array).is_some_and(Vec::is_empty) {
          perms.remove(k);
        }
      }
    }
    if v
      .get("permissions")
      .and_then(Value::as_object)
      .is_some_and(|p| p.is_empty())
    {
      v.as_object_mut().map(|o| o.remove("permissions"));
    }
    if rec.created && v.as_object().is_some_and(|o| o.is_empty()) {
      let _ = std::fs::remove_file(&path);
      // Only an empty folder: a person's own files stay.
      if let Some(d) = path.parent() {
        let _ = std::fs::remove_dir(d);
      }
    } else {
      save(&path, &v)?;
    }
  }
  let _ = std::fs::remove_file(record_path(repo));
  Ok(rec.file)
}

pub struct State {
  // Settings files that take Bash away.
  pub files: Vec<String>,
  pub by_mig: bool,
  // Where the MCP server named `server` is registered.
  pub mcp: Vec<String>,
}

fn denies_bash(path: &Path) -> bool {
  load(path)
    .ok()
    .and_then(|v| v.get("permissions")?.get("deny")?.as_array().map(|a| has(a, "Bash")))
    .unwrap_or(false)
}

fn has_server(v: &Value, server: &str) -> bool {
  v.get("mcpServers").and_then(|m| m.get(server)).is_some()
}

pub fn state(repo: &Repository, server: &str) -> State {
  let root = repo.workdir().map(Path::to_path_buf).unwrap_or_default();
  let mut files = Vec::new();
  for rel in [".claude/settings.local.json", ".claude/settings.json"] {
    if denies_bash(&root.join(rel)) {
      files.push(rel.to_string());
    }
  }
  let mut mcp = Vec::new();
  if load(&root.join(".mcp.json")).is_ok_and(|v| has_server(&v, server)) {
    mcp.push(".mcp.json (project)".to_string());
  }
  if let Some(home) = dirs::home_dir()
    && let Ok(v) = load(&home.join(".claude.json"))
  {
    if has_server(&v, server) {
      mcp.push("~/.claude.json (user)".to_string());
    }
    let key = root.to_string_lossy();
    let alt = key.replace('\\', "/");
    if let Some(p) = v.get("projects").and_then(Value::as_object) {
      for (k, val) in p {
        if (k == key.as_ref() || *k == alt) && has_server(val, server) {
          mcp.push("~/.claude.json (this project, local)".to_string());
        }
      }
    }
  }
  State {
    files,
    by_mig: read_record(repo).is_some(),
    mcp,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn repo() -> (PathBuf, Repository) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("mig-lock-{}-{}-{n}", std::process::id(), crate::live::now_ms()));
    std::fs::create_dir_all(&d).unwrap();
    let r = Repository::init(&d).unwrap();
    (d, r)
  }

  fn json_of(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
  }

  #[test]
  fn lock_adds_rules_and_unlock_removes_only_those() {
    let (d, r) = repo();
    std::fs::create_dir_all(d.join(".claude")).unwrap();
    std::fs::write(
      d.join(".claude/settings.local.json"),
      r#"{"model":"opus","permissions":{"deny":["Read(./.env)","Bash"],"allow":["WebFetch"]},"env":{"A":"1"}}"#,
    )
    .unwrap();
    let l = lock(&r, "mig", false).unwrap();
    assert_eq!(l.file, ".claude/settings.local.json");
    let v = json_of(&d.join(l.file));
    let deny: Vec<&str> = v["permissions"]["deny"]
      .as_array()
      .unwrap()
      .iter()
      .map(|x| x.as_str().unwrap())
      .collect();
    assert!(
      deny.contains(&"Bash")
        && deny.contains(&"PowerShell")
        && deny.contains(&"Monitor")
        && deny.contains(&"Read(./.env)")
    );
    assert_eq!(deny.iter().filter(|x| **x == "Bash").count(), 1);
    let allow: Vec<&str> = v["permissions"]["allow"]
      .as_array()
      .unwrap()
      .iter()
      .map(|x| x.as_str().unwrap())
      .collect();
    assert!(allow.contains(&"mcp__mig__run") && allow.contains(&"WebFetch"));
    assert_eq!(v["model"], "opus");
    assert!(state(&r, "mig").by_mig);
    assert!(lock(&r, "mig", false).is_err());

    unlock(&r).unwrap();
    let v = json_of(&d.join(".claude/settings.local.json"));
    // The person's own "Bash" deny stays; everything mig added is gone.
    assert_eq!(v["permissions"]["deny"], json!(["Read(./.env)", "Bash"]));
    assert_eq!(v["permissions"]["allow"], json!(["WebFetch"]));
    assert_eq!(v["env"]["A"], "1");
    assert!(!state(&r, "mig").by_mig);
    let _ = std::fs::remove_dir_all(d);
  }

  #[test]
  fn a_file_mig_created_is_removed_again() {
    let (d, r) = repo();
    lock(&r, "mig", false).unwrap();
    let p = d.join(".claude/settings.local.json");
    assert!(p.exists());
    assert!(r.is_path_ignored(".claude/settings.local.json").unwrap());
    assert_eq!(state(&r, "mig").files, vec![".claude/settings.local.json".to_string()]);
    unlock(&r).unwrap();
    assert!(!p.exists());
    let _ = std::fs::remove_dir_all(d);
  }

  #[test]
  fn shared_settings_and_server_names() {
    let (d, r) = repo();
    lock(&r, "mig-app", true).unwrap();
    let v = json_of(&d.join(".claude/settings.json"));
    assert!(
      v["permissions"]["allow"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x == "mcp__mig-app__run_script")
    );
    unlock(&r).unwrap();
    assert!(lock(&r, "bad name!", false).is_err());
    let _ = std::fs::remove_dir_all(d);
  }

  #[test]
  fn broken_settings_are_left_alone() {
    let (d, r) = repo();
    std::fs::create_dir_all(d.join(".claude")).unwrap();
    std::fs::write(d.join(".claude/settings.local.json"), "{ not json").unwrap();
    assert!(lock(&r, "mig", false).is_err());
    assert_eq!(
      std::fs::read_to_string(d.join(".claude/settings.local.json")).unwrap(),
      "{ not json"
    );
    let _ = std::fs::remove_dir_all(d);
  }
}
