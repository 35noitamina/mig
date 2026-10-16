// Agent sessions. Several agents can work in one repository at the same time, sharing its working
// tree. Each session remembers the files its agent wrote through mig (with their content before
// the agent's first write), so finishing or aborting one agent leaves the others' work alone.
// A session that never overlapped with another works on the whole tree instead, which also covers
// edits made with tools other than mig.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use git2::build::CheckoutBuilder;
use git2::{Index, IndexAddOption, IndexEntry, IndexTime, ObjectType, Oid, Repository};
use serde::{Deserialize, Serialize};

use crate::{ai, assist, oplog, ops, repo, rev};

const SAVED: &str = "refs/mig/saved";
const LEGACY_REFS: [&str; 3] = ["refs/mig/agent/start", "refs/mig/agent/base", "refs/mig/agent/cp"];

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct Touch {
  // Blob ids; None means the file did not exist.
  pub base: Option<String>,
  pub now: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Session {
  pub name: String,
  #[serde(default)]
  pub id: String,
  pub branch: String,
  pub started: i64,
  // Another session was running at some point during this one.
  #[serde(default)]
  pub shared: bool,
  #[serde(default)]
  pub files: BTreeMap<String, Touch>,
  #[serde(default)]
  pub writes: usize,
  #[serde(default)]
  pub last_active: i64,
  // Process id of the MCP server that started it; None when started from the CLI or the GUI.
  #[serde(default)]
  pub owner: Option<u32>,
}

fn dir(repo: &Repository) -> PathBuf {
  repo.path().join("mig").join("agents")
}

fn meta_path(repo: &Repository, id: &str) -> PathBuf {
  dir(repo).join(format!("{id}.json"))
}

fn refname(id: &str, what: &str) -> String {
  format!("refs/mig/agents/{id}/{what}")
}

fn save_meta(repo: &Repository, s: &Session) -> Result<()> {
  std::fs::create_dir_all(dir(repo))?;
  let path = meta_path(repo, &s.id);
  let tmp = path.with_extension("json.tmp");
  std::fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
  std::fs::rename(tmp, path)?;
  Ok(())
}

fn slug(name: &str) -> String {
  let s: String = name
    .chars()
    .map(|c| {
      if c.is_ascii_alphanumeric() {
        c.to_ascii_lowercase()
      } else {
        '-'
      }
    })
    .collect();
  let s = s.trim_matches('-').to_string();
  if s.is_empty() { "agent".into() } else { s }
}

// Sessions from before several agents were supported lived in refs/mig/agent/* and agent.json.
fn migrate(repo: &Repository) {
  let old = repo.path().join("mig").join("agent.json");
  let Ok(text) = std::fs::read_to_string(&old) else {
    return;
  };
  let Ok(mut s) = serde_json::from_str::<Session>(&text) else {
    return;
  };
  s.id = slug(&s.name);
  for (r, what) in LEGACY_REFS.iter().zip(["start", "base", "cp"]) {
    if let Ok(oid) = repo.refname_to_id(r) {
      let _ = repo.reference(&refname(&s.id, what), oid, true, "mig agent");
    }
    if let Ok(mut r) = repo.find_reference(r) {
      let _ = r.delete();
    }
  }
  if save_meta(repo, &s).is_ok() {
    let _ = std::fs::remove_file(old);
  }
}

// Oldest first.
pub fn list(repo: &Repository) -> Vec<Session> {
  migrate(repo);
  let mut out: Vec<Session> = std::fs::read_dir(dir(repo))
    .map(|rd| {
      rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_json::from_str(&t).ok())
        .collect()
    })
    .unwrap_or_default();
  out.sort_by_key(|s: &Session| s.started);
  out
}

pub fn get(repo: &Repository, key: &str) -> Option<Session> {
  list(repo).into_iter().find(|s| s.id == key || s.name == key)
}

// `key` when given; otherwise the only session.
pub fn resolve(repo: &Repository, key: Option<&str>) -> Result<Session> {
  if let Some(k) = key {
    return get(repo, k).ok_or_else(|| anyhow!("no agent session named {k}"));
  }
  let all = list(repo);
  match all.as_slice() {
    [] => bail!("no agent session; start one with `mig agent start`"),
    [only] => Ok(only.clone()),
    _ => bail!(
      "several agent sessions are running ({}); name one",
      all.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
    ),
  }
}

// Commit the whole working tree (tracked and untracked, minus ignored) without moving any ref
// or changing the index on disk.
fn snapshot(repo: &Repository, message: &str, parent: Option<Oid>) -> Result<Oid> {
  let mut index = repo.index()?;
  let tree = (|| -> Result<Oid> {
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)?;
    index.update_all(["*"].iter(), None)?;
    Ok(index.write_tree()?)
  })();
  index.read(true)?;
  let tree = repo.find_tree(tree?)?;
  let sig = repo::signature(repo)?;
  let parent = parent.map(|p| repo.find_commit(p)).transpose()?;
  let parents: Vec<&git2::Commit> = parent.iter().collect();
  Ok(repo.commit(None, &sig, &sig, message, &tree, &parents)?)
}

fn set_ref(repo: &Repository, name: &str, oid: Oid) -> Result<()> {
  repo.reference(name, oid, true, "mig agent")?;
  Ok(())
}

fn clear(repo: &Repository, id: &str) -> Result<()> {
  for what in ["start", "base", "cp", "files"] {
    if let Ok(mut r) = repo.find_reference(&refname(id, what)) {
      r.delete()?;
    }
  }
  let _ = std::fs::remove_file(meta_path(repo, id));
  Ok(())
}

// Replace the working tree with `tree_of` while keeping HEAD and making the index match HEAD,
// so the restored state shows up as uncommitted changes.
fn restore_tree(repo: &Repository, tree_of: Oid) -> Result<()> {
  let commit = repo.find_commit(tree_of)?;
  let mut cb = CheckoutBuilder::new();
  cb.force().remove_untracked(true);
  repo.checkout_tree(commit.as_object(), Some(&mut cb))?;
  let mut index = repo.index()?;
  match repo::head_commit(repo)? {
    Some(h) => index.read_tree(&h.tree()?)?,
    None => index.clear()?,
  }
  index.write()?;
  Ok(())
}

pub fn start(repo: &Repository, name: &str) -> Result<Session> {
  let head = repo::head_commit(repo)?.ok_or_else(|| anyhow!("make a first commit before starting an agent session"))?;
  let branch = repo::branch_name(repo).ok_or_else(|| anyhow!("switch to a branch first"))?;
  let others = list(repo);
  if others.iter().any(|s| s.name == name) {
    bail!("an agent session named {name} is already running");
  }
  let mut id = slug(name);
  let mut n = 2;
  while others.iter().any(|s| s.id == id) {
    id = format!("{}-{n}", slug(name));
    n += 1;
  }
  let base = snapshot(repo, &format!("agent {name}: start"), Some(head.id()))?;
  set_ref(repo, &refname(&id, "start"), head.id())?;
  set_ref(repo, &refname(&id, "base"), base)?;
  set_ref(repo, &refname(&id, "cp"), base)?;
  for mut o in others.iter().cloned().filter(|o| !o.shared) {
    o.shared = true;
    save_meta(repo, &o)?;
  }
  let now = chrono::Utc::now().timestamp();
  let s = Session {
    name: name.to_string(),
    id,
    branch,
    started: now,
    shared: !others.is_empty(),
    files: BTreeMap::new(),
    writes: 0,
    last_active: now,
    owner: None,
  };
  save_meta(repo, &s)?;
  Ok(s)
}

fn blob(repo: &Repository, content: Option<&[u8]>) -> Result<Option<String>> {
  content.map(|c| Ok(repo.blob(c)?.to_string())).transpose()
}

pub fn hash(content: &[u8]) -> String {
  Oid::hash_object(ObjectType::Blob, content).map_or_else(|_| String::new(), |o| o.to_string())
}

// Keeps the blobs a session refers to reachable, so garbage collection cannot drop them.
fn pin_files(repo: &Repository, s: &Session) -> Result<()> {
  let mut index = Index::new()?;
  for (path, t) in &s.files {
    for (side, id) in [("base", &t.base), ("now", &t.now)] {
      let Some(id) = id else { continue };
      let p = format!("{side}/{path}");
      index.add(&IndexEntry {
        ctime: IndexTime::new(0, 0),
        mtime: IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        file_size: 0,
        id: Oid::from_str(id)?,
        flags: p.len().min(0xfff) as u16,
        flags_extended: 0,
        path: p.into_bytes(),
      })?;
    }
  }
  let tree = repo.find_tree(index.write_tree_to(repo)?)?;
  let sig = repo::signature(repo)?;
  let oid = repo.commit(None, &sig, &sig, &format!("agent {}: files", s.name), &tree, &[])?;
  set_ref(repo, &refname(&s.id, "files"), oid)
}

pub fn set_owner(repo: &Repository, id: &str, pid: u32) -> Result<()> {
  let mut s = get(repo, id).ok_or_else(|| anyhow!("no agent session named {id}"))?;
  s.owner = Some(pid);
  save_meta(repo, &s)
}

// Notes that the agent changed `path` from `before` to `after` (None: absent).
pub fn record_write(
  repo: &Repository,
  id: &str,
  path: &str,
  before: Option<&[u8]>,
  after: Option<&[u8]>,
) -> Result<()> {
  let mut s = get(repo, id).ok_or_else(|| anyhow!("the agent session {id} has ended"))?;
  let now = blob(repo, after)?;
  match s.files.get_mut(path) {
    Some(t) => t.now = now,
    None => {
      let base = blob(repo, before)?;
      s.files.insert(path.to_string(), Touch { base, now });
    }
  }
  s.writes += 1;
  s.last_active = chrono::Utc::now().timestamp();
  save_meta(repo, &s)?;
  pin_files(repo, &s)
}

// Ok(None) when nothing changed since the last checkpoint.
pub fn checkpoint(repo: &Repository, id: &str, message: &str) -> Result<Option<Oid>> {
  let s = get(repo, id).ok_or_else(|| anyhow!("no agent session named {id}"))?;
  let cp = refname(&s.id, "cp");
  let last = repo.refname_to_id(&cp)?;
  let oid = snapshot(repo, message, Some(last))?;
  if repo.find_commit(oid)?.tree_id() == repo.find_commit(last)?.tree_id() {
    return Ok(None);
  }
  set_ref(repo, &cp, oid)?;
  Ok(Some(oid))
}

pub struct Checkpoint {
  pub oid: Oid,
  pub time: i64,
  pub message: String,
}

// Newest first, ending with the snapshot taken at start.
pub fn checkpoints(repo: &Repository, id: &str) -> Result<Vec<Checkpoint>> {
  let base = repo.refname_to_id(&refname(id, "base"))?;
  let mut out = Vec::new();
  let mut cur = Some(repo.find_commit(repo.refname_to_id(&refname(id, "cp"))?)?);
  while let Some(c) = cur {
    out.push(Checkpoint {
      oid: c.id(),
      time: c.time().seconds(),
      message: repo::summary(&c),
    });
    if c.id() == base {
      break;
    }
    cur = c.parents().next();
  }
  Ok(out)
}

// `spec` is a number from `mig agent log` (1 = latest) or a commit id. The current state is
// checkpointed (or kept in refs/mig/saved outside a session) first, so restoring can be undone.
// The whole tree is replaced, so this is refused while other agents are working.
pub fn restore(repo: &Repository, id: Option<&str>, spec: &str) -> Result<Oid> {
  let session = match id {
    Some(k) => Some(resolve(repo, Some(k))?),
    None => list(repo).into_iter().next(),
  };
  if list(repo).len() > 1 {
    bail!("other agents are working in this repository; restoring would overwrite their files");
  }
  let target = match (spec.parse::<usize>(), &session) {
    (Ok(n), Some(s)) if spec.len() <= 4 => checkpoints(repo, &s.id)?
      .get(n.wrapping_sub(1))
      .map(|c| c.oid)
      .ok_or_else(|| anyhow!("there is no checkpoint #{n}"))?,
    _ => repo.revparse_single(spec)?.peel_to_commit()?.id(),
  };
  match &session {
    Some(s) => {
      checkpoint(repo, &s.id, "agent: before restore")?;
    }
    None => {
      let head = repo::head_commit(repo)?.map(|c| c.id());
      set_ref(repo, SAVED, snapshot(repo, "mig: before restore", head)?)?;
    }
  }
  restore_tree(repo, target)?;
  Ok(target)
}

fn read_worktree(repo: &Repository, path: &str) -> Option<Vec<u8>> {
  let root = repo.workdir()?;
  std::fs::read(root.join(Path::new(path))).ok()
}

fn changed_paths(repo: &Repository) -> Result<Vec<String>> {
  Ok(repo::changes(repo)?.into_iter().map(|c| c.path).collect())
}

pub struct Finished {
  pub commits: usize,
  pub head: Option<Oid>,
  // Changed files left uncommitted because they are not this agent's.
  pub left: usize,
}

// Turn the agent's work into one commit and end the session.
pub fn finish(repo: &Repository, id: &str, message: Option<&str>, use_ai: bool) -> Result<Finished> {
  let s = resolve(repo, Some(id))?;
  let start = repo.refname_to_id(&refname(&s.id, "start"))?;
  let msg = match (message, use_ai) {
    (Some(m), _) => m.to_string(),
    (None, true) => assist::commit_message_since(repo, &ai::AiConfig::load(), start)?.text,
    (None, false) => format!("Agent: {}", s.name),
  };
  if s.shared {
    let changed = changed_paths(repo)?;
    let mine: Vec<String> = changed.iter().filter(|p| s.files.contains_key(*p)).cloned().collect();
    if !mine.is_empty() {
      oplog::record(repo, "agent finish", || ops::save(repo, Some(&msg), &mine, false))?;
    }
    clear(repo, &s.id)?;
    return Ok(Finished {
      commits: usize::from(!mine.is_empty()),
      head: repo::head_commit(repo)?.map(|c| c.id()),
      left: changed.len() - mine.len(),
    });
  }
  let pending = !repo::changes(repo)?.is_empty();
  oplog::record(repo, "agent finish", || {
    if pending {
      ops::save(repo, Some(&msg), &[], false)?;
    }
    let k = rev::position(repo, &start.to_string())?;
    if k >= 2 {
      ops::fold(repo, 0, k - 1, Some(&msg))?;
    } else if k == 1 && !pending && message.is_some() {
      ops::reword(repo, 0, &msg)?;
    }
    Ok(())
  })?;
  let head = repo::head_commit(repo)?.map(|c| c.id());
  let commits = rev::position(repo, &start.to_string()).unwrap_or(0);
  clear(repo, &s.id)?;
  Ok(Finished { commits, head, left: 0 })
}

pub struct Aborted {
  // Snapshot of the tree before aborting; `mig agent restore <id>` brings it back.
  pub saved: Oid,
  pub reverted: usize,
  // Files another writer changed after this agent, left as they are.
  pub kept: Vec<String>,
}

// Throw away the agent's work. Alone, that is everything since start (files and commits on the
// branch); alongside other agents, only the files this agent wrote that nobody changed since.
pub fn abort(repo: &Repository, id: &str) -> Result<Aborted> {
  let s = resolve(repo, Some(id))?;
  let start = repo.refname_to_id(&refname(&s.id, "start"))?;
  let base = repo.refname_to_id(&refname(&s.id, "base"))?;
  let saved = snapshot(
    repo,
    &format!("mig: discarded work of agent {}", s.name),
    Some(repo.refname_to_id(&refname(&s.id, "cp"))?),
  )?;
  set_ref(repo, SAVED, saved)?;
  if !s.shared {
    oplog::record(repo, "agent abort", || {
      if repo::head_commit(repo)?.map(|c| c.id()) != Some(start) {
        repo::move_head(repo, start, "mig: agent abort")?;
      }
      Ok(())
    })?;
    restore_tree(repo, base)?;
    clear(repo, &s.id)?;
    return Ok(Aborted {
      saved,
      reverted: s.files.len(),
      kept: Vec::new(),
    });
  }
  let root = repo
    .workdir()
    .ok_or_else(|| anyhow!("no working folder"))?
    .to_path_buf();
  let (mut reverted, mut kept) = (0, Vec::new());
  for (path, t) in &s.files {
    let current = read_worktree(repo, path);
    if current.as_deref().map(hash) != t.now {
      kept.push(path.clone());
      continue;
    }
    let file = root.join(Path::new(path));
    match &t.base {
      Some(id) => {
        let content = repo.find_blob(Oid::from_str(id)?)?;
        if let Some(d) = file.parent() {
          std::fs::create_dir_all(d)?;
        }
        std::fs::write(&file, content.content())?;
      }
      None => {
        let _ = std::fs::remove_file(&file);
      }
    }
    reverted += 1;
  }
  clear(repo, &s.id)?;
  Ok(Aborted { saved, reverted, kept })
}

pub struct Status {
  pub session: Session,
  pub checkpoints: usize,
  pub commits: usize,
  pub changed_files: usize,
}

pub fn status(repo: &Repository) -> Result<Vec<Status>> {
  let changed = repo::changes(repo)?.len();
  list(repo)
    .into_iter()
    .map(|s| {
      let start = repo.refname_to_id(&refname(&s.id, "start"))?;
      Ok(Status {
        checkpoints: checkpoints(repo, &s.id)?.len().saturating_sub(1),
        commits: rev::position(repo, &start.to_string()).unwrap_or(0),
        changed_files: if s.shared { s.files.len() } else { changed },
        session: s,
      })
    })
    .collect()
}

// The working tree as it was when the earliest running session started.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn base_tree(repo: &Repository) -> Option<git2::Tree<'_>> {
  let s = list(repo).into_iter().next()?;
  repo
    .find_reference(&refname(&s.id, "base"))
    .ok()?
    .peel_to_commit()
    .ok()?
    .tree()
    .ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn repo_with(files: &[(&str, &str)]) -> (PathBuf, Repository) {
    let dir = std::env::temp_dir().join(format!("mig-agent-test-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let r = Repository::init(&dir).unwrap();
    let mut cfg = r.config().unwrap();
    cfg.set_str("user.name", "t").unwrap();
    cfg.set_str("user.email", "t@e.x").unwrap();
    for (p, c) in files {
      std::fs::write(dir.join(p), c).unwrap();
    }
    ops::save(&r, Some("init"), &[], true).unwrap();
    (dir, r)
  }

  fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  }

  fn write(dir: &Path, r: &Repository, id: &str, path: &str, content: &str) {
    let before = std::fs::read(dir.join(path)).ok();
    std::fs::write(dir.join(path), content).unwrap();
    record_write(r, id, path, before.as_deref(), Some(content.as_bytes())).unwrap();
  }

  #[test]
  fn shared_abort_reverts_only_own_untouched_files() {
    let (dir, r) = repo_with(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
    let one = start(&r, "one").unwrap();
    let two = start(&r, "two").unwrap();
    assert!(get(&r, &one.id).unwrap().shared && two.shared);
    write(&dir, &r, &one.id, "a.txt", "a by one\n");
    write(&dir, &r, &one.id, "new.txt", "new by one\n");
    write(&dir, &r, &one.id, "c.txt", "c by one\n");
    write(&dir, &r, &two.id, "b.txt", "b by two\n");
    // two overwrites one's change to c.txt
    write(&dir, &r, &two.id, "c.txt", "c by two\n");
    let a = abort(&r, &one.id).unwrap();
    assert_eq!(a.reverted, 2);
    assert_eq!(a.kept, vec!["c.txt".to_string()]);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
    assert!(!dir.join("new.txt").exists());
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b by two\n");
    assert_eq!(std::fs::read_to_string(dir.join("c.txt")).unwrap(), "c by two\n");
    assert_eq!(list(&r).len(), 1);
    let _ = std::fs::remove_dir_all(dir);
  }

  #[test]
  fn shared_finish_commits_only_own_files() {
    let (dir, r) = repo_with(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let one = start(&r, "one").unwrap();
    let two = start(&r, "two").unwrap();
    write(&dir, &r, &one.id, "a.txt", "a by one\n");
    write(&dir, &r, &two.id, "b.txt", "b by two\n");
    let f = finish(&r, &one.id, Some("one's work"), false).unwrap();
    assert_eq!((f.commits, f.left), (1, 1));
    let head = repo::head_commit(&r).unwrap().unwrap();
    assert_eq!(repo::summary(&head), "one's work");
    let changes: Vec<String> = repo::changes(&r).unwrap().into_iter().map(|c| c.path).collect();
    assert_eq!(changes, vec!["b.txt".to_string()]);
    let _ = std::fs::remove_dir_all(dir);
  }

  #[test]
  fn alone_abort_restores_everything() {
    let (dir, r) = repo_with(&[("a.txt", "a\n")]);
    let s = start(&r, "solo").unwrap();
    std::fs::write(dir.join("a.txt"), "edited elsewhere\n").unwrap();
    std::fs::write(dir.join("x.txt"), "x\n").unwrap();
    abort(&r, &s.id).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
    assert!(!dir.join("x.txt").exists());
    let _ = std::fs::remove_dir_all(dir);
  }
}
