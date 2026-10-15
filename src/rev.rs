use anyhow::{Context, Result, anyhow, bail};
use git2::{Commit, Repository};

use crate::repo;

// Commits along the first-parent line from HEAD, newest first.
pub fn chain(repo: &Repository, limit: usize) -> Result<Vec<Commit<'_>>> {
  let mut out = Vec::new();
  let mut cur = repo::head_commit(repo)?;
  while let Some(c) = cur {
    if out.len() >= limit {
      break;
    }
    cur = c.parents().next();
    out.push(c);
  }
  Ok(out)
}

// "@" = HEAD, short numbers = position in `mig log` (1 = newest), anything else goes to revparse.
pub fn resolve<'r>(repo: &'r Repository, spec: &str) -> Result<Commit<'r>> {
  if spec == "@" {
    return repo::head_commit(repo)?.ok_or_else(|| anyhow!("no commits yet"));
  }
  if spec.len() <= 4 && spec.chars().all(|c| c.is_ascii_digit()) {
    let n: usize = spec.parse()?;
    if n == 0 {
      bail!("commit numbers start at 1");
    }
    let mut ch = chain(repo, n)?;
    if ch.len() < n {
      bail!("there is no commit #{n}");
    }
    return Ok(ch.swap_remove(n - 1));
  }
  let obj = repo
    .revparse_single(spec)
    .with_context(|| format!("unknown revision: {spec}"))?;
  Ok(obj.peel_to_commit()?)
}

// Position (0-based, newest first) of a revision on the current first-parent line.
pub fn position(repo: &Repository, spec: &str) -> Result<usize> {
  let target = resolve(repo, spec)?.id();
  let mut cur = repo::head_commit(repo)?;
  let mut i = 0;
  while let Some(c) = cur {
    if c.id() == target {
      return Ok(i);
    }
    cur = c.parents().next();
    i += 1;
  }
  bail!("{spec} is not on the current branch")
}

// "a..b" or "a" -> inclusive (newer, older) positions.
pub fn range(repo: &Repository, spec: &str) -> Result<(usize, usize)> {
  let (a, b) = match spec.split_once("..") {
    Some((a, b)) => (position(repo, a)?, position(repo, b)?),
    None => {
      let p = position(repo, spec)?;
      (p, p)
    }
  };
  Ok((a.min(b), a.max(b)))
}
