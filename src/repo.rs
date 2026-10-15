use std::path::Path;

use anyhow::{Context, Result, bail};
use git2::build::CheckoutBuilder;
use git2::{Commit, ErrorCode, Oid, Repository, RepositoryInitOptions, Signature, Status, StatusOptions};

pub fn open(dir: &Path) -> Result<Repository> {
  Repository::discover(dir).with_context(|| format!("not a repository: {}", dir.display()))
}

pub fn init(dir: &Path) -> Result<Repository> {
  let branch = git2::Config::open_default()
    .and_then(|c| c.get_string("init.defaultBranch"))
    .unwrap_or_else(|_| "main".into());
  let mut opts = RepositoryInitOptions::new();
  opts.initial_head(&branch);
  Ok(Repository::init_opts(dir, &opts)?)
}

pub fn signature(repo: &Repository) -> Result<Signature<'static>> {
  repo
    .signature()
    .context("user.name / user.email are not set (git config --global user.name ...)")
}

pub fn head_commit(repo: &Repository) -> Result<Option<Commit<'_>>> {
  match repo.head() {
    Ok(h) => Ok(Some(h.peel_to_commit()?)),
    Err(e) if e.code() == ErrorCode::UnbornBranch || e.code() == ErrorCode::NotFound => Ok(None),
    Err(e) => Err(e.into()),
  }
}

pub fn branch_name(repo: &Repository) -> Option<String> {
  let head = repo.find_reference("HEAD").ok()?;
  let target = head.symbolic_target().ok()??;
  target.strip_prefix("refs/heads/").map(str::to_string)
}

pub fn summary(c: &Commit<'_>) -> String {
  c.summary().ok().flatten().unwrap_or("").to_string()
}

pub fn message(c: &Commit<'_>) -> String {
  c.message().unwrap_or("").to_string()
}

pub struct Change {
  pub path: String,
  pub kind: char,
}

pub fn changes(repo: &Repository) -> Result<Vec<Change>> {
  let mut opts = StatusOptions::new();
  opts
    .include_untracked(true)
    .recurse_untracked_dirs(true)
    .include_ignored(false);
  let statuses = repo.statuses(Some(&mut opts))?;
  let mut out = Vec::new();
  for e in statuses.iter() {
    let s = e.status();
    let kind = if s.is_conflicted() {
      'C'
    } else if s.contains(Status::WT_NEW) && !s.contains(Status::INDEX_NEW) {
      '?'
    } else if s.intersects(Status::INDEX_NEW) {
      'A'
    } else if s.intersects(Status::INDEX_DELETED | Status::WT_DELETED) {
      'D'
    } else if s.intersects(Status::INDEX_RENAMED | Status::WT_RENAMED) {
      'R'
    } else if s
      .intersects(Status::INDEX_MODIFIED | Status::WT_MODIFIED | Status::INDEX_TYPECHANGE | Status::WT_TYPECHANGE)
    {
      'M'
    } else {
      continue;
    };
    out.push(Change {
      path: e.path().unwrap_or_default().to_string(),
      kind,
    });
  }
  Ok(out)
}

pub fn ensure_clean(repo: &Repository) -> Result<()> {
  if !changes(repo)?.is_empty() {
    bail!("you have unsaved changes; run `mig save` first (or pass --force to discard them)");
  }
  Ok(())
}

// Point HEAD (or the branch it refers to) at `oid`.
pub fn move_head(repo: &Repository, oid: Oid, msg: &str) -> Result<()> {
  let head = repo.find_reference("HEAD")?;
  match head.symbolic_target()? {
    Some(name) => {
      repo.reference(name, oid, true, msg)?;
    }
    None => repo.set_head_detached(oid)?,
  }
  Ok(())
}

pub fn checkout_head_force(repo: &Repository) -> Result<()> {
  if head_commit(repo)?.is_none() {
    let mut index = repo.index()?;
    index.clear()?;
    index.write()?;
    return Ok(());
  }
  let mut cb = CheckoutBuilder::new();
  cb.force();
  repo.checkout_head(Some(&mut cb))?;
  Ok(())
}

pub fn short(oid: Oid) -> String {
  oid.to_string()[..7].to_string()
}
