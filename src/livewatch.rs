// Watches the working folder and turns every write into an event with a line diff against the
// previous version of that file. The first time a file is seen, "previous" is the version from the
// start of the agent session, or from HEAD when there is no session.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local};
use git2::{DiffOptions, Patch, Repository};
use ignore::WalkBuilder;
use notify::{EventKind, RecursiveMode, Watcher};

use crate::{agent, live, repo};

// Larger files are reported without a diff.
pub const MAX_TEXT: u64 = 1 << 20;
// Whole-file diffs are kept up to this size; larger ones keep only the changes and their context.
const WHOLE_MAX: usize = 100 * 1024;
pub const CONTEXT: usize = 3;
const DEBOUNCE: Duration = Duration::from_millis(100);
const TOOL_WINDOW_MS: i64 = 5000;
const CACHE_MAX: usize = 2000;
const WATCH_MAX: usize = 20_000;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
  Created,
  Edited,
  Deleted,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sign {
  Add,
  Del,
  Ctx,
}

#[derive(Clone, Debug)]
pub struct Line {
  pub sign: Sign,
  pub old: Option<u32>,
  pub new: Option<u32>,
  pub text: String,
}

pub struct Event {
  pub id: u64,
  pub at: DateTime<Local>,
  pub arrived: Instant,
  pub path: String,
  pub kind: Kind,
  pub tool: String,
  // The agent session that wrote it through mig, if any.
  pub agent: Option<String>,
  pub added: usize,
  pub removed: usize,
  pub binary: bool,
  pub lines: Vec<Line>,
  // `lines` holds the whole file, not only the changes with context.
  pub whole: bool,
  // Changed line ranges (first, last), numbered in the new file.
  pub ranges: Vec<(u32, u32)>,
}

pub enum Msg {
  Event(Box<Event>),
  Note(String),
}

pub struct Handle {
  pub rx: Receiver<Msg>,
  stop: Arc<AtomicBool>,
}

impl Drop for Handle {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
  }
}

pub struct Diff {
  pub lines: Vec<Line>,
  pub added: usize,
  pub removed: usize,
  pub ranges: Vec<(u32, u32)>,
}

pub fn diff(old: &str, new: &str, path: &str, whole: bool) -> Result<Diff> {
  let mut opts = DiffOptions::new();
  opts.context_lines(if whole { 1_000_000 } else { CONTEXT as u32 });
  let patch = Patch::from_buffers(
    old.as_bytes(),
    Some(Path::new(path)),
    new.as_bytes(),
    Some(Path::new(path)),
    Some(&mut opts),
  )?;
  let mut lines = Vec::new();
  for h in 0..patch.num_hunks() {
    let (_, n) = patch.hunk(h)?;
    for i in 0..n {
      let l = patch.line_in_hunk(h, i)?;
      let sign = match l.origin() {
        '+' => Sign::Add,
        '-' => Sign::Del,
        ' ' => Sign::Ctx,
        _ => continue,
      };
      lines.push(Line {
        sign,
        old: l.old_lineno(),
        new: l.new_lineno(),
        text: String::from_utf8_lossy(l.content())
          .trim_end_matches(['\n', '\r'])
          .to_string(),
      });
    }
  }
  let added = lines.iter().filter(|l| l.sign == Sign::Add).count();
  let removed = lines.iter().filter(|l| l.sign == Sign::Del).count();
  let mut ranges: Vec<(u32, u32)> = Vec::new();
  let (mut in_run, mut last_new) = (false, 0u32);
  for l in &lines {
    if l.sign == Sign::Ctx {
      in_run = false;
    } else if let Some(n) = l.new {
      match (in_run, ranges.last_mut()) {
        (true, Some(r)) => r.1 = n,
        _ => ranges.push((n, n)),
      }
      in_run = true;
    } else if !in_run {
      // A pure deletion sits between two lines of the new file.
      ranges.push((last_new + 1, last_new + 1));
      in_run = true;
    }
    if let Some(n) = l.new {
      last_new = n;
    }
  }
  Ok(Diff {
    lines,
    added,
    removed,
    ranges,
  })
}

fn is_binary(bytes: &[u8]) -> bool {
  bytes[..bytes.len().min(8000)].contains(&0)
}

// The text of `rel` before the watcher saw it change.
fn baseline(repo: &Repository, rel: &str) -> Option<String> {
  let tree = match agent::base_tree(repo) {
    Some(t) => t,
    None => repo::head_commit(repo).ok().flatten()?.tree().ok()?,
  };
  let blob = repo.find_blob(tree.get_path(Path::new(rel)).ok()?.id()).ok()?;
  (blob.size() as u64 <= MAX_TEXT && !blob.is_binary()).then(|| String::from_utf8_lossy(blob.content()).into_owned())
}

fn skip_name(name: &str) -> bool {
  name.ends_with(".mig-tmp") || name.ends_with(".swp") || name.ends_with('~')
}

struct State {
  repo: Repository,
  root: PathBuf,
  cache: HashMap<String, String>,
  next_id: u64,
}

impl State {
  fn rel(&self, p: &Path) -> Option<String> {
    let r = p.strip_prefix(&self.root).ok()?;
    let mut parts = Vec::new();
    for c in r.components() {
      parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() || parts[0] == ".git" || skip_name(parts.last()?) {
      return None;
    }
    Some(parts.join("/"))
  }

  fn event(&mut self, rel: &str, kind: Kind, old: &str, new: &str) -> Option<Event> {
    let whole = old.len().max(new.len()) <= WHOLE_MAX;
    let d = diff(old, new, rel, whole).ok()?;
    let by = live::recent(self.repo.path(), rel, TOOL_WINDOW_MS);
    let (tool, agent) = by.map_or(("other".to_string(), None), |e| (e.tool, e.agent));
    self.next_id += 1;
    Some(Event {
      id: self.next_id,
      at: Local::now(),
      arrived: Instant::now(),
      path: rel.to_string(),
      kind,
      tool,
      agent,
      added: d.added,
      removed: d.removed,
      binary: false,
      lines: d.lines,
      whole,
      ranges: d.ranges,
    })
  }

  fn changed(&mut self, p: &Path) -> Option<Event> {
    let rel = self.rel(p)?;
    if self.repo.is_path_ignored(&rel).unwrap_or(false) {
      return None;
    }
    if !p.is_file() {
      // Gone: report it only if we knew the file.
      let old = self.cache.remove(&rel).or_else(|| baseline(&self.repo, &rel))?;
      return self.event(&rel, Kind::Deleted, &old, "");
    }
    let meta = std::fs::metadata(p).ok()?;
    let bytes = if meta.len() > MAX_TEXT {
      Vec::new()
    } else {
      std::fs::read(p).ok()?
    };
    if meta.len() > MAX_TEXT || is_binary(&bytes) {
      let (tool, agent) =
        live::recent(self.repo.path(), &rel, TOOL_WINDOW_MS).map_or(("other".to_string(), None), |e| (e.tool, e.agent));
      self.next_id += 1;
      return Some(Event {
        id: self.next_id,
        at: Local::now(),
        arrived: Instant::now(),
        path: rel.clone(),
        kind: if baseline(&self.repo, &rel).is_some() {
          Kind::Edited
        } else {
          Kind::Created
        },
        tool,
        agent,
        added: 0,
        removed: 0,
        binary: true,
        lines: Vec::new(),
        whole: false,
        ranges: Vec::new(),
      });
    }
    let new = String::from_utf8_lossy(&bytes).into_owned();
    let old = self.cache.get(&rel).cloned().or_else(|| baseline(&self.repo, &rel));
    if self.cache.len() >= CACHE_MAX {
      self.cache.clear();
    }
    self.cache.insert(rel.clone(), new.clone());
    if old.as_deref() == Some(new.as_str()) {
      return None;
    }
    let kind = if old.is_some() { Kind::Edited } else { Kind::Created };
    self.event(&rel, kind, old.as_deref().unwrap_or(""), &new)
  }
}

// Watches every folder that is not ignored; ignored trees such as `target` would exhaust the
// operating system's limit on watches. Returns how many folders could not be watched.
fn watch_dirs(w: &mut impl Watcher, dir: &Path, watched: &mut HashSet<PathBuf>) -> usize {
  let mut failed = 0;
  let walk = WalkBuilder::new(dir)
    .hidden(false)
    .filter_entry(|e| e.file_name() != ".git")
    .build();
  for e in walk.flatten() {
    if !e.file_type().is_some_and(|t| t.is_dir()) || watched.contains(e.path()) {
      continue;
    }
    if watched.len() >= WATCH_MAX || w.watch(e.path(), RecursiveMode::NonRecursive).is_err() {
      failed += 1;
    } else {
      watched.insert(e.path().to_path_buf());
    }
  }
  failed
}

fn run(root: PathBuf, tx: Sender<Msg>, stop: Arc<AtomicBool>, ctx: eframe::egui::Context) {
  let Ok(repo) = repo::open(&root) else { return };
  let (ntx, nrx) = mpsc::channel();
  let mut watcher = match notify::recommended_watcher(move |r| {
    let _ = ntx.send(r);
  }) {
    Ok(w) => w,
    Err(e) => {
      let _ = tx.send(Msg::Note(format!("file watching is unavailable: {e}")));
      return;
    }
  };
  let mut watched = HashSet::new();
  let failed = watch_dirs(&mut watcher, &root, &mut watched);
  let _ = tx.send(Msg::Note(if failed == 0 {
    format!("watching {} folders", watched.len())
  } else {
    format!("watching {} folders, {failed} could not be watched", watched.len())
  }));

  let mut state = State {
    repo,
    root: root.clone(),
    cache: HashMap::new(),
    next_id: 0,
  };
  let mut pending: HashSet<PathBuf> = HashSet::new();
  let mut last = Instant::now();
  while !stop.load(Ordering::Relaxed) {
    match nrx.recv_timeout(Duration::from_millis(50)) {
      Ok(Ok(ev)) => {
        if !matches!(ev.kind, EventKind::Access(_)) {
          pending.extend(ev.paths);
          last = Instant::now();
        }
      }
      Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
      Err(mpsc::RecvTimeoutError::Disconnected) => break,
    }
    if pending.is_empty() || last.elapsed() < DEBOUNCE {
      continue;
    }
    let mut batch: Vec<PathBuf> = pending.drain().collect();
    batch.sort();
    let mut sent = false;
    for p in batch {
      if p.is_dir() {
        watch_dirs(&mut watcher, &p, &mut watched);
        // Files written before the new folder was watched would be missed otherwise.
        let inside = WalkBuilder::new(&p)
          .hidden(false)
          .filter_entry(|e| e.file_name() != ".git")
          .build();
        for f in inside.flatten().filter(|e| e.file_type().is_some_and(|t| t.is_file())) {
          if let Some(ev) = state.changed(f.path()) {
            sent |= tx.send(Msg::Event(Box::new(ev))).is_ok();
          }
        }
        continue;
      }
      if let Some(ev) = state.changed(&p) {
        sent |= tx.send(Msg::Event(Box::new(ev))).is_ok();
      }
    }
    if sent {
      ctx.request_repaint();
    }
  }
}

pub fn spawn(root: PathBuf, ctx: eframe::egui::Context) -> Handle {
  let (tx, rx) = mpsc::channel();
  let stop = Arc::new(AtomicBool::new(false));
  let flag = stop.clone();
  std::thread::spawn(move || run(root, tx, flag, ctx));
  Handle { rx, stop }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn whole_file_diff_marks_changes() {
    let d = diff("a\nb\nc\nd\n", "a\nB\nc\nd\ne\n", "f.txt", true).unwrap();
    assert_eq!((d.added, d.removed), (2, 1));
    assert_eq!(d.lines.len(), 6);
    assert_eq!(d.lines[1].sign, Sign::Del);
    assert_eq!(d.lines[2].new, Some(2));
    assert_eq!(d.ranges, vec![(2, 2), (5, 5)]);
  }

  #[test]
  fn pure_deletion_has_a_position() {
    let d = diff("a\nb\nc\n", "a\nc\n", "f.txt", true).unwrap();
    assert_eq!((d.added, d.removed), (0, 1));
    assert_eq!(d.ranges, vec![(2, 2)]);
  }

  #[test]
  fn created_file_is_all_added() {
    let d = diff("", "x\ny\n", "f.txt", true).unwrap();
    assert_eq!((d.added, d.removed), (2, 0));
    assert_eq!(d.ranges, vec![(1, 2)]);
  }

  #[test]
  fn context_is_limited_when_not_whole() {
    let old: String = (1..=100).map(|i| format!("{i}\n")).collect();
    let new = old.replace("50\n", "fifty\n");
    let d = diff(&old, &new, "f.txt", false).unwrap();
    assert_eq!(d.lines.len(), 2 + 2 * CONTEXT);
  }
}
