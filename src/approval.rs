// Runs that wait for a person. With `mode = "ask"` (or "strict") in .mig.toml, a command the check
// warns about is not run at once: it is put in .git/mig/run/pending, and a person decides in the GUI
// (Approvals) or with `mig run approve` / `mig run deny`. The agent's call waits for the answer.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::runcheck::{Finding, Level};

const STALE_SECS: i64 = 3600;
const POLL: Duration = Duration::from_millis(200);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Note {
  pub rule: String,
  pub level: String,
  pub what: String,
  pub line: usize,
  pub snippet: String,
}

impl From<&Finding> for Note {
  fn from(f: &Finding) -> Note {
    Note {
      rule: f.rule.to_string(),
      level: match f.level {
        Level::Block => "block",
        Level::Warn => "warn",
        Level::Note => "note",
      }
      .into(),
      what: f.what.clone(),
      line: f.line,
      snippet: f.snippet.clone(),
    }
  }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Pending {
  pub id: String,
  // Unix seconds.
  pub ts: i64,
  pub agent: Option<String>,
  // "argv" or "script"
  pub kind: String,
  pub lang: Option<String>,
  // The command line, or the whole script.
  pub code: String,
  pub cwd: String,
  pub notes: Vec<Note>,
  // Seconds the agent's call keeps waiting.
  pub wait_secs: u64,
  // "pending", "approved", "always" or "denied"
  pub status: String,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Decision {
  Approve,
  Always,
  Deny,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
  Approved,
  Denied,
  TimedOut,
}

fn dir(git_dir: &Path) -> PathBuf {
  git_dir.join("mig").join("run").join("pending")
}

fn file(git_dir: &Path, id: &str) -> PathBuf {
  dir(git_dir).join(format!("{id}.json"))
}

fn always_file(git_dir: &Path) -> PathBuf {
  git_dir.join("mig").join("run-allow.json")
}

pub fn new_id() -> String {
  static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  let n = crate::live::now_ms() as u64 ^ (u64::from(std::process::id()) << 20);
  let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  format!("{:06x}", (n.wrapping_mul(31).wrapping_add(c * 7919)) % 0xff_ffff)
}

// What "always allow" remembers: the kind, the language and the exact text.
pub fn key(kind: &str, lang: Option<&str>, code: &str) -> String {
  crate::agent::hash(format!("{kind}\u{0}{}\u{0}{code}", lang.unwrap_or("")).as_bytes())
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
  if let Some(d) = path.parent() {
    std::fs::create_dir_all(d)?;
  }
  let tmp = path.with_extension("tmp");
  std::fs::write(&tmp, text)?;
  std::fs::rename(tmp, path)?;
  Ok(())
}

pub fn create(git_dir: &Path, p: &Pending) -> Result<()> {
  write_atomic(&file(git_dir, &p.id), &serde_json::to_string_pretty(p)?)
}

fn read(path: &Path) -> Option<Pending> {
  serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub fn remove(git_dir: &Path, id: &str) {
  let _ = std::fs::remove_file(file(git_dir, id));
}

// Requests nobody has answered yet, oldest first. Very old leftovers (a crashed server) are removed.
pub fn pending(git_dir: &Path) -> Vec<Pending> {
  let now = chrono::Utc::now().timestamp();
  let mut out: Vec<Pending> = Vec::new();
  let Ok(rd) = std::fs::read_dir(dir(git_dir)) else {
    return out;
  };
  for e in rd.flatten() {
    if e.path().extension().is_none_or(|x| x != "json") {
      continue;
    }
    let Some(p) = read(&e.path()) else { continue };
    if now - p.ts > STALE_SECS.max(p.wait_secs as i64 * 2) {
      let _ = std::fs::remove_file(e.path());
    } else if p.status == "pending" && now - p.ts <= p.wait_secs as i64 + 5 {
      out.push(p);
    }
  }
  out.sort_by_key(|p| p.ts);
  out
}

pub fn decide(git_dir: &Path, id: &str, d: Decision) -> Result<()> {
  let path = file(git_dir, id);
  let mut p = read(&path).ok_or_else(|| anyhow!("no request {id} is waiting (it may have timed out)"))?;
  if p.status != "pending" {
    return Err(anyhow!("request {id} was already answered"));
  }
  p.status = match d {
    Decision::Approve => "approved",
    Decision::Always => "always",
    Decision::Deny => "denied",
  }
  .into();
  if d == Decision::Always {
    add_always(git_dir, &key(&p.kind, p.lang.as_deref(), &p.code));
  }
  write_atomic(&path, &serde_json::to_string_pretty(&p)?)
}

// Waits for an answer; the request file is removed afterwards either way.
pub fn wait(git_dir: &Path, id: &str, limit: Duration) -> Answer {
  let start = Instant::now();
  let path = file(git_dir, id);
  let answer = loop {
    match read(&path).map(|p| p.status) {
      Some(s) if s == "approved" || s == "always" => break Answer::Approved,
      Some(s) if s == "denied" => break Answer::Denied,
      None => break Answer::Denied,
      _ => {}
    }
    if start.elapsed() >= limit {
      break Answer::TimedOut;
    }
    std::thread::sleep(POLL);
  };
  remove(git_dir, id);
  answer
}

fn read_always(git_dir: &Path) -> Vec<String> {
  std::fs::read_to_string(always_file(git_dir))
    .ok()
    .and_then(|s| serde_json::from_str(&s).ok())
    .unwrap_or_default()
}

fn add_always(git_dir: &Path, k: &str) {
  let mut all = read_always(git_dir);
  if !all.iter().any(|x| x == k) {
    all.push(k.to_string());
    let _ = write_atomic(&always_file(git_dir), &serde_json::to_string(&all).unwrap_or_default());
  }
}

pub fn is_always(git_dir: &Path, k: &str) -> bool {
  read_always(git_dir).iter().any(|x| x == k)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn tmp() -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("mig-appr-{}-{}-{n}", std::process::id(), crate::live::now_ms()));
    std::fs::create_dir_all(&d).unwrap();
    d
  }

  fn sample(id: &str) -> Pending {
    Pending {
      id: id.into(),
      ts: chrono::Utc::now().timestamp(),
      agent: Some("a".into()),
      kind: "argv".into(),
      lang: None,
      code: "curl https://x".into(),
      cwd: ".".into(),
      notes: vec![],
      wait_secs: 30,
      status: "pending".into(),
    }
  }

  #[test]
  fn approve_and_deny_reach_the_waiting_call() {
    let g = tmp();
    create(&g, &sample("aaaaaa")).unwrap();
    assert_eq!(pending(&g).len(), 1);
    let g2 = g.clone();
    let t = std::thread::spawn(move || wait(&g2, "aaaaaa", Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(300));
    decide(&g, "aaaaaa", Decision::Approve).unwrap();
    assert_eq!(t.join().unwrap(), Answer::Approved);
    assert!(pending(&g).is_empty());

    create(&g, &sample("bbbbbb")).unwrap();
    let g2 = g.clone();
    let t = std::thread::spawn(move || wait(&g2, "bbbbbb", Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(300));
    decide(&g, "bbbbbb", Decision::Deny).unwrap();
    assert_eq!(t.join().unwrap(), Answer::Denied);
    let _ = std::fs::remove_dir_all(g);
  }

  #[test]
  fn unanswered_requests_time_out_and_disappear() {
    let g = tmp();
    create(&g, &sample("cccccc")).unwrap();
    assert_eq!(wait(&g, "cccccc", Duration::from_millis(500)), Answer::TimedOut);
    assert!(pending(&g).is_empty());
    assert!(decide(&g, "cccccc", Decision::Approve).is_err());
    let _ = std::fs::remove_dir_all(g);
  }

  #[test]
  fn always_allow_is_remembered_by_exact_text() {
    let g = tmp();
    let p = sample("dddddd");
    create(&g, &p).unwrap();
    decide(&g, "dddddd", Decision::Always).unwrap();
    assert!(is_always(&g, &key("argv", None, "curl https://x")));
    assert!(!is_always(&g, &key("argv", None, "curl https://y")));
    assert!(!is_always(&g, &key("script", Some("bash"), "curl https://x")));
    let _ = std::fs::remove_dir_all(g);
  }
}
