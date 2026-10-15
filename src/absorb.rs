use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use git2::{BlameOptions, Commit, DiffOptions, Oid, Repository, ResetType};

use crate::patch::{self, Body, Edit};
use crate::{ops, repo, rev};

const MAX_DEPTH: usize = 200;

pub struct Move {
  pub path: String,
  pub header: String,
  pub pos: usize,
  pub summary: String,
}

pub struct Plan {
  pub moves: Vec<Move>,
  pub left: usize,
}

// Commits that may be rewritten: unpushed, first-parent, no merges. Maps id -> position.
pub(crate) fn mutable(repo: &Repository, chain: &[Commit<'_>]) -> Result<HashMap<Oid, usize>> {
  let up = repo::upstream(repo);
  let mut out = HashMap::new();
  for (i, c) in chain.iter().enumerate() {
    if c.parent_count() > 1 {
      break;
    }
    if let Some(u) = up
      && (u == c.id() || repo.graph_descendant_of(u, c.id())?)
    {
      break;
    }
    out.insert(c.id(), i);
  }
  Ok(out)
}

pub fn absorb(repo: &Repository, dry_run: bool) -> Result<Plan> {
  let head = repo::head_commit(repo)?.ok_or_else(|| anyhow!("no commits yet"))?;
  let chain = rev::chain(repo, MAX_DEPTH)?;
  let movable = mutable(repo, &chain)?;
  if movable.is_empty() {
    bail!("every commit is already pushed; nothing can absorb changes");
  }

  let head_tree = head.tree()?;
  let mut opts = DiffOptions::new();
  opts.context_lines(0).interhunk_lines(0);
  let diff = repo.diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut opts))?;
  let files = patch::collect(repo, &diff)?;

  // target position -> [(file index, hunk index)]
  let mut targets: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
  let mut moves = Vec::new();
  let mut left = 0;
  for (fi, fc) in files.iter().enumerate() {
    let Body::Text { old, hunks } = &fc.body else {
      left += 1;
      continue;
    };
    let total = old.split_inclusive(|&b| b == b'\n').count();
    let mut bo = BlameOptions::new();
    bo.newest_commit(head.id());
    let blame = repo.blame_file(Path::new(&fc.path), Some(&mut bo))?;
    for (hi, h) in hunks.iter().enumerate() {
      let lines: Vec<usize> = if h.old_lines > 0 {
        (h.old_start..h.old_start + h.old_lines).collect()
      } else {
        [h.old_start, h.old_start + 1]
          .into_iter()
          .filter(|&n| n >= 1 && n <= total)
          .collect()
      };
      let owners: Vec<Option<Oid>> = lines
        .iter()
        .map(|&n| blame.get_line(n).map(|b| b.final_commit_id()))
        .collect();
      let target = match owners.first() {
        Some(Some(first)) if owners.iter().all(|o| *o == Some(*first)) => movable.get(first).copied(),
        _ => None,
      };
      match target {
        Some(pos) => {
          targets.entry(pos).or_default().push((fi, hi));
          moves.push(Move {
            path: fc.path.clone(),
            header: h.text.lines().next().unwrap_or("").to_string(),
            pos,
            summary: repo::summary(&chain[pos]),
          });
        }
        None => left += 1,
      }
    }
  }
  let plan = Plan { moves, left };
  if dry_run || targets.is_empty() {
    return Ok(plan);
  }

  let sig = repo::signature(repo)?;
  let mut fixups: HashMap<Oid, Oid> = HashMap::new();
  for (&pos, picks) in &targets {
    let mut by_file: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &(fi, hi) in picks {
      by_file.entry(fi).or_default().push(hi);
    }
    let edits = by_file
      .into_iter()
      .map(|(fi, his)| {
        let fc = &files[fi];
        let Body::Text { old, hunks } = &fc.body else {
          unreachable!()
        };
        let sel: Vec<_> = his.iter().map(|&hi| &hunks[hi]).collect();
        Edit::Write(fc.path.clone(), patch::apply_hunks(old, &sel), patch::text_mode(fc))
      })
      .collect();
    let tree = repo.find_tree(patch::build_tree(repo, &head_tree, edits)?)?;
    let fix = repo.commit(None, &sig, &sig, "mig absorb fixup", &tree, &[&head])?;
    fixups.insert(chain[pos].id(), fix);
  }

  let oldest = *targets.keys().last().unwrap();
  let mut onto = chain[oldest].parent_ids().next();
  let mut changed = false;
  for c in chain[..=oldest].iter().rev() {
    let mut tree_id = if changed {
      ops::pick(repo, c, onto.unwrap())?
    } else {
      c.tree_id()
    };
    let parents: Vec<Oid> = onto.into_iter().collect();
    if let Some(&fix) = fixups.get(&c.id()) {
      let tmp = ops::commit_with(repo, c, &parents, "tmp", &repo.find_tree(tree_id)?)?;
      tree_id = ops::pick(repo, &repo.find_commit(fix)?, tmp)?;
      changed = true;
    }
    onto = Some(ops::commit_with(
      repo,
      c,
      &parents,
      &repo::message(c),
      &repo.find_tree(tree_id)?,
    )?);
  }
  let tip = onto.unwrap();
  repo::move_head(repo, tip, "mig: absorb")?;
  repo.reset(repo.find_commit(tip)?.as_object(), ResetType::Mixed, None)?;
  Ok(plan)
}
