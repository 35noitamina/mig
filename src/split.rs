use anyhow::{Result, bail};
use git2::{Commit, Oid, Repository};

use crate::patch::{self, Body, Edit, FileChange, Hunk};
use crate::{ops, repo, rev};

// Split the commit at `pos` in two. `select` decides what goes into the first commit:
// it is called once per hunk for text changes and once (with None) for whole-file changes.
pub fn split(
  repo: &Repository,
  pos: usize,
  message: Option<&str>,
  select: &mut dyn FnMut(&FileChange, Option<&Hunk>) -> bool,
) -> Result<Oid> {
  let chain = rev::chain(repo, pos + 1)?;
  let Some(target) = chain.get(pos) else {
    bail!("there is no commit #{}", pos + 1)
  };
  if target.parent_count() > 1 {
    bail!("cannot split a merge commit");
  }
  let base_tree = match target.parents().next() {
    Some(p) => p.tree()?,
    None => repo.find_tree(repo.treebuilder(None)?.write()?)?,
  };
  let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&target.tree()?), None)?;
  let files = patch::collect(repo, &diff)?;

  let mut edits = Vec::new();
  for fc in &files {
    match &fc.body {
      Body::Text { old, hunks } => {
        let sel: Vec<&Hunk> = hunks.iter().filter(|h| select(fc, Some(h))).collect();
        if !sel.is_empty() {
          edits.push(Edit::Write(
            fc.path.clone(),
            patch::apply_hunks(old, &sel),
            patch::text_mode(fc),
          ));
        }
      }
      Body::Whole { .. } => {
        if select(fc, None) {
          edits.push(Edit::Set(fc.path.clone(), fc.new));
        }
      }
    }
  }
  let first_tree = patch::build_tree(repo, &base_tree, edits)?;
  if first_tree == base_tree.id() || first_tree == target.tree_id() {
    bail!("nothing to split: pick some, but not all, of the changes");
  }

  let msg = match message {
    Some(m) => m.to_string(),
    None => format!("{} (1/2)", repo::summary(target)),
  };
  let parents: Vec<Oid> = target.parent_ids().collect();
  let first = ops::commit_with(repo, target, &parents, &msg, &repo.find_tree(first_tree)?)?;
  let second = ops::commit_with(repo, target, &[first], &repo::message(target), &target.tree()?)?;
  let rest: Vec<Commit> = chain[..pos].iter().rev().cloned().collect();
  let tip = ops::replay(repo, &rest, Some(second), true)?.unwrap();
  repo::move_head(repo, tip, "mig: split")?;
  Ok(tip)
}

pub fn split_paths(repo: &Repository, pos: usize, paths: &[String], message: Option<&str>) -> Result<Oid> {
  let norm: Vec<String> = paths
    .iter()
    .map(|p| p.replace('\\', "/").trim_end_matches('/').to_string())
    .collect();
  split(repo, pos, message, &mut |fc, _| {
    norm
      .iter()
      .any(|p| fc.path == *p || fc.path.starts_with(&format!("{p}/")))
  })
}

// Changed paths of the commit at `pos`, for pickers.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn files(repo: &Repository, pos: usize) -> Result<Vec<String>> {
  let c = rev::chain(repo, pos + 1)?.swap_remove(pos);
  let base = c.parents().next().map(|p| p.tree()).transpose()?;
  let diff = repo.diff_tree_to_tree(base.as_ref(), Some(&c.tree()?), None)?;
  Ok(
    diff
      .deltas()
      .filter_map(|d| {
        d.new_file()
          .path()
          .or(d.old_file().path())
          .map(|p| p.to_string_lossy().replace('\\', "/"))
      })
      .collect(),
  )
}
