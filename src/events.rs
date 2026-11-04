// What happens around an agent, as events: a person's note, another agent starting or ending, a
// run by someone else, a new commit, a file written by another tool, a timer, a webhook. The MCP
// server keeps them here. An AI can wait for them with `wait_events`, and, when Claude Code loads
// mig as a channel, they are also pushed into the session as they happen. Events say what happened,
// not what is inside a file, so pushed text is short and carries no outside content.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use globset::Glob;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::livewatch::{self, Kind as FileKind, Msg};
use crate::{agent, inbox, repo, runtool};

pub const DEFAULT_KINDS: &[&str] = &["message", "agent", "run", "commit"];
pub const ALL_KINDS: &[&str] = &["message", "agent", "run", "commit", "file", "timer", "webhook"];
const KEEP: usize = 500;
const PUSH_PER_MINUTE: usize = 30;
const POLL: Duration = Duration::from_millis(500);
const BODY_MAX: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct Event {
  pub id: u64,
  pub ts: i64,
  pub kind: String,
  pub repo: String,
  pub root: PathBuf,
  pub actor: Option<String>,
  pub path: Option<String>,
  pub text: String,
  pub meta: BTreeMap<String, String>,
  // Kept for `events` with an id; never pushed.
  pub body: Option<String>,
  shown: bool,
}

#[derive(Clone, Debug)]
pub struct Sub {
  pub kinds: BTreeSet<String>,
  // Globs for file events; empty means every path.
  pub paths: Vec<String>,
  // Also report what this agent itself did.
  pub include_own: bool,
  // Push into the session as a channel (events are kept for wait_events either way).
  pub push: bool,
  pub quiet_secs: u64,
  pub every_secs: Option<u64>,
  pub webhook: bool,
}

impl Default for Sub {
  fn default() -> Sub {
    Sub {
      kinds: DEFAULT_KINDS.iter().map(|s| s.to_string()).collect(),
      paths: Vec::new(),
      include_own: false,
      push: true,
      quiet_secs: 2,
      every_secs: None,
      webhook: false,
    }
  }
}

pub type Notifier = Arc<dyn Fn(Value) + Send + Sync>;

struct Inner {
  next: u64,
  events: VecDeque<Event>,
  subs: HashMap<PathBuf, Sub>,
  cursor: HashMap<PathBuf, u64>,
  me: HashMap<PathBuf, String>,
  started: HashSet<PathBuf>,
  files_on: HashSet<PathBuf>,
  webhooks: HashMap<PathBuf, String>,
  pushes: HashMap<PathBuf, VecDeque<Instant>>,
  left_out: HashMap<PathBuf, u32>,
  file_last: HashMap<(PathBuf, String), Instant>,
  own_at: HashMap<PathBuf, Instant>,
}

pub struct Hub {
  started_at: i64,
  inner: Mutex<Inner>,
  cv: Condvar,
  notify: Notifier,
}

fn clock(ts: i64) -> String {
  chrono::DateTime::from_timestamp(ts, 0)
    .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
    .unwrap_or_default()
}

fn short(s: &str, n: usize) -> String {
  let mut t: String = s.chars().take(n).collect();
  if s.chars().count() > n {
    t.push('…');
  }
  t
}

pub fn format_event(e: &Event) -> String {
  let who = e.actor.as_deref().map(|a| format!(" [{a}]")).unwrap_or_default();
  format!("#{} {} {}{who}: {}", e.id, clock(e.ts), e.kind, e.text)
}

fn notification(e: &Event) -> Value {
  let mut meta = serde_json::Map::new();
  meta.insert("kind".into(), json!(e.kind));
  meta.insert("repo".into(), json!(e.repo));
  meta.insert("event_id".into(), json!(e.id.to_string()));
  if let Some(a) = &e.actor {
    meta.insert("agent".into(), json!(a));
  }
  if let Some(p) = &e.path {
    meta.insert("path".into(), json!(p));
  }
  for (k, v) in &e.meta {
    if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
      meta.insert(k.clone(), json!(v));
    }
  }
  json!({"jsonrpc": "2.0", "method": "notifications/claude/channel", "params": {"content": e.text, "meta": meta}})
}

fn random_token() -> String {
  use std::hash::{BuildHasher, Hasher};
  let mut out = String::new();
  for _ in 0..2 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(crate::live::now_ms() as u128);
    out.push_str(&format!("{:016x}", h.finish()));
  }
  out
}

// What the GUI's Monitor page reads: the watches that running servers hold, and the events that
// reached them. Each server writes a small file about itself (and refreshes it every few seconds,
// so a server that is gone drops out), and every event it keeps is appended to one shared log.
const LOG_KEEP: usize = 300;
const LOG_ROTATE_AT: u64 = 192 * 1024;
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
const WATCH_ALIVE_SECS: i64 = 20;
const LOG_TEXT: usize = 400;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Logged {
  pub id: u64,
  pub pid: u32,
  pub ts: i64,
  pub kind: String,
  pub actor: Option<String>,
  pub path: Option<String>,
  pub text: String,
  // The agent that received it.
  pub by: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct WatchInfo {
  pub pid: u32,
  pub agent: Option<String>,
  pub started: i64,
  // Heartbeat, Unix seconds.
  pub ts: i64,
  pub kinds: Vec<String>,
  pub paths: Vec<String>,
  pub include_own: bool,
  pub push: bool,
  pub every_secs: Option<u64>,
  pub webhook: bool,
}

fn log_file(git_dir: &Path) -> PathBuf {
  git_dir.join("mig").join("events.jsonl")
}

fn watch_dir(git_dir: &Path) -> PathBuf {
  git_dir.join("mig").join("watch")
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn read_log(git_dir: &Path, n: usize) -> Vec<Logged> {
  let all: Vec<Logged> = std::fs::read_to_string(log_file(git_dir))
    .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
    .unwrap_or_default();
  all[all.len().saturating_sub(n)..].to_vec()
}

fn append_log(git_dir: &Path, e: &Logged) {
  let file = log_file(git_dir);
  if let Some(d) = file.parent() {
    let _ = std::fs::create_dir_all(d);
  }
  if std::fs::metadata(&file).is_ok_and(|m| m.len() > LOG_ROTATE_AT) {
    let keep: Vec<String> = read_log(git_dir, LOG_KEEP)
      .iter()
      .filter_map(|e| serde_json::to_string(e).ok())
      .collect();
    let _ = std::fs::write(&file, keep.join("\n") + "\n");
  }
  if let (Ok(line), Ok(mut f)) = (
    serde_json::to_string(e),
    std::fs::OpenOptions::new().create(true).append(true).open(&file),
  ) {
    let _ = writeln!(f, "{line}");
  }
}

// The watches of servers that are still alive (newest first); files of long-gone ones are removed.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn watches(git_dir: &Path) -> Vec<WatchInfo> {
  let now = chrono::Utc::now().timestamp();
  let mut out = Vec::new();
  let Ok(rd) = std::fs::read_dir(watch_dir(git_dir)) else {
    return out;
  };
  for e in rd.filter_map(Result::ok) {
    let Some(w) = std::fs::read_to_string(e.path())
      .ok()
      .and_then(|t| serde_json::from_str::<WatchInfo>(&t).ok())
    else {
      continue;
    };
    if now - w.ts > 3600 {
      let _ = std::fs::remove_file(e.path());
    } else if now - w.ts <= WATCH_ALIVE_SECS {
      out.push(w);
    }
  }
  out.sort_by_key(|w| std::cmp::Reverse(w.started));
  out
}

impl Hub {
  // Writes (or refreshes) this server's watch file for `root`.
  fn save_watch(&self, root: &Path) {
    let (sub, me, webhook, started) = {
      let g = self.lock();
      (
        g.subs.get(root).cloned().unwrap_or_default(),
        g.me.get(root).cloned(),
        g.webhooks.contains_key(root),
        self.started_at,
      )
    };
    let info = WatchInfo {
      pid: std::process::id(),
      agent: me,
      started,
      ts: chrono::Utc::now().timestamp(),
      kinds: sub.kinds.iter().cloned().collect(),
      paths: sub.paths.clone(),
      include_own: sub.include_own,
      push: sub.push,
      every_secs: sub.every_secs,
      webhook,
    };
    let dir = watch_dir(&root.join(".git"));
    if std::fs::create_dir_all(&dir).is_ok()
      && let Ok(t) = serde_json::to_string(&info)
    {
      let _ = std::fs::write(dir.join(format!("{}.json", info.pid)), t);
    }
  }

  pub fn new(notify: Notifier) -> Arc<Hub> {
    Arc::new(Hub {
      started_at: chrono::Utc::now().timestamp(),
      inner: Mutex::new(Inner {
        next: 1,
        events: VecDeque::new(),
        subs: HashMap::new(),
        cursor: HashMap::new(),
        me: HashMap::new(),
        started: HashSet::new(),
        files_on: HashSet::new(),
        webhooks: HashMap::new(),
        pushes: HashMap::new(),
        left_out: HashMap::new(),
        file_last: HashMap::new(),
        own_at: HashMap::new(),
      }),
      cv: Condvar::new(),
      notify,
    })
  }

  fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
    self.inner.lock().unwrap_or_else(|e| e.into_inner())
  }

  // Who this server speaks for in `root`: its own doings are not reported back to it.
  pub fn set_me(&self, root: &Path, name: Option<&str>) {
    let mut g = self.lock();
    match name {
      Some(n) => g.me.insert(root.to_path_buf(), n.to_string()),
      None => g.me.remove(root),
    };
    drop(g);
    self.save_watch(root);
  }

  pub fn note_own(&self, root: &Path) {
    self.lock().own_at.insert(root.to_path_buf(), Instant::now());
  }

  pub fn sub(&self, root: &Path) -> Sub {
    self.lock().subs.get(root).cloned().unwrap_or_default()
  }

  // Starts watching `root` for notes, other agents, runs and commits (once).
  pub fn ensure_started(self: &Arc<Hub>, root: &Path, name: &str) {
    if !self.lock().started.insert(root.to_path_buf()) {
      return;
    }
    let hub = self.clone();
    let (root, name) = (root.to_path_buf(), name.to_string());
    std::thread::spawn(move || poll_loop(hub, root, name));
  }

  pub fn watch(self: &Arc<Hub>, root: &Path, name: &str, sub: Sub) -> Result<String> {
    for k in &sub.kinds {
      if !ALL_KINDS.contains(&k.as_str()) {
        bail!("unknown kind {k}; use {}", ALL_KINDS.join(", "));
      }
    }
    for p in &sub.paths {
      Glob::new(p).map_err(|e| anyhow!("bad path pattern {p}: {e}"))?;
    }
    let wants_files = sub.kinds.contains("file");
    let wants_webhook = sub.kinds.contains("webhook") || sub.webhook;
    let mut url = None;
    {
      let mut g = self.lock();
      g.subs.insert(root.to_path_buf(), sub.clone());
      if wants_files && g.files_on.insert(root.to_path_buf()) {
        let hub = self.clone();
        let (r, n) = (root.to_path_buf(), name.to_string());
        std::thread::spawn(move || file_loop(hub, r, n));
      }
      if wants_webhook {
        if let Some(t) = g.webhooks.get(root) {
          url = Some(t.clone());
        } else {
          drop(g);
          let (port, token) = start_webhook(self.clone(), root.to_path_buf(), name.to_string())?;
          let u = format!("http://127.0.0.1:{port}/?token={token}");
          self.lock().webhooks.insert(root.to_path_buf(), u.clone());
          url = Some(u);
        }
      }
    }
    self.ensure_started(root, name);
    self.save_watch(root);
    let mut out = format!(
      "watching {} (events are {}pushed to you as a channel when Claude Code loads mig as one; wait_events always works)",
      sub.kinds.iter().cloned().collect::<Vec<_>>().join(", "),
      if sub.push { "" } else { "not " }
    );
    if let Some(u) = url {
      out.push_str(&format!("\nwebhook: POST to {u}"));
    }
    Ok(out)
  }

  pub fn unwatch(&self, root: &Path) {
    let mut g = self.lock();
    g.subs.insert(
      root.to_path_buf(),
      Sub {
        kinds: BTreeSet::new(),
        push: false,
        ..Sub::default()
      },
    );
    g.files_on.remove(root);
    drop(g);
    self.save_watch(root);
  }

  fn wants_files(&self, root: &Path) -> bool {
    let g = self.lock();
    g.files_on.contains(root) && g.subs.get(root).is_some_and(|s| s.kinds.contains("file"))
  }

  // Adds an event if it passes the subscription; returns whether it was kept.
  pub fn push(
    &self,
    root: &Path,
    name: &str,
    kind: &str,
    actor: Option<String>,
    path: Option<String>,
    text: String,
    meta: BTreeMap<String, String>,
    body: Option<String>,
  ) -> bool {
    let mut out: Vec<Value> = Vec::new();
    let logged: Logged;
    {
      let mut g = self.lock();
      let sub = g.subs.get(root).cloned().unwrap_or_default();
      // A job's output always goes to the agent that started it.
      let own_kind = matches!(kind, "message" | "job");
      if !own_kind && !sub.kinds.contains(kind) {
        return false;
      }
      if !sub.include_own && !own_kind && actor.is_some() && g.me.get(root) == actor.as_ref() {
        return false;
      }
      if kind == "file"
        && !sub.paths.is_empty()
        && !path.as_deref().is_some_and(|p| {
          sub
            .paths
            .iter()
            .filter_map(|g| Glob::new(g).ok())
            .any(|g| g.compile_matcher().is_match(p))
        })
      {
        return false;
      }
      if kind == "commit" && g.own_at.get(root).is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
        return false;
      }
      let job_done = kind == "job" && meta.get("state").is_some_and(|s| s == "done");
      let id = g.next;
      g.next += 1;
      let ev = Event {
        id,
        ts: chrono::Utc::now().timestamp(),
        kind: kind.to_string(),
        repo: name.to_string(),
        root: root.to_path_buf(),
        actor,
        path,
        text,
        meta,
        body: body.map(|b| b.chars().take(BODY_MAX).collect()),
        shown: false,
      };
      // Decide about pushing: a file that was just pushed stays quiet for a while, and the number
      // of pushes per minute is limited (a person's note always goes through).
      let mut push = sub.push;
      if push && kind == "file" {
        let key = (root.to_path_buf(), ev.path.clone().unwrap_or_default());
        let quiet = Duration::from_secs(sub.quiet_secs);
        if g.file_last.get(&key).is_some_and(|t| t.elapsed() < quiet) {
          push = false;
        } else {
          g.file_last.insert(key, Instant::now());
        }
      }
      if push && job_done {
        // The end of a job is always reported.
      } else if push && kind != "message" {
        let window = g.pushes.entry(root.to_path_buf()).or_default();
        let now = Instant::now();
        while window
          .front()
          .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
        {
          window.pop_front();
        }
        if window.len() >= PUSH_PER_MINUTE {
          push = false;
          *g.left_out.entry(root.to_path_buf()).or_default() += 1;
        } else {
          window.push_back(now);
        }
      }
      if push {
        let skipped = g.left_out.remove(root).unwrap_or(0);
        if skipped > 0 {
          let note = Event {
            text: format!("{skipped} event(s) were left out of the channel to keep it quiet; wait_events has them"),
            kind: "note".into(),
            meta: BTreeMap::new(),
            actor: None,
            path: None,
            body: None,
            ..ev.clone()
          };
          out.push(notification(&note));
        }
        out.push(notification(&ev));
      }
      logged = Logged {
        id: ev.id,
        pid: std::process::id(),
        ts: ev.ts,
        kind: ev.kind.clone(),
        actor: ev.actor.clone(),
        path: ev.path.clone(),
        text: short(&ev.text, LOG_TEXT),
        by: g.me.get(root).cloned(),
      };
      g.events.push_back(ev);
      while g.events.len() > KEEP {
        g.events.pop_front();
      }
    }
    append_log(&root.join(".git"), &logged);
    self.cv.notify_all();
    for n in out {
      (self.notify)(n);
    }
    true
  }

  // Events for `root` newer than the last call; waits up to `timeout` for the first one.
  pub fn wait(&self, root: &Path, timeout: Duration, max: usize) -> Vec<Event> {
    let deadline = Instant::now() + timeout;
    let mut g = self.lock();
    loop {
      let after = g.cursor.get(root).copied().unwrap_or(0);
      let mut found: Vec<Event> = g
        .events
        .iter()
        .filter(|e| e.root == root && e.id > after)
        .cloned()
        .collect();
      if found.len() > max {
        found.drain(..found.len() - max);
      }
      if let Some(last) = found.last() {
        let last = last.id;
        g.cursor.insert(root.to_path_buf(), last);
        for e in g.events.iter_mut().filter(|e| e.root == root && e.id <= last) {
          e.shown = true;
        }
        return found;
      }
      let now = Instant::now();
      if now >= deadline {
        return Vec::new();
      }
      g = self
        .cv
        .wait_timeout(g, deadline - now)
        .unwrap_or_else(|e| e.into_inner())
        .0;
    }
  }

  pub fn recent(&self, root: &Path, since: u64, limit: usize) -> Vec<Event> {
    let g = self.lock();
    let mut v: Vec<Event> = g
      .events
      .iter()
      .filter(|e| e.root == root && e.id > since)
      .cloned()
      .collect();
    if v.len() > limit {
      v.drain(..v.len() - limit);
    }
    v
  }

  pub fn get(&self, root: &Path, id: u64) -> Option<Event> {
    self
      .lock()
      .events
      .iter()
      .find(|e| e.root == root && e.id == id)
      .cloned()
  }

  // Notes from the person that no tool result has shown yet. They are added to the next tool
  // result, so they reach the AI even when the channel is not loaded.
  pub fn take_unseen_messages(&self, root: &Path) -> Vec<Event> {
    let mut g = self.lock();
    let mut out = Vec::new();
    for e in g
      .events
      .iter_mut()
      .filter(|e| e.root == root && e.kind == "message" && !e.shown)
    {
      e.shown = true;
      out.push(e.clone());
    }
    out
  }
}

fn describe_message(m: &inbox::Message) -> String {
  let mut t = format!("Message from the user: {}", m.text);
  if let Some(f) = &m.file {
    match m.lines {
      Some((a, b)) if a == b => t.push_str(&format!("\n(about {f}, line {a})")),
      Some((a, b)) => t.push_str(&format!("\n(about {f}, lines {a}-{b})")),
      None => t.push_str(&format!("\n(about {f})")),
    }
  }
  if let Some(s) = &m.selection {
    t.push_str(&format!("\nThe user selected:\n{s}"));
  }
  t
}

fn poll_loop(hub: Arc<Hub>, root: PathBuf, name: String) {
  let Ok(repo) = repo::open(&root) else { return };
  let git = repo.path().to_path_buf();
  let mut sessions: HashMap<String, String> = agent::list(&repo).into_iter().map(|s| (s.id, s.name)).collect();
  let mut head = repo::head_commit(&repo).ok().flatten().map(|c| c.id());
  let mut log_at = std::fs::metadata(git.join("mig").join("run.jsonl")).map_or(0, |m| m.len());
  let mut last_timer = Instant::now();
  let mut tick = 0u64;
  loop {
    std::thread::sleep(POLL);
    tick += 1;
    if tick % 8 == 1 {
      hub.save_watch(&root);
    }
    let me = hub.lock().me.get(&root).cloned();
    for m in inbox::claim(&git, me.as_deref()) {
      let mut meta = BTreeMap::new();
      if let Some(f) = &m.file {
        meta.insert("file".to_string(), f.clone());
      }
      if let Some((a, b)) = m.lines {
        meta.insert("lines".to_string(), format!("{a}-{b}"));
      }
      meta.insert("from".to_string(), "user".to_string());
      hub.push(
        &root,
        &name,
        "message",
        None,
        m.file.clone(),
        describe_message(&m),
        meta,
        None,
      );
    }
    if tick % 2 == 0 {
      let now: HashMap<String, String> = agent::list(&repo).into_iter().map(|s| (s.id, s.name)).collect();
      for (id, n) in &now {
        if !sessions.contains_key(id) {
          hub.push(
            &root,
            &name,
            "agent",
            Some(n.clone()),
            None,
            format!("agent {n} started"),
            BTreeMap::from([("change".into(), "started".into())]),
            None,
          );
        }
      }
      for (id, n) in &sessions {
        if !now.contains_key(id) {
          hub.push(
            &root,
            &name,
            "agent",
            Some(n.clone()),
            None,
            format!("agent {n} ended"),
            BTreeMap::from([("change".into(), "ended".into())]),
            None,
          );
        }
      }
      sessions = now;

      let size = std::fs::metadata(git.join("mig").join("run.jsonl")).map_or(0, |m| m.len());
      if size < log_at {
        log_at = 0;
      }
      if size > log_at {
        let all = runtool::recent(&git, 50);
        let fresh = all
          .into_iter()
          .filter(|e| e.ts / 1000 >= chrono::Utc::now().timestamp() - 3 && e.pid != std::process::id());
        for e in fresh {
          let who = e.agent.clone();
          let result = match (e.verdict.as_str(), e.exit, e.timed_out) {
            (_, _, true) => "timed out".to_string(),
            ("ran", Some(c), _) => format!("exit {c}"),
            (v, _, _) => v.to_string(),
          };
          let text = format!(
            "{} ran: {} → {result}",
            who.as_deref().unwrap_or("someone"),
            short(&e.summary, 160)
          );
          hub.push(
            &root,
            &name,
            "run",
            who,
            None,
            text,
            BTreeMap::from([("result".into(), result)]),
            Some(e.tail.clone()),
          );
        }
        log_at = size;
      }

      let now_head = repo::head_commit(&repo).ok().flatten().map(|c| c.id());
      if now_head != head {
        if let Some(h) = now_head {
          let summary = repo.find_commit(h).map(|c| repo::summary(&c)).unwrap_or_default();
          hub.push(
            &root,
            &name,
            "commit",
            None,
            None,
            format!("HEAD is now {} {}", repo::short(h), short(&summary, 100)),
            BTreeMap::new(),
            None,
          );
        }
        head = now_head;
      }
    }
    let sub = hub.sub(&root);
    if let Some(every) = sub.every_secs
      && sub.kinds.contains("timer")
      && last_timer.elapsed() >= Duration::from_secs(every.max(10))
    {
      last_timer = Instant::now();
      hub.push(
        &root,
        &name,
        "timer",
        None,
        None,
        format!("timer: {every} seconds have passed"),
        BTreeMap::new(),
        None,
      );
    }
  }
}

fn file_loop(hub: Arc<Hub>, root: PathBuf, name: String) {
  let waker: livewatch::Waker = Arc::new(|| {});
  let handle = livewatch::spawn(root.clone(), waker);
  loop {
    if !hub.wants_files(&root) {
      return;
    }
    match handle.rx.recv_timeout(Duration::from_millis(500)) {
      Ok(Msg::Event(ev)) => {
        let kind = match ev.kind {
          FileKind::Created => "created",
          FileKind::Edited => "edited",
          FileKind::Deleted => "deleted",
        };
        let by = ev.agent.clone();
        let tool = ev.tool.clone();
        let stat = if ev.binary {
          "binary".to_string()
        } else {
          format!("+{} −{}", ev.added, ev.removed)
        };
        let text = format!(
          "{} {kind} ({stat}) by {}",
          ev.path,
          by.as_deref().map_or(tool.clone(), |a| format!("{tool} · {a}"))
        );
        hub.push(
          &root,
          &name,
          "file",
          by,
          Some(ev.path.clone()),
          text,
          BTreeMap::from([("change".into(), kind.into()), ("tool".into(), tool)]),
          None,
        );
      }
      Ok(Msg::Note(_)) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
    }
  }
}

fn start_webhook(hub: Arc<Hub>, root: PathBuf, name: String) -> Result<(u16, String)> {
  let listener = TcpListener::bind("127.0.0.1:0")?;
  let port = listener.local_addr()?.port();
  let token = random_token();
  let t = token.clone();
  std::thread::spawn(move || {
    for stream in listener.incoming().flatten() {
      let (hub, root, name, token) = (hub.clone(), root.clone(), name.clone(), t.clone());
      std::thread::spawn(move || {
        let _ = serve_hook(stream, &hub, &root, &name, &token);
      });
    }
  });
  Ok((port, token))
}

fn serve_hook(mut s: TcpStream, hub: &Hub, root: &Path, name: &str, token: &str) -> Result<()> {
  s.set_read_timeout(Some(Duration::from_secs(5)))?;
  let mut buf = Vec::new();
  let mut chunk = [0u8; 4096];
  let head_end = loop {
    let n = s.read(&mut chunk)?;
    if n == 0 {
      bail!("closed");
    }
    buf.extend_from_slice(&chunk[..n]);
    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
      break p + 4;
    }
    if buf.len() > 16 * 1024 {
      bail!("header too large");
    }
  };
  let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
  let mut lines = head.lines();
  let request = lines.next().unwrap_or("");
  let mut parts = request.split_whitespace();
  let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
  let header = |n: &str| {
    lines.clone().find_map(|l| {
      l.split_once(':')
        .filter(|(k, _)| k.eq_ignore_ascii_case(n))
        .map(|(_, v)| v.trim().to_string())
    })
  };
  let given = header("x-mig-token").or_else(|| {
    target
      .split_once("token=")
      .map(|(_, t)| t.split('&').next().unwrap_or("").to_string())
  });
  let reply = |s: &mut TcpStream, code: &str| {
    let _ = write!(s, "HTTP/1.1 {code}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
  };
  if given.as_deref() != Some(token) {
    reply(&mut s, "401 Unauthorized");
    return Ok(());
  }
  let want: usize = header("content-length")
    .and_then(|v| v.parse().ok())
    .unwrap_or(0)
    .min(BODY_MAX);
  while buf.len() < head_end + want {
    let n = s.read(&mut chunk)?;
    if n == 0 {
      break;
    }
    buf.extend_from_slice(&chunk[..n]);
  }
  let body = String::from_utf8_lossy(&buf[head_end.min(buf.len())..]).into_owned();
  let path = target.split('?').next().unwrap_or("/").to_string();
  // Only the size and kind of the request go in the event text; the body is kept for `events`.
  let text = format!("webhook {method} {path} ({} bytes)", body.len());
  let meta = BTreeMap::from([
    ("method".to_string(), method.to_string()),
    (
      "content_type".to_string(),
      header("content-type")
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect(),
    ),
  ]);
  hub.push(root, name, "webhook", None, Some(path), text, meta, Some(body));
  reply(&mut s, "200 OK");
  Ok(())
}

pub fn repo_name(root: &Path) -> String {
  crate::workspace::load()
    .into_iter()
    .find(|w| crate::workspace::same(&w.path, root))
    .map(|w| w.name)
    .unwrap_or_else(|| {
      root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
    })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn hub() -> (Arc<Hub>, Arc<Mutex<Vec<Value>>>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let s2 = sent.clone();
    (Hub::new(Arc::new(move |v| s2.lock().unwrap().push(v))), sent)
  }

  fn push(h: &Hub, root: &Path, kind: &str, actor: Option<&str>, path: Option<&str>, text: &str) -> bool {
    h.push(
      root,
      "r",
      kind,
      actor.map(str::to_string),
      path.map(str::to_string),
      text.into(),
      BTreeMap::new(),
      None,
    )
  }

  #[test]
  fn default_subscription_keeps_notes_agents_runs_and_commits_only() {
    let (h, sent) = hub();
    let r = PathBuf::from("/x");
    assert!(push(&h, &r, "message", None, None, "hello"));
    assert!(push(&h, &r, "agent", Some("bob"), None, "bob started"));
    assert!(!push(&h, &r, "file", Some("bob"), Some("a.rs"), "a.rs edited"));
    assert!(!push(&h, &r, "timer", None, None, "tick"));
    let v = sent.lock().unwrap().clone();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0]["method"], "notifications/claude/channel");
    assert_eq!(v[0]["params"]["content"], "hello");
    assert_eq!(v[0]["params"]["meta"]["kind"], "message");
    assert_eq!(v[1]["params"]["meta"]["agent"], "bob");
  }

  #[test]
  fn own_doings_are_not_echoed_unless_asked() {
    let (h, _) = hub();
    let r = PathBuf::from("/x");
    h.set_me(&r, Some("alice"));
    assert!(!push(&h, &r, "agent", Some("alice"), None, "alice started"));
    assert!(push(&h, &r, "agent", Some("bob"), None, "bob started"));
    assert!(push(&h, &r, "message", Some("alice"), None, "note for alice"));
    let mut sub = h.sub(&r);
    sub.include_own = true;
    h.lock().subs.insert(r.clone(), sub);
    assert!(push(&h, &r, "agent", Some("alice"), None, "alice again"));
  }

  #[test]
  fn file_events_need_the_kind_and_match_the_paths() {
    let (h, sent) = hub();
    let r = PathBuf::from("/x");
    let mut sub = h.sub(&r);
    sub.kinds.insert("file".into());
    sub.paths = vec!["src/**/*.rs".into()];
    h.lock().subs.insert(r.clone(), sub);
    assert!(push(&h, &r, "file", None, Some("src/a/b.rs"), "kept"));
    assert!(!push(&h, &r, "file", None, Some("docs/a.md"), "other path"));
    // The same file again within the quiet time is stored but not pushed.
    assert!(push(&h, &r, "file", None, Some("src/a/b.rs"), "again"));
    assert_eq!(sent.lock().unwrap().len(), 1);
    assert_eq!(h.recent(&r, 0, 10).len(), 2);
  }

  #[test]
  fn wait_returns_new_events_and_then_times_out() {
    let (h, _) = hub();
    let r = PathBuf::from("/x");
    let h2 = h.clone();
    let r2 = r.clone();
    let t = std::thread::spawn(move || h2.wait(&r2, Duration::from_secs(5), 10));
    std::thread::sleep(Duration::from_millis(200));
    push(&h, &r, "message", None, None, "later");
    let got = t.join().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].text, "later");
    assert!(h.wait(&r, Duration::from_millis(200), 10).is_empty());
    push(&h, &r, "commit", None, None, "c1");
    assert_eq!(h.wait(&r, Duration::from_millis(200), 10).len(), 1);
  }

  #[test]
  fn unseen_messages_are_taken_once() {
    let (h, _) = hub();
    let r = PathBuf::from("/x");
    push(&h, &r, "message", None, None, "m1");
    push(&h, &r, "agent", Some("b"), None, "b started");
    assert_eq!(h.take_unseen_messages(&r).len(), 1);
    assert!(h.take_unseen_messages(&r).is_empty());
  }

  #[test]
  fn pushes_are_rate_limited_but_notes_are_not() {
    let (h, sent) = hub();
    let r = PathBuf::from("/x");
    for i in 0..40 {
      push(&h, &r, "commit", None, None, &format!("c{i}"));
    }
    assert_eq!(sent.lock().unwrap().len(), PUSH_PER_MINUTE);
    push(&h, &r, "message", None, None, "urgent");
    assert_eq!(sent.lock().unwrap().len(), PUSH_PER_MINUTE + 2);
    let last = sent.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["params"]["content"], "urgent");
    assert_eq!(h.recent(&r, 0, 100).len(), 41);
  }

  #[test]
  fn webhook_needs_the_token() {
    let (h, sent) = hub();
    let r = PathBuf::from("/x");
    let url = h
      .watch(
        &r,
        "r",
        Sub {
          kinds: ["webhook".to_string()].into_iter().collect(),
          ..Sub::default()
        },
      )
      .unwrap();
    let u = url
      .lines()
      .last()
      .unwrap()
      .trim_start_matches("webhook: POST to http://127.0.0.1:")
      .to_string();
    let (port, token) = u.split_once("/?token=").unwrap();
    let post = |t: &str, body: &str| {
      let mut s = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
      write!(
        s,
        "POST /ci?token={t} HTTP/1.1\r\nHost: x\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
      )
      .unwrap();
      let mut out = String::new();
      let _ = s.read_to_string(&mut out);
      out
    };
    assert!(post("wrong", "x").starts_with("HTTP/1.1 401"));
    assert!(post(token, "build failed").starts_with("HTTP/1.1 200"));
    std::thread::sleep(Duration::from_millis(100));
    let ev = h.recent(&r, 0, 10);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].text, "webhook POST /ci (12 bytes)");
    assert_eq!(ev[0].body.as_deref(), Some("build failed"));
    assert!(
      !sent.lock().unwrap()[0]["params"]["content"]
        .as_str()
        .unwrap()
        .contains("build failed")
    );
  }
}
