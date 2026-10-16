// Agent sessions: snapshot the working tree before an AI agent starts, take cheap checkpoints while
// it works (without touching the branch or the index), then squash the result into one commit or
// throw it all away.
use anyhow::{Result, anyhow, bail};
use git2::build::CheckoutBuilder;
use git2::{IndexAddOption, Oid, Repository};
use serde::{Deserialize, Serialize};

use crate::{ai, assist, oplog, ops, repo, rev};

const START: &str = "refs/mig/agent/start";
const BASE: &str = "refs/mig/agent/base";
const CP: &str = "refs/mig/agent/cp";
const SAVED: &str = "refs/mig/saved";

#[derive(Serialize, Deserialize, Clone)]
pub struct Session {
  pub name: String,
  pub branch: String,
  pub started: i64,
}

fn meta_path(repo: &Repository) -> std::path::PathBuf {
  repo.path().join("mig").join("agent.json")
}

pub fn session(repo: &Repository) -> Option<Session> {
  let text = std::fs::read_to_string(meta_path(repo)).ok()?;
  serde_json::from_str(&text).ok()
}

// The working tree as it was when the session started.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn base_tree(repo: &Repository) -> Option<git2::Tree<'_>> {
  repo.find_reference(BASE).ok()?.peel_to_commit().ok()?.tree().ok()
}

fn require(repo: &Repository) -> Result<Session> {
  session(repo).ok_or_else(|| anyhow!("no agent session; start one with `mig agent start`"))
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

fn clear(repo: &Repository) -> Result<()> {
  for r in [START, BASE, CP] {
    if let Ok(mut r) = repo.find_reference(r) {
      r.delete()?;
    }
  }
  let _ = std::fs::remove_file(meta_path(repo));
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
  if session(repo).is_some() {
    bail!("an agent session is already running; finish or abort it first");
  }
  let head = repo::head_commit(repo)?.ok_or_else(|| anyhow!("make a first commit before starting an agent session"))?;
  let branch = repo::branch_name(repo).ok_or_else(|| anyhow!("switch to a branch first"))?;
  let base = snapshot(repo, "agent: start", Some(head.id()))?;
  set_ref(repo, START, head.id())?;
  set_ref(repo, BASE, base)?;
  set_ref(repo, CP, base)?;
  let s = Session {
    name: name.to_string(),
    branch,
    started: chrono::Utc::now().timestamp(),
  };
  std::fs::create_dir_all(meta_path(repo).parent().unwrap())?;
  std::fs::write(meta_path(repo), serde_json::to_string_pretty(&s)?)?;
  Ok(s)
}

// Ok(None) when nothing changed since the last checkpoint.
pub fn checkpoint(repo: &Repository, message: &str) -> Result<Option<Oid>> {
  require(repo)?;
  let last = repo.refname_to_id(CP)?;
  let oid = snapshot(repo, message, Some(last))?;
  if repo.find_commit(oid)?.tree_id() == repo.find_commit(last)?.tree_id() {
    return Ok(None);
  }
  set_ref(repo, CP, oid)?;
  Ok(Some(oid))
}

pub struct Checkpoint {
  pub oid: Oid,
  pub time: i64,
  pub message: String,
}

// Newest first, ending with the snapshot taken at start.
pub fn checkpoints(repo: &Repository) -> Result<Vec<Checkpoint>> {
  require(repo)?;
  let base = repo.refname_to_id(BASE)?;
  let mut out = Vec::new();
  let mut cur = Some(repo.find_commit(repo.refname_to_id(CP)?)?);
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
pub fn restore(repo: &Repository, spec: &str) -> Result<Oid> {
  let target = match spec.parse::<usize>() {
    Ok(n) if spec.len() <= 4 => {
      let list = checkpoints(repo)?;
      list
        .get(n.wrapping_sub(1))
        .map(|c| c.oid)
        .ok_or_else(|| anyhow!("there is no checkpoint #{n}"))?
    }
    _ => repo.revparse_single(spec)?.peel_to_commit()?.id(),
  };
  if session(repo).is_some() {
    checkpoint(repo, "agent: before restore")?;
  } else {
    let head = repo::head_commit(repo)?.map(|c| c.id());
    set_ref(repo, SAVED, snapshot(repo, "mig: before restore", head)?)?;
  }
  restore_tree(repo, target)?;
  Ok(target)
}

pub struct Finished {
  pub commits: usize,
  pub head: Option<Oid>,
}

// Turn everything the agent did into one commit on the branch and end the session.
pub fn finish(repo: &Repository, message: Option<&str>, use_ai: bool) -> Result<Finished> {
  let s = require(repo)?;
  let start = repo.refname_to_id(START)?;
  let msg = match (message, use_ai) {
    (Some(m), _) => m.to_string(),
    (None, true) => assist::commit_message_since(repo, &ai::AiConfig::load(), start)?.text,
    (None, false) => format!("Agent: {}", s.name),
  };
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
  clear(repo)?;
  Ok(Finished { commits, head })
}

// Throw away everything since `mig agent start`: files, and commits the agent made on the branch.
// The discarded state is kept in refs/mig/saved; `mig agent restore <id>` brings it back.
pub fn abort(repo: &Repository) -> Result<Oid> {
  require(repo)?;
  let start = repo.refname_to_id(START)?;
  let base = repo.refname_to_id(BASE)?;
  let saved = snapshot(repo, "mig: discarded agent work", Some(repo.refname_to_id(CP)?))?;
  set_ref(repo, SAVED, saved)?;
  oplog::record(repo, "agent abort", || {
    if repo::head_commit(repo)?.map(|c| c.id()) != Some(start) {
      repo::move_head(repo, start, "mig: agent abort")?;
    }
    Ok(())
  })?;
  restore_tree(repo, base)?;
  clear(repo)?;
  Ok(saved)
}

pub struct Status {
  pub session: Session,
  pub checkpoints: usize,
  pub commits: usize,
  pub changed_files: usize,
}

pub fn status(repo: &Repository) -> Result<Option<Status>> {
  let Some(s) = session(repo) else { return Ok(None) };
  let start = repo.refname_to_id(START)?;
  Ok(Some(Status {
    session: s,
    checkpoints: checkpoints(repo)?.len().saturating_sub(1),
    commits: rev::position(repo, &start.to_string()).unwrap_or(0),
    changed_files: repo::changes(repo)?.len(),
  }))
}
