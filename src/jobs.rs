// Runs that go on while the agent does something else: `run` with background = true, or with a
// wait_secs that runs out. The output is in a file (see outlog) that anyone can read at any time;
// this module keeps the running threads, sends job events, and hands out the output.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use git2::Repository;

use crate::events::Hub;
use crate::outfmt::{self, Filter};
use crate::runtool::LineSink;
use crate::{agent, outlog, runtool};

const BATCH_BYTES: usize = 4096;
const BATCH_EVERY: Duration = Duration::from_millis(1000);
const KEEP_JOBS: usize = 20;

struct Job {
  id: String,
  root: PathBuf,
  end: Mutex<Option<String>>,
  result: Mutex<Option<String>>,
}

fn table() -> &'static Mutex<HashMap<String, Arc<Job>>> {
  static T: OnceLock<Mutex<HashMap<String, Arc<Job>>>> = OnceLock::new();
  T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
  m.lock().unwrap_or_else(|e| e.into_inner())
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
  let meta = std::collections::BTreeMap::from([
    ("job".to_string(), job.id.clone()),
    ("state".to_string(), state.to_string()),
  ]);
  hub.push(&job.root, name, "job", actor.clone(), None, text, meta, None);
}

// Output lines go to the agent as events, about once a second and at most 4 KB at a time.
struct Batch {
  unsent: Vec<String>,
  last: Instant,
}

fn take(b: &mut Batch) -> Option<String> {
  if b.unsent.is_empty() {
    return None;
  }
  let folded = outfmt::fold_repeats(&std::mem::take(&mut b.unsent)).join("\n");
  b.last = Instant::now();
  Some(clip(&folded, BATCH_BYTES))
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
  let id = prepared.id().to_string();
  let job = Arc::new(Job {
    id: id.clone(),
    root: root.clone(),
    end: Mutex::new(None),
    result: Mutex::new(None),
  });
  {
    let mut t = lock(table());
    if t.len() >= KEEP_JOBS {
      t.retain(|_, j| lock(&j.end).is_none());
    }
    if t.len() >= KEEP_JOBS {
      bail!("too many background jobs are running; stop one with run_stop");
    }
    t.insert(id.clone(), job.clone());
  }
  let actor = agent.map(|a| a.name.clone());
  let batch = Arc::new(Mutex::new(Batch {
    unsent: Vec::new(),
    last: Instant::now(),
  }));
  let (j2, hub2, name2, actor2, b2) = (job.clone(), hub.clone(), name.clone(), actor.clone(), batch.clone());
  let sink: LineSink = Arc::new(move |lines: Vec<String>| {
    let ready = {
      let mut b = lock(&b2);
      b.unsent.extend(lines);
      let size: usize = b.unsent.iter().map(String::len).sum();
      if size >= BATCH_BYTES || b.last.elapsed() >= BATCH_EVERY {
        take(&mut b)
      } else {
        None
      }
    };
    if let Some(t) = ready {
      send(&hub2, &j2, &name2, &actor2, format!("job {}:\n{}", j2.id, t), "output");
    }
  });
  let git_root = root.clone();
  let summary = prepared.summary().to_string();
  let job_t = job.clone();
  std::thread::spawn(move || {
    let job = job_t;
    let result = match crate::repo::open(&git_root) {
      Ok(r) => prepared.go(&r, Some(sink), None).map(|d| d.text),
      Err(e) => Err(e),
    };
    let rest = take(&mut lock(&batch));
    if let Some(t) = rest {
      send(&hub, &job, &name, &actor, format!("job {}:\n{t}", job.id), "output");
    }
    let (headline, full) = match result {
      Ok(t) => (
        t.lines().next().unwrap_or("").to_string()
          + &t
            .lines()
            .find(|l| l.starts_with("changed:"))
            .map(|l| format!("; {l}"))
            .unwrap_or_default(),
        t,
      ),
      Err(e) => (format!("failed: {e:#}"), format!("failed: {e:#}")),
    };
    *lock(&job.result) = Some(full);
    *lock(&job.end) = Some(headline.clone());
    send(
      &hub,
      &job,
      &name,
      &actor,
      format!("job {} finished: {headline}", job.id),
      "done",
    );
  });
  // The output file is made by the thread; a reader that asks at once must find it.
  let git = repo.path().to_path_buf();
  for _ in 0..60 {
    if outlog::meta(&git, &id).is_some() || lock(&job.end).is_some() {
      break;
    }
    std::thread::sleep(Duration::from_millis(50));
  }
  Ok(format!(
    "started job {id}: {}\nOutput arrives as events; run_output reads it (with grep, head, tail), run_stop stops it.",
    clip(&summary, 160)
  ))
}

// Waits up to `secs` for the job to end. Some(result) when it did.
pub fn wait(root: &Path, id: &str, secs: u64) -> Option<String> {
  let job = lock(table()).get(id).filter(|j| j.root == root).cloned()?;
  let until = Instant::now() + Duration::from_secs(secs);
  loop {
    if let Some(r) = lock(&job.result).clone() {
      return Some(r);
    }
    if Instant::now() >= until {
      return None;
    }
    std::thread::sleep(Duration::from_millis(100));
  }
}

pub struct Slice {
  pub since: usize,
  pub from: Option<usize>,
  pub to: Option<usize>,
}

// What a run printed so far (or all of it), cut down by `filter`. Reads the file, so it works for
// finished runs, runs of other processes and runs started by a person.
pub fn output(git_dir: &Path, id: &str, slice: &Slice, filter: &Filter, limit: usize) -> Result<String> {
  let all = outlog::lines(git_dir, id)?;
  let m = outlog::meta(git_dir, id).ok_or_else(|| anyhow!("no run {id}"))?;
  let total = all.len();
  let (a, b) = match (slice.from, slice.to) {
    (None, None) => (slice.since.min(total), total),
    (f, t) => (
      f.unwrap_or(1).saturating_sub(1).min(total),
      t.unwrap_or(total).min(total),
    ),
  };
  let part = &all[a..b.max(a)];
  let ok = !m.running() && m.exit == Some(0);
  let shown = outfmt::render_from(part, filter, ok, limit, a);
  let state = if m.running() {
    format!("running {:.0}s", (crate::live::now_ms() - m.ts) as f64 / 1000.0)
  } else {
    format!("{} in {:.1}s", m.headline(), m.secs)
  };
  let mut out = shown.text;
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  Ok(format!(
    "{out}[lines {}-{} of {total}; {state}; next since={b}]",
    a + 1,
    b.max(a)
  ))
}

pub fn stop(git_dir: &Path, id: &str) -> Result<String> {
  outlog::request_stop(git_dir, id)?;
  Ok("stopping; the end is reported as an event".into())
}

pub fn list(git_dir: &Path) -> String {
  let rows: Vec<String> = outlog::list(git_dir)
    .into_iter()
    .take(15)
    .map(|m| {
      format!(
        "{}  {}  {} lines  {}{}",
        m.id,
        m.headline(),
        m.lines,
        clip(&m.summary, 90),
        m.agent.map(|a| format!("  [{a}]")).unwrap_or_default()
      )
    })
    .collect();
  if rows.is_empty() {
    "no runs yet".into()
  } else {
    rows.join("\n")
  }
}
