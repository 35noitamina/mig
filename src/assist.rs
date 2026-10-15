use anyhow::{Context, Result, anyhow, bail};
use git2::{DiffOptions, Repository};
use serde::Deserialize;

use crate::ai::{self, AiConfig, Reply};
use crate::{absorb, oplog, ops, repo, rev, scan};

fn pending_diff(repo: &Repository) -> Result<String> {
  let head = repo::head_commit(repo)?.map(|c| c.id());
  diff_since(repo, head)
}

// Working tree (including untracked files) against the given commit.
fn diff_since(repo: &Repository, base: Option<git2::Oid>) -> Result<String> {
  let head = base.map(|o| repo.find_commit(o).and_then(|c| c.tree())).transpose()?;
  let mut opts = DiffOptions::new();
  opts
    .include_untracked(true)
    .recurse_untracked_dirs(true)
    .show_untracked_content(true)
    .max_size(2 * 1024 * 1024);
  let d = repo.diff_tree_to_workdir_with_index(head.as_ref(), Some(&mut opts))?;
  crate::cli::render_diff(&d, false)
}

// (header describing the commit, its diff)
fn commit_diff(repo: &Repository, spec: &str) -> Result<(String, String)> {
  let c = rev::resolve(repo, spec)?;
  let parent = c.parents().next().map(|p| p.tree()).transpose()?;
  let mut opts = DiffOptions::new();
  opts.max_size(2 * 1024 * 1024);
  let d = repo.diff_tree_to_tree(parent.as_ref(), Some(&c.tree()?), Some(&mut opts))?;
  let header = format!(
    "Commit {} by {}\n\n{}",
    repo::short(c.id()),
    c.author().name().unwrap_or("?"),
    repo::message(&c).trim()
  );
  Ok((header, crate::cli::render_diff(&d, false)?))
}

// The change to talk about: a commit, or the uncommitted work when `spec` is None.
fn subject(repo: &Repository, spec: Option<&str>) -> Result<String> {
  let (header, diff) = match spec {
    Some(s) => commit_diff(repo, s)?,
    None => (
      "Uncommitted changes in the working tree".to_string(),
      pending_diff(repo)?,
    ),
  };
  if diff.trim().is_empty() {
    bail!("there are no changes to look at");
  }
  let (diff, _) = ai::clip(&diff, ai::MAX_INPUT_CHARS);
  Ok(format!("{header}\n\n```diff\n{diff}```\n"))
}

pub fn commit_message(repo: &Repository, cfg: &AiConfig) -> Result<Reply> {
  message_for(cfg, &pending_diff(repo)?)
}

// Message for everything from `base` to the current working tree, e.g. a whole agent session.
pub fn commit_message_since(repo: &Repository, cfg: &AiConfig, base: git2::Oid) -> Result<Reply> {
  message_for(cfg, &diff_since(repo, Some(base))?)
}

fn message_for(cfg: &AiConfig, diff: &str) -> Result<Reply> {
  if diff.trim().is_empty() {
    bail!("nothing to describe: there are no uncommitted changes");
  }
  let (diff, _) = ai::clip(diff, ai::MAX_INPUT_CHARS);
  let system = "You write git commit messages. Reply with the message only: a summary line of at most 72 \
    characters in the imperative mood, then, only if it adds something, a blank line and a short body \
    explaining what changed and why. No code fences, no quotes, no preamble.";
  let mut r = ai::ask(
    cfg,
    system,
    &format!("Write a commit message for this change.\n\n```diff\n{diff}```"),
  )?;
  r.text = r.text.trim_matches('`').trim().trim_matches('"').trim().to_string();
  Ok(r)
}

pub fn explain(repo: &Repository, cfg: &AiConfig, spec: Option<&str>) -> Result<Reply> {
  let system = "You explain code changes to a developer reading the history of a repository. Say what the \
    change does and, where the diff makes it clear, why. Be concise; use short Markdown sections.";
  ai::ask(cfg, system, &subject(repo, spec)?)
}

pub fn review(repo: &Repository, cfg: &AiConfig, spec: Option<&str>) -> Result<Reply> {
  let system = "You review code changes. List concrete problems you can point to in the diff (bugs, \
    security issues, data loss, missing error handling, risky behaviour), most severe first, each with \
    the file and line and a suggested fix. If there is nothing important, say so in one sentence. Markdown.";
  ai::ask(cfg, system, &subject(repo, spec)?)
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Group {
  // `mig log` numbers, inclusive; from < to.
  pub from: usize,
  pub to: usize,
  pub message: String,
}

#[derive(Deserialize)]
struct Plan {
  groups: Vec<Group>,
}

// Suggest how to fold the latest unpushed commits into meaningful ones.
pub fn tidy_plan(repo: &Repository, cfg: &AiConfig, limit: usize) -> Result<Vec<Group>> {
  let chain = rev::chain(repo, limit)?;
  let n = absorb::mutable(repo, &chain)?.len().min(limit);
  if n < 2 {
    bail!("need at least two unpushed commits to tidy");
  }
  let mut listing = String::new();
  for (i, c) in chain[..n].iter().enumerate() {
    listing.push_str(&format!("#{} {}\n", i + 1, repo::summary(c)));
    let parent = c.parents().next().map(|p| p.tree()).transpose()?;
    let d = repo.diff_tree_to_tree(parent.as_ref(), Some(&c.tree()?), None)?;
    let files: Vec<String> = d
      .deltas()
      .filter_map(|x| x.new_file().path().or(x.old_file().path()))
      .map(|p| p.display().to_string())
      .take(15)
      .collect();
    if let Ok(s) = d.stats() {
      listing.push_str(&format!(
        "   +{} -{} in {}\n",
        s.insertions(),
        s.deletions(),
        files.join(", ")
      ));
    }
  }
  let system = "You tidy up git history. You get commits numbered from newest (#1) to oldest, with the files \
    they touch. Group neighbouring commits that belong to the same logical change, and write one commit \
    message for each group. Only group commits whose numbers are consecutive, never reorder, and leave out \
    commits that should stay alone. Reply with JSON only, in exactly this shape: \
    {\"groups\": [{\"from\": 1, \"to\": 3, \"message\": \"Summary line\\n\\nOptional body\"}]} where from < to.";
  let r = ai::ask(cfg, system, &listing)?;
  let groups = parse_plan(&r.text)?;
  validate(&groups, n)?;
  Ok(groups)
}

fn parse_plan(text: &str) -> Result<Vec<Group>> {
  let start = text
    .find('{')
    .ok_or_else(|| anyhow!("the model did not return a plan"))?;
  let end = text
    .rfind('}')
    .ok_or_else(|| anyhow!("the model did not return a plan"))?;
  let plan: Plan = serde_json::from_str(&text[start..=end]).context("the model's plan was not valid JSON")?;
  Ok(plan.groups)
}

fn validate(groups: &[Group], n: usize) -> Result<()> {
  let mut sorted: Vec<&Group> = groups.iter().collect();
  sorted.sort_by_key(|g| g.from);
  let mut last = 0;
  for g in sorted {
    if g.from < 1 || g.to > n || g.from >= g.to || g.message.trim().is_empty() {
      bail!("the model's plan has an invalid group #{}..#{}", g.from, g.to);
    }
    if g.from <= last {
      bail!("the model's plan has overlapping groups");
    }
    last = g.to;
  }
  Ok(())
}

// Oldest groups first, so the numbers of the newer ones stay valid.
pub fn apply_tidy(repo: &Repository, groups: &[Group]) -> Result<()> {
  let mut sorted = groups.to_vec();
  sorted.sort_by_key(|g| std::cmp::Reverse(g.from));
  oplog::record(repo, "ai tidy", || {
    for g in &sorted {
      ops::fold(repo, g.from - 1, g.to - 1, Some(g.message.trim()))?;
    }
    Ok(())
  })
}

pub struct ContextOpts {
  pub diff: bool,
  pub files: Vec<String>,
  pub commits: usize,
}

// A Markdown briefing about the repository for pasting into an AI chat, with secrets redacted.
pub fn context(repo: &Repository, opts: &ContextOpts) -> Result<(String, usize)> {
  let root = repo.workdir().context("bare repositories are not supported")?;
  let name = root
    .file_name()
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_default();
  let mut out = format!("# Repository: {name}\n\n");
  out.push_str(&format!(
    "Branch: {}\n\n",
    repo::branch_name(repo).unwrap_or_else(|| "(detached)".into())
  ));

  let mut files: Vec<String> = repo
    .index()?
    .iter()
    .map(|e| String::from_utf8_lossy(&e.path).into_owned())
    .collect();
  files.sort();
  out.push_str(&format!("## Files ({} tracked)\n\n```\n", files.len()));
  for f in files.iter().take(500) {
    out.push_str(f);
    out.push('\n');
  }
  if files.len() > 500 {
    out.push_str(&format!("... and {} more\n", files.len() - 500));
  }
  out.push_str("```\n\n");

  out.push_str("## Recent commits\n\n");
  for (i, c) in rev::chain(repo, opts.commits)?.iter().enumerate() {
    out.push_str(&format!("- #{} {} {}\n", i + 1, repo::short(c.id()), repo::summary(c)));
  }
  out.push('\n');

  let changes = repo::changes(repo)?;
  out.push_str("## Uncommitted changes\n\n");
  if changes.is_empty() {
    out.push_str("None.\n\n");
  } else {
    for c in changes.iter().take(200) {
      out.push_str(&format!("- {} {}\n", c.kind, c.path));
    }
    out.push('\n');
    if opts.diff {
      let (diff, _) = ai::clip(&pending_diff(repo)?, ai::MAX_INPUT_CHARS);
      out.push_str(&format!("```diff\n{diff}```\n\n"));
    }
  }

  for f in &opts.files {
    let text = std::fs::read_to_string(root.join(f)).with_context(|| format!("cannot read {f} as text"))?;
    let (text, _) = ai::clip(&text, ai::MAX_INPUT_CHARS);
    let lang = std::path::Path::new(f)
      .extension()
      .map(|e| e.to_string_lossy().into_owned())
      .unwrap_or_default();
    out.push_str(&format!("## File: {f}\n\n```{lang}\n{text}\n```\n\n"));
  }
  Ok(scan::redact(&out))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn plan_parsing_and_checks() {
    let g = parse_plan("Sure:\n```json\n{\"groups\":[{\"from\":1,\"to\":3,\"message\":\"Add x\"}]}\n```").unwrap();
    assert_eq!(
      g,
      vec![Group {
        from: 1,
        to: 3,
        message: "Add x".into()
      }]
    );
    assert!(validate(&g, 3).is_ok());
    assert!(validate(&g, 2).is_err());
    let overlap = vec![
      Group {
        from: 1,
        to: 2,
        message: "a".into(),
      },
      Group {
        from: 2,
        to: 3,
        message: "b".into(),
      },
    ];
    assert!(validate(&overlap, 5).is_err());
  }
}
