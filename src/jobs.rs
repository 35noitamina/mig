// Runs that go on in the background: `run` with background = true returns at once with a job id,
// output arrives as events (and can be read again with run_output), and the end is reported too.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use git2::Repository;

use crate::events::Hub;
use crate::runner::Sink;
use crate::{agent, repo, runtool, scan};

const KEEP_BYTES: usize = 256 * 1024;
const BATCH_BYTES: usize = 4096;
const BATCH_EVERY: Duration = Duration::from_millis(1000);
const KEEP_JOBS: usize = 20;

struct Buf {
  data: VecDeque<u8>,
  // Offset of data[0] in everything the job ever printed.
  base: usize,
  // A line that is not finished yet.
  partial: Vec<u8>,
  // Lines not yet sent as an event.
  unsent: String,
  last_sent: Instant,
}

pub struct Job {
  id: String,
  root: PathBuf,
  summary: String,
  started: Instant,
  buf: Mutex<Buf>,
  cancel: Arc<AtomicBool>,
  end: Mutex<Option<String>>,
}

fn table() -> &'static Mutex<HashMap<String, Arc<Job>>> {
  static T: OnceLock<Mutex<HashMap<String, Arc<Job>>>> = OnceLock::new();
  T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
  m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Buf {
  fn add_line(&mut self, line: &str) {
    let clean = scan::redact(line).0;
    self.data.extend(clean.bytes());
    self.data.push_back(b'\n');
    while self.data.len() > KEEP_BYTES {
      self.data.pop_front();
      self.base += 1;
    }
    self.unsent.push_str(&clean);
    self.unsent.push('\n');
  }

  fn feed(&mut self, bytes: &[u8]) {
    for &b in bytes {
      if b == b'\n' || b == b'\r' {
        if !self.partial.is_empty() || b == b'\n' {
          let line = String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned();
          self.add_line(&line);
        }
      } else {
        self.partial.push(b);
      }
    }
  }

  fn finish(&mut self) {
    if !self.partial.is_empty() {
      let line = String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned();
      self.add_line(&line);
    }
  }
}

fn clip(s: &str, n: usize) -> String {
  if s.len() <= n {
    return s.to_string();
  }
  let mut end = n;
  while !s.is_char_boundary(end) {
    end -= 1;
  }
  format!("{}…", &s[..end])
}

fn send(hub: &Hub, job: &Job, name: &str, actor: &Option<String>, text: String, state: &str) {
  let mut meta = BTreeMap::new();
  meta.insert("job".to_string(), job.id.clone());
  meta.insert("state".to_string(), state.to_string());
  hub.push(&job.root, name, "job", actor.clone(), None, text, meta, None);
}

// Starts the program on a thread of its own and returns the job id. Checks and approval happen
// first, here, so a refusal comes back as an ordinary error.
pub fn start(
  repo: &Repository,
  agent: Option<&agent::Session>,
  req: runtool::Request,
  hub: Arc<Hub>,
  name: String,
) -> Result<String> {
  let root = repo
    .workdir()
    .ok_or_else(|| anyhow!("this repository has no working folder"))?
    .to_path_buf();
  let prepared = runtool::prepare(repo, agent, req)?;
  let id = crate::approval::new_id();
  let job = Arc::new(Job {
    id: id.clone(),
    root: root.clone(),
    summary: clip(prepared.summary(), 160),
    started: Instant::now(),
    buf: Mutex::new(Buf {
      data: VecDeque::new(),
      base: 0,
      partial: Vec::new(),
      unsent: String::new(),
      last_sent: Instant::now(),
    }),
    cancel: Arc::new(AtomicBool::new(false)),
    end: Mutex::new(None),
  });
  {
    let mut t = lock(table());
    if t.len() >= KEEP_JOBS {
      let finished: Vec<String> = t
        .iter()
        .filter(|(_, j)| lock(&j.end).is_some())
        .map(|(k, _)| k.clone())
        .collect();
      for k in finished {
        t.remove(&k);
      }
    }
    if t.len() >= KEEP_JOBS {
      bail!("too many background jobs are running; stop one with run_stop");
    }
    t.insert(id.clone(), job.clone());
  }
  let actor = agent.map(|a| a.name.clone());
  let (j2, hub2, name2, actor2) = (job.clone(), hub.clone(), name.clone(), actor.clone());
  let sink: Sink = Arc::new(move |bytes: &[u8]| {
    let ready = {
      let mut b = lock(&j2.buf);
      b.feed(bytes);
      if !b.unsent.is_empty() && (b.unsent.len() >= BATCH_BYTES || b.last_sent.elapsed() >= BATCH_EVERY) {
        b.last_sent = Instant::now();
        Some(clip(&std::mem::take(&mut b.unsent), BATCH_BYTES))
      } else {
        None
      }
    };
    if let Some(t) = ready {
      send(
        &hub2,
        &j2,
        &name2,
        &actor2,
        format!("job {}:\n{}", j2.id, t.trim_end()),
        "output",
      );
    }
  });
  let cancel = job.cancel.clone();
  let git_root = root.clone();
  std::thread::spawn(move || {
    let result = match repo::open(&git_root) {
      Ok(r) => prepared.go(&r, Some(sink), Some(cancel)).map(|d| d.text),
      Err(e) => Err(e),
    };
    let rest = {
      let mut b = lock(&job.buf);
      b.finish();
      std::mem::take(&mut b.unsent)
    };
    if !rest.trim().is_empty() {
      send(
        &hub,
        &job,
        &name,
        &actor,
        format!("job {}:\n{}", job.id, clip(rest.trim_end(), BATCH_BYTES)),
        "output",
      );
    }
    let text = match result {
      Ok(t) => t.lines().next().unwrap_or("").to_string() + &tail_notes(&t),
      Err(e) => format!("failed: {e:#}"),
    };
    *lock(&job.end) = Some(text.clone());
    send(
      &hub,
      &job,
      &name,
      &actor,
      format!("job {} finished: {text}", job.id),
      "done",
    );
  });
  Ok(format!(
    "started job {id}: {}\nOutput arrives as events; run_output reads it, run_stop stops it.",
    lock(table()).get(&id).map(|j| j.summary.clone()).unwrap_or_default()
  ))
}

// The "changed: …" line is worth keeping in the end report.
fn tail_notes(text: &str) -> String {
  text
    .lines()
    .find(|l| l.starts_with("changed:"))
    .map(|l| format!("; {l}"))
    .unwrap_or_default()
}

fn find(root: &Path, id: &str) -> Result<Arc<Job>> {
  lock(table())
    .get(id)
    .filter(|j| j.root == root)
    .cloned()
    .ok_or_else(|| anyhow!("no job {id} in this folder (jobs are forgotten when the server restarts)"))
}

pub fn output(root: &Path, id: &str, since: usize) -> Result<String> {
  let job = find(root, id)?;
  let b = lock(&job.buf);
  let from = since.max(b.base);
  let total = b.base + b.data.len();
  let bytes: Vec<u8> = b.data.iter().skip(from - b.base).copied().collect();
  let mut out = String::new();
  if since < b.base {
    out.push_str(&format!(
      "… {} bytes were dropped (only the latest {} KB are kept) …\n",
      b.base - since,
      KEEP_BYTES / 1024
    ));
  }
  out.push_str(&String::from_utf8_lossy(&bytes));
  let state = match &*lock(&job.end) {
    Some(e) => format!("finished: {e}"),
    None => format!("running for {:.0}s", job.started.elapsed().as_secs_f64()),
  };
  Ok(format!(
    "{}[{state}; next since={total}]",
    if out.is_empty() || out.ends_with('\n') {
      out
    } else {
      out + "\n"
    }
  ))
}

pub fn stop(root: &Path, id: &str) -> Result<String> {
  let job = find(root, id)?;
  if lock(&job.end).is_some() {
    return Ok("already finished".into());
  }
  job.cancel.store(true, Ordering::Relaxed);
  Ok("stopping; the end is reported as an event".into())
}

pub fn list(root: &Path) -> String {
  let t = lock(table());
  let mut rows: Vec<String> = t
    .values()
    .filter(|j| j.root == root)
    .map(|j| {
      let state = match &*lock(&j.end) {
        Some(e) => format!("finished: {}", clip(e, 80)),
        None => format!("running {:.0}s", j.started.elapsed().as_secs_f64()),
      };
      format!("{}  {state}  {}", j.id, j.summary)
    })
    .collect();
  rows.sort();
  if rows.is_empty() {
    "no background jobs".into()
  } else {
    rows.join("\n")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn lines_are_split_and_secrets_masked() {
    let mut b = Buf {
      data: VecDeque::new(),
      base: 0,
      partial: Vec::new(),
      unsent: String::new(),
      last_sent: Instant::now(),
    };
    b.feed(b"one\ntw");
    b.feed(b"o\r\nghp_abcdefghijklmnopqrstuvwxyz0123456789\nlast");
    b.finish();
    let text = String::from_utf8_lossy(&b.data.iter().copied().collect::<Vec<_>>()).into_owned();
    assert!(text.starts_with("one\ntwo\n"));
    assert!(!text.contains("ghp_abc"));
    assert!(text.ends_with("last\n"));
  }
}
