use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};

use crate::repo;

const MAX_OPS: usize = 500;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Snapshot {
  pub head: String,
  pub refs: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Op {
  pub time: i64,
  pub cmd: String,
  pub before: Snapshot,
  pub after: Snapshot,
}

#[derive(Serialize, Deserialize, Default)]
pub struct OpLog {
  pub ops: Vec<Op>,
  // Number of ops currently applied; ops[pos..] are undone and can be redone.
  pub pos: usize,
}

fn path(repo: &Repository) -> PathBuf {
  repo.path().join("mig").join("oplog.json")
}

pub fn load(repo: &Repository) -> Result<OpLog> {
  let p = path(repo);
  if !p.exists() {
    return Ok(OpLog::default());
  }
  Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?)
}

fn store(repo: &Repository, log: &OpLog) -> Result<()> {
  let p = path(repo);
  std::fs::create_dir_all(p.parent().unwrap())?;
  let tmp = p.with_extension("tmp");
  std::fs::write(&tmp, serde_json::to_string(log)?)?;
  std::fs::rename(tmp, p)?;
  Ok(())
}

pub fn snapshot(repo: &Repository) -> Result<Snapshot> {
  let head_ref = repo.find_reference("HEAD")?;
  let head = match head_ref.symbolic_target()? {
    Some(t) => format!("ref: {t}"),
    None => head_ref.target().map(|o| o.to_string()).unwrap_or_default(),
  };
  let mut refs = BTreeMap::new();
  for r in repo.references()? {
    let r = r?;
    let (Ok(name), Some(target)) = (r.name(), r.target()) else {
      continue;
    };
    if name.starts_with("refs/heads/") || name.starts_with("refs/tags/") {
      refs.insert(name.to_string(), target.to_string());
    }
  }
  Ok(Snapshot { head, refs })
}

// Run `f` and record the ref changes it made so they can be undone.
pub fn record<T>(repo: &Repository, cmd: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
  let before = snapshot(repo)?;
  let out = f();
  let after = snapshot(repo)?;
  if before != after {
    let mut log = load(repo)?;
    log.ops.truncate(log.pos);
    log.ops.push(Op {
      time: chrono::Utc::now().timestamp(),
      cmd: cmd.to_string(),
      before,
      after,
    });
    if log.ops.len() > MAX_OPS {
      log.ops.drain(..log.ops.len() - MAX_OPS);
    }
    log.pos = log.ops.len();
    store(repo, &log)?;
  }
  out
}

fn restore(repo: &Repository, snap: &Snapshot, force: bool) -> Result<()> {
  if !force {
    repo::ensure_clean(repo)?;
  }
  let current = snapshot(repo)?;
  for name in current.refs.keys() {
    if !snap.refs.contains_key(name) {
      repo.find_reference(name)?.delete()?;
    }
  }
  for (name, oid) in &snap.refs {
    if current.refs.get(name) != Some(oid) {
      repo.reference(name, Oid::from_str(oid)?, true, "mig: restore")?;
    }
  }
  match snap.head.strip_prefix("ref: ") {
    Some(name) => repo.set_head(name)?,
    None => repo.set_head_detached(Oid::from_str(&snap.head)?)?,
  }
  repo::checkout_head_force(repo)
}

pub fn undo(repo: &Repository, force: bool) -> Result<Op> {
  let mut log = load(repo)?;
  if log.pos == 0 {
    bail!("nothing to undo");
  }
  let op = log.ops[log.pos - 1].clone();
  restore(repo, &op.before, force)?;
  log.pos -= 1;
  store(repo, &log)?;
  Ok(op)
}

pub fn redo(repo: &Repository, force: bool) -> Result<Op> {
  let mut log = load(repo)?;
  if log.pos >= log.ops.len() {
    bail!("nothing to redo");
  }
  let op = log.ops[log.pos].clone();
  restore(repo, &op.after, force)?;
  log.pos += 1;
  store(repo, &log)?;
  Ok(op)
}
