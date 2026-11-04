// Every run writes its output to a file while it goes, so anyone (the agent through run_output,
// a person in the GUI's Console, `mig run tail`) can read it before the run ends, and read the
// parts that were not handed back. Files live in .git/mig/out/ and the newest few are kept.
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::{outfmt, scan};

const KEEP_FILES: usize = 40;
const MAX_BYTES: usize = 4 << 20;
const CODE_MAX: usize = 20_000;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Meta {
  pub id: String,
  // Unix milliseconds.
  pub ts: i64,
  pub agent: Option<String>,
  pub summary: String,
  // "running", "done"
  pub state: String,
  pub exit: Option<i32>,
  pub timed_out: bool,
  pub cancelled: bool,
  pub secs: f64,
  pub lines: usize,
  // What was run and how, for the detail views (all optional: older runs have none of it).
  #[serde(default)]
  pub kind: String,
  #[serde(default)]
  pub lang: Option<String>,
  #[serde(default)]
  pub code: String,
  #[serde(default)]
  pub cwd: String,
  #[serde(default)]
  pub timeout_secs: u64,
  // What mig's check found (warnings and notes), as sentences.
  #[serde(default)]
  pub notes: Vec<String>,
  #[serde(default)]
  pub changed: Vec<String>,
  #[serde(default)]
  pub checkpoint: Option<String>,
}

// What a run is, known before it starts.
#[derive(Default, Clone)]
pub struct Info {
  pub kind: String,
  pub lang: Option<String>,
  pub code: String,
  pub cwd: String,
  pub timeout_secs: u64,
  pub notes: Vec<String>,
}

impl Meta {
  pub fn running(&self) -> bool {
    self.state == "running"
  }

  pub fn headline(&self) -> String {
    if self.running() {
      "running".into()
    } else if self.cancelled {
      "stopped".into()
    } else if self.timed_out {
      "timed out".into()
    } else {
      match self.exit {
        Some(0) => "ok".into(),
        Some(c) => format!("exit {c}"),
        None => "ended".into(),
      }
    }
  }
}

pub fn dir(git_dir: &Path) -> PathBuf {
  git_dir.join("mig").join("out")
}

fn log_file(git_dir: &Path, id: &str) -> PathBuf {
  dir(git_dir).join(format!("{id}.log"))
}

fn meta_file(git_dir: &Path, id: &str) -> PathBuf {
  dir(git_dir).join(format!("{id}.json"))
}

fn stop_file(git_dir: &Path, id: &str) -> PathBuf {
  dir(git_dir).join(format!("{id}.stop"))
}

fn valid(id: &str) -> bool {
  !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())
}

fn write_meta(git_dir: &Path, m: &Meta) {
  if let Ok(t) = serde_json::to_string(m) {
    let tmp = dir(git_dir).join(format!("{}.json.tmp", m.id));
    if std::fs::write(&tmp, t).is_ok() {
      let _ = std::fs::rename(&tmp, meta_file(git_dir, &m.id));
    }
  }
}

pub fn meta(git_dir: &Path, id: &str) -> Option<Meta> {
  if !valid(id) {
    return None;
  }
  serde_json::from_str(&std::fs::read_to_string(meta_file(git_dir, id)).ok()?).ok()
}

// Newest first.
pub fn list(git_dir: &Path) -> Vec<Meta> {
  let mut out: Vec<Meta> = std::fs::read_dir(dir(git_dir))
    .map(|rd| {
      rd.filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
        .collect()
    })
    .unwrap_or_default();
  out.sort_by_key(|m: &Meta| std::cmp::Reverse(m.ts));
  out
}

fn prune(git_dir: &Path) {
  let all = list(git_dir);
  for m in all.iter().skip(KEEP_FILES).filter(|m| !m.running()) {
    for f in [
      log_file(git_dir, &m.id),
      meta_file(git_dir, &m.id),
      stop_file(git_dir, &m.id),
    ] {
      let _ = std::fs::remove_file(f);
    }
  }
}

// What a running program prints, cut into clean, masked lines and appended to the file.
pub struct Writer {
  git_dir: PathBuf,
  inner: Mutex<Inner>,
}

struct Inner {
  meta: Meta,
  partial: Vec<u8>,
  // A carriage return was seen: the next byte decides whether it was a line end or an overwrite.
  cr: bool,
  bytes: usize,
  full: bool,
}

pub fn create(git_dir: &Path, id: &str, agent: Option<&str>, summary: &str, info: Info) -> Result<Writer> {
  std::fs::create_dir_all(dir(git_dir))?;
  prune(git_dir);
  let meta = Meta {
    id: id.to_string(),
    ts: crate::live::now_ms(),
    agent: agent.map(str::to_string),
    summary: summary.chars().take(200).collect(),
    state: "running".into(),
    kind: info.kind,
    lang: info.lang,
    code: scan::redact(&info.code.chars().take(CODE_MAX).collect::<String>()).0,
    cwd: info.cwd,
    timeout_secs: info.timeout_secs,
    notes: info.notes,
    ..Default::default()
  };
  std::fs::write(log_file(git_dir, id), b"")?;
  write_meta(git_dir, &meta);
  Ok(Writer {
    git_dir: git_dir.to_path_buf(),
    inner: Mutex::new(Inner {
      meta,
      partial: Vec::new(),
      cr: false,
      bytes: 0,
      full: false,
    }),
  })
}

impl Writer {
  fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
    self.inner.lock().unwrap_or_else(|e| e.into_inner())
  }

  // Returns the lines this chunk completed.
  pub fn feed(&self, bytes: &[u8]) -> Vec<String> {
    let mut g = self.lock();
    let mut done: Vec<String> = Vec::new();
    for &b in bytes {
      if g.cr {
        g.cr = false;
        if b != b'\n' {
          // "\r" alone: the line is being redrawn, so what was there is dropped.
          g.partial.clear();
        }
      }
      match b {
        b'\n' => {
          let line = String::from_utf8_lossy(&std::mem::take(&mut g.partial)).into_owned();
          done.push(tidy(&line));
        }
        b'\r' => g.cr = true,
        _ => g.partial.push(b),
      }
    }
    self.append(&mut g, &done);
    done
  }

  fn append(&self, g: &mut Inner, lines: &[String]) {
    if lines.is_empty() {
      return;
    }
    g.meta.lines += lines.len();
    if g.full {
      return;
    }
    let mut text = String::new();
    for l in lines {
      text.push_str(l);
      text.push('\n');
    }
    g.bytes += text.len();
    if g.bytes > MAX_BYTES {
      g.full = true;
      text.push_str(&format!(
        "… more than {} MB of output; the rest was not stored\n",
        MAX_BYTES >> 20
      ));
    }
    if let Ok(mut f) = OpenOptions::new()
      .append(true)
      .open(log_file(&self.git_dir, &g.meta.id))
    {
      let _ = f.write_all(text.as_bytes());
    }
  }

  pub fn finish(&self, exit: Option<i32>, timed_out: bool, cancelled: bool, secs: f64) -> Vec<String> {
    let mut g = self.lock();
    let mut rest = Vec::new();
    if !g.partial.is_empty() {
      let line = String::from_utf8_lossy(&std::mem::take(&mut g.partial)).into_owned();
      rest.push(tidy(&line));
      self.append(&mut g, &rest);
    }
    g.meta.state = "done".into();
    g.meta.exit = exit;
    g.meta.timed_out = timed_out;
    g.meta.cancelled = cancelled;
    g.meta.secs = secs;
    write_meta(&self.git_dir, &g.meta);
    let _ = std::fs::remove_file(stop_file(&self.git_dir, &g.meta.id));
    rest
  }
}

fn tidy(line: &str) -> String {
  outfmt::clip_line(&scan::redact(&outfmt::strip_ansi(line)).0)
}

pub fn lines(git_dir: &Path, id: &str) -> Result<Vec<String>> {
  if !valid(id) || meta(git_dir, id).is_none() {
    return Err(anyhow!(
      "no output {id} (the newest {KEEP_FILES} are kept, and only for this repository)"
    ));
  }
  let text = std::fs::read(log_file(git_dir, id))?;
  Ok(String::from_utf8_lossy(&text).lines().map(str::to_string).collect())
}

pub fn request_stop(git_dir: &Path, id: &str) -> Result<()> {
  let m = meta(git_dir, id).ok_or_else(|| anyhow!("no run {id}"))?;
  if !m.running() {
    return Err(anyhow!("{id} has already finished"));
  }
  std::fs::write(stop_file(git_dir, id), b"")?;
  Ok(())
}

// Notes which files the run changed (after it ended).
pub fn set_changed(git_dir: &Path, id: &str, changed: &[String], checkpoint: Option<String>) {
  if let Some(mut m) = meta(git_dir, id) {
    m.changed = changed.to_vec();
    m.checkpoint = checkpoint;
    write_meta(git_dir, &m);
  }
}

pub fn stop_requested(git_dir: &Path, id: &str) -> bool {
  stop_file(git_dir, id).exists()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn tmp() -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
      "mig-outlog-{}-{}",
      std::process::id(),
      N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
  }

  #[test]
  fn lines_are_cleaned_and_stored_while_running() {
    let git = tmp();
    let w = create(&git, "abc123", Some("a"), "demo", Info::default()).unwrap();
    w.feed(b"one\n\x1b[31mtwo\x1b[0m\nprogress 10%\rprogress 50%\rprogress 100%\nthr");
    assert_eq!(lines(&git, "abc123").unwrap(), vec!["one", "two", "progress 100%"]);
    assert!(meta(&git, "abc123").unwrap().running());
    w.feed(b"ee\n");
    w.finish(Some(0), false, false, 1.0);
    assert_eq!(lines(&git, "abc123").unwrap().last().unwrap(), "three");
    assert_eq!(meta(&git, "abc123").unwrap().headline(), "ok");
  }

  #[test]
  fn secrets_are_masked_in_the_file() {
    let git = tmp();
    let w = create(&git, "s1", None, "x", Info::default()).unwrap();
    w.feed(format!("key ghp_{}\n", "abcdefghijklmnopqrstuvwxyz0123456789").as_bytes());
    assert!(!lines(&git, "s1").unwrap()[0].contains("ghp_abc"));
  }

  #[test]
  fn stop_request_is_seen_and_ids_are_checked() {
    let git = tmp();
    let _w = create(&git, "r1", None, "x", Info::default()).unwrap();
    request_stop(&git, "r1").unwrap();
    assert!(stop_requested(&git, "r1"));
    assert!(lines(&git, "../x").is_err());
  }
}
