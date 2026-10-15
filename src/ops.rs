use std::cell::RefCell;

use anyhow::{Context, Result, anyhow, bail};
use git2::build::CheckoutBuilder;
use git2::{
  BranchType, Commit, Cred, CredentialType, FetchOptions, IndexAddOption, Oid, PushOptions, RemoteCallbacks,
  Repository, Sort,
};

use crate::scan::{self, SecretsFound};
use crate::{repo, rev};

pub fn save(repo: &Repository, message: Option<&str>, paths: &[String], force: bool) -> Result<Oid> {
  let mut index = repo.index()?;
  let specs: Vec<&str> = if paths.is_empty() {
    vec!["*"]
  } else {
    paths.iter().map(String::as_str).collect()
  };
  let result = (|| {
    index.add_all(specs.iter(), IndexAddOption::DEFAULT, None)?;
    index.update_all(specs.iter(), None)?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let parent = repo::head_commit(repo)?;
    let parent_tree = parent.as_ref().map(|p| p.tree()).transpose()?;
    if parent_tree.as_ref().map(|t| t.id()) == Some(tree.id()) || (parent.is_none() && tree.is_empty()) {
      bail!("nothing to save");
    }
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
    if !force {
      let found = scan::scan_diff(repo, &diff)?;
      if !found.is_empty() {
        return Err(SecretsFound(found).into());
      }
    }
    let msg = match message {
      Some(m) if !m.trim().is_empty() => m.to_string(),
      _ => auto_message(&diff),
    };
    let sig = repo::signature(repo)?;
    let parents: Vec<&Commit> = parent.iter().collect();
    let oid = repo.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &parents)?;
    index.write()?;
    Ok(oid)
  })();
  if result.is_err() {
    index.read(true)?;
  }
  result
}

fn auto_message(diff: &git2::Diff<'_>) -> String {
  let names: Vec<String> = diff
    .deltas()
    .filter_map(|d| d.new_file().path().or(d.old_file().path()))
    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    .collect();
  match names.len() {
    0 => "Update".into(),
    1..=3 => format!("Update {}", names.join(", ")),
    n => format!("Update {} (+{} more)", names[..3].join(", "), n - 3),
  }
}

// Recreate `commits` (oldest first) on top of `onto`. With keep_trees the snapshots stay
// as they are; otherwise each commit's changes are cherry-picked.
fn replay(repo: &Repository, commits: &[Commit<'_>], mut onto: Option<Oid>, keep_trees: bool) -> Result<Option<Oid>> {
  let committer = repo::signature(repo)?;
  for c in commits {
    let tree = if keep_trees {
      c.tree()?
    } else {
      if c.parent_count() > 1 {
        bail!("cannot replay merge commit {}", repo::short(c.id()));
      }
      let base = repo.find_commit(onto.ok_or_else(|| anyhow!("cannot replay onto an empty history"))?)?;
      let mut idx = repo.cherrypick_commit(c, &base, 0, None)?;
      if idx.has_conflicts() {
        bail!(
          "conflict while replaying {} \"{}\"",
          repo::short(c.id()),
          repo::summary(c)
        );
      }
      repo.find_tree(idx.write_tree_to(repo)?)?
    };
    let mut parent_ids: Vec<Oid> = onto.into_iter().collect();
    parent_ids.extend(c.parent_ids().skip(1));
    let parents: Vec<Commit> = parent_ids
      .iter()
      .map(|&id| repo.find_commit(id))
      .collect::<Result<_, _>>()?;
    let parent_refs: Vec<&Commit> = parents.iter().collect();
    let msg = repo::message(c);
    onto = Some(repo.commit(None, &c.author(), &committer, &msg, &tree, &parent_refs)?);
  }
  Ok(onto)
}

fn commit_with(repo: &Repository, base: &Commit<'_>, parents: &[Oid], msg: &str, tree: &git2::Tree<'_>) -> Result<Oid> {
  let committer = repo::signature(repo)?;
  let ps: Vec<Commit> = parents
    .iter()
    .map(|&id| repo.find_commit(id))
    .collect::<Result<_, _>>()?;
  let refs: Vec<&Commit> = ps.iter().collect();
  Ok(repo.commit(None, &base.author(), &committer, msg, tree, &refs)?)
}

// Fold commits at positions newer..=older (0-based, newest first) into one.
pub fn fold(repo: &Repository, newer: usize, older: usize, message: Option<&str>) -> Result<Oid> {
  if newer == older {
    bail!("need at least two commits to fold");
  }
  let chain = rev::chain(repo, older + 1)?;
  if chain.len() <= older {
    bail!("range is out of history");
  }
  let range = &chain[newer..=older];
  if range.iter().any(|c| c.parent_count() > 1) {
    bail!("cannot fold merge commits");
  }
  let oldest = &chain[older];
  let msg = match message {
    Some(m) => m.to_string(),
    None => range
      .iter()
      .rev()
      .map(|c| repo::message(c).trim_end().to_string())
      .collect::<Vec<_>>()
      .join("\n\n"),
  };
  let parents: Vec<Oid> = oldest.parent_ids().collect();
  let folded = commit_with(repo, oldest, &parents, &msg, &chain[newer].tree()?)?;
  let rest: Vec<Commit> = chain[..newer].iter().rev().cloned().collect();
  let tip = replay(repo, &rest, Some(folded), true)?.unwrap();
  repo::move_head(repo, tip, "mig: fold")?;
  Ok(tip)
}

pub fn reword(repo: &Repository, pos: usize, message: &str) -> Result<Oid> {
  let chain = rev::chain(repo, pos + 1)?;
  let target = &chain[pos];
  let parents: Vec<Oid> = target.parent_ids().collect();
  let new = commit_with(repo, target, &parents, message, &target.tree()?)?;
  let rest: Vec<Commit> = chain[..pos].iter().rev().cloned().collect();
  let tip = replay(repo, &rest, Some(new), true)?.unwrap();
  repo::move_head(repo, tip, "mig: reword")?;
  Ok(tip)
}

pub fn drop_commit(repo: &Repository, pos: usize) -> Result<Oid> {
  repo::ensure_clean(repo)?;
  let chain = rev::chain(repo, pos + 1)?;
  let target = &chain[pos];
  let Some(base) = target.parent_ids().next() else {
    bail!("cannot drop the root commit")
  };
  if target.parent_count() > 1 {
    bail!("cannot drop a merge commit");
  }
  let rest: Vec<Commit> = chain[..pos].iter().rev().cloned().collect();
  let tip = replay(repo, &rest, Some(base), false)?.unwrap();
  repo::move_head(repo, tip, "mig: drop")?;
  repo::checkout_head_force(repo)?;
  Ok(tip)
}

pub struct BranchInfo {
  pub name: String,
  pub current: bool,
  pub summary: String,
}

pub fn branches(repo: &Repository) -> Result<Vec<BranchInfo>> {
  let cur = repo::branch_name(repo);
  let mut out = Vec::new();
  for b in repo.branches(Some(BranchType::Local))? {
    let (b, _) = b?;
    let name = b.name()?.unwrap_or_default().to_string();
    let summary = b.get().peel_to_commit().map(|c| repo::summary(&c)).unwrap_or_default();
    out.push(BranchInfo {
      current: cur.as_deref() == Some(&name),
      name,
      summary,
    });
  }
  Ok(out)
}

pub fn branch_create(repo: &Repository, name: &str, at: Option<&str>) -> Result<()> {
  let c = rev::resolve(repo, at.unwrap_or("@"))?;
  repo.branch(name, &c, false)?;
  Ok(())
}

pub fn branch_delete(repo: &Repository, name: &str) -> Result<()> {
  if repo::branch_name(repo).as_deref() == Some(name) {
    bail!("cannot delete the current branch");
  }
  repo.find_branch(name, BranchType::Local)?.delete()?;
  Ok(())
}

pub fn switch(repo: &Repository, name: &str, create: bool) -> Result<()> {
  if create {
    branch_create(repo, name, None)?;
  }
  let branch = match repo.find_branch(name, BranchType::Local) {
    Ok(b) => b,
    Err(_) => {
      let remote = repo
        .find_branch(&format!("origin/{name}"), BranchType::Remote)
        .with_context(|| format!("no branch named {name}"))?;
      let mut b = repo.branch(name, &remote.get().peel_to_commit()?, false)?;
      b.set_upstream(Some(&format!("origin/{name}")))?;
      b
    }
  };
  let refname = branch.get().name()?.to_string();
  let commit = branch.get().peel_to_commit()?;
  let mut cb = CheckoutBuilder::new();
  cb.safe();
  repo
    .checkout_tree(commit.as_object(), Some(&mut cb))
    .context("switching would overwrite unsaved changes; run `mig save` first")?;
  repo.set_head(&refname)?;
  Ok(())
}

fn callbacks<'a>() -> RemoteCallbacks<'a> {
  let mut cb = RemoteCallbacks::new();
  let mut tried = 0u32;
  cb.credentials(move |url, user, allowed| {
    tried += 1;
    let user = user.unwrap_or("git");
    if allowed.contains(CredentialType::SSH_KEY) {
      if tried == 1 {
        return Cred::ssh_key_from_agent(user);
      }
      if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let ssh = std::path::Path::new(&home).join(".ssh");
        let keys = ["id_ed25519", "id_ecdsa", "id_rsa"];
        if let Some(k) = keys
          .iter()
          .map(|k| ssh.join(k))
          .filter(|p| p.exists())
          .nth(tried as usize - 2)
        {
          return Cred::ssh_key(user, None, &k, None);
        }
      }
    }
    if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) && tried <= 2 {
      let cfg = git2::Config::open_default()?;
      return Cred::credential_helper(&cfg, url, Some(user).filter(|u| *u != "git"));
    }
    if allowed.contains(CredentialType::DEFAULT) && tried <= 3 {
      return Cred::default();
    }
    Err(git2::Error::from_str(
      "authentication failed (configure ssh-agent or a git credential helper)",
    ))
  });
  cb
}

#[derive(Default)]
pub struct SyncReport {
  pub remote: String,
  pub branch: String,
  pub fast_forwarded: bool,
  pub rebased: usize,
  pub pushed: usize,
}

pub fn sync(repo: &Repository, force: bool) -> Result<SyncReport> {
  let branch = repo::branch_name(repo).ok_or_else(|| anyhow!("HEAD is detached; switch to a branch first"))?;
  let cfg = repo.config()?;
  let remote_name = cfg
    .get_string(&format!("branch.{branch}.remote"))
    .unwrap_or_else(|_| "origin".into());
  let merge = cfg
    .get_string(&format!("branch.{branch}.merge"))
    .unwrap_or_else(|_| format!("refs/heads/{branch}"));
  let remote_branch = merge.strip_prefix("refs/heads/").unwrap_or(&merge).to_string();
  let mut remote = repo
    .find_remote(&remote_name)
    .with_context(|| format!("no remote named {remote_name}"))?;
  let mut report = SyncReport {
    remote: remote_name.clone(),
    branch: branch.clone(),
    ..Default::default()
  };

  let mut fo = FetchOptions::new();
  fo.remote_callbacks(callbacks());
  remote.fetch::<&str>(&[], Some(&mut fo), None).context("fetch failed")?;

  let local = repo::head_commit(repo)?
    .ok_or_else(|| anyhow!("nothing to sync yet"))?
    .id();
  let tracking = format!("refs/remotes/{remote_name}/{remote_branch}");
  let upstream = repo.refname_to_id(&tracking).ok();
  let mut tip = local;

  if let Some(up) = upstream {
    let (ahead, behind) = repo.graph_ahead_behind(local, up)?;
    if behind > 0 {
      repo::ensure_clean(repo)?;
      if ahead == 0 {
        tip = up;
        report.fast_forwarded = true;
      } else {
        let mine = commits_between(repo, local, Some(up))?;
        report.rebased = mine.len();
        tip = replay(repo, &mine, Some(up), false)?.unwrap();
      }
      repo::move_head(repo, tip, "mig: sync")?;
      repo::checkout_head_force(repo)?;
    }
  }

  let unpushed = commits_between(repo, tip, upstream)?;
  if unpushed.is_empty() {
    return Ok(report);
  }
  if !force {
    let mut found = Vec::new();
    for c in &unpushed {
      let parent_tree = c.parents().next().map(|p| p.tree()).transpose()?;
      found.extend(scan::scan_trees(repo, parent_tree.as_ref(), &c.tree()?)?);
    }
    if !found.is_empty() {
      return Err(SecretsFound(found).into());
    }
  }

  let rejected = RefCell::new(None::<String>);
  let mut cb = callbacks();
  cb.push_update_reference(|_, status| {
    if let Some(s) = status {
      *rejected.borrow_mut() = Some(s.to_string());
    }
    Ok(())
  });
  let mut po = PushOptions::new();
  po.remote_callbacks(cb);
  let spec = format!("refs/heads/{branch}:refs/heads/{remote_branch}");
  remote.push(&[spec.as_str()], Some(&mut po)).context("push failed")?;
  if let Some(s) = rejected.borrow().clone() {
    bail!("push rejected: {s}");
  }
  repo.reference(&tracking, tip, true, "mig: sync")?;
  if upstream.is_none() {
    let mut cfg = repo.config()?;
    cfg.set_str(&format!("branch.{branch}.remote"), &remote_name)?;
    cfg.set_str(
      &format!("branch.{branch}.merge"),
      &format!("refs/heads/{remote_branch}"),
    )?;
  }
  report.pushed = unpushed.len();
  Ok(report)
}

// Commits reachable from `tip` but not from `hide`, oldest first.
fn commits_between(repo: &Repository, tip: Oid, hide: Option<Oid>) -> Result<Vec<Commit<'_>>> {
  let mut walk = repo.revwalk()?;
  walk.push(tip)?;
  if let Some(h) = hide {
    walk.hide(h)?;
  }
  walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
  walk.map(|id| Ok(repo.find_commit(id?)?)).collect()
}
