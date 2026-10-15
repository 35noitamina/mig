use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use git2::{Oid, Repository};
use ignore::WalkBuilder;
use ignore::gitignore::GitignoreBuilder;

use crate::config::{self, Kind};
use crate::github::{self, Created};
use crate::guard::SaveBlocked;
use crate::scan::{self, Finding};
use crate::{oplog, ops, repo};

const MAX_PREVIEW_FILES: usize = 100_000;
const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024;
// Stop reading file contents for the secret preview after this much.
const MAX_SCAN_TOTAL: u64 = 256 * 1024 * 1024;

pub fn gitignore_for(kind: Option<Kind>) -> String {
  let mut s = String::from("# secrets\n.env\n.env.*\n!.env.example\n\n# OS files\n.DS_Store\nThumbs.db\n");
  let extra = match kind {
    Some(Kind::Rust) => "\n# Rust\n/target/\n",
    Some(Kind::Node) => "\n# Node.js\nnode_modules/\ndist/\n",
    Some(Kind::Python) => "\n# Python\n__pycache__/\n*.pyc\n.venv/\nvenv/\n",
    Some(Kind::Go) | None => "",
  };
  s.push_str(extra);
  s
}

fn same_dir(a: &Path, b: &Path) -> bool {
  match (a.canonicalize(), b.canonicalize()) {
    (Ok(a), Ok(b)) => a == b,
    _ => false,
  }
}

// Ok(true) if `dir` already is a repository; errors if it sits inside another one.
fn existing(dir: &Path) -> Result<bool> {
  match Repository::discover(dir) {
    Ok(r) => match r.workdir() {
      Some(w) if same_dir(w, dir) => Ok(true),
      Some(w) => bail!(
        "{} is inside the repository at {}; pick a folder outside it",
        dir.display(),
        w.display()
      ),
      None => bail!("{} is inside a bare repository", dir.display()),
    },
    Err(_) => Ok(false),
  }
}

pub struct Preview {
  pub kind: Option<Kind>,
  #[cfg_attr(not(feature = "gui"), allow(dead_code))]
  pub already_repo: bool,
  // Proposed contents; None when the folder already has a .gitignore.
  pub gitignore: Option<String>,
  pub files: usize,
  pub bytes: u64,
  pub secrets: Vec<Finding>,
  pub large: Vec<(String, u64)>,
}

pub fn preview(dir: &Path) -> Result<Preview> {
  let already_repo = existing(dir)?;
  let kind = config::project_kind(dir);
  let gitignore = (!dir.join(".gitignore").exists()).then(|| gitignore_for(kind));
  let mut p = Preview {
    kind,
    already_repo,
    gitignore,
    files: 0,
    bytes: 0,
    secrets: vec![],
    large: vec![],
  };
  if !dir.exists() {
    return Ok(p);
  }
  let cfg = config::load_dir(dir)?;
  let max = (cfg.save.max_file_mb * 1024.0 * 1024.0) as u64;
  let scan_cfg = scan::Config::load_dir(dir)?;
  let files = list_files(dir, p.gitignore.as_deref().unwrap_or(""))?;
  let mut scanned = 0u64;
  for (rel, size) in files {
    p.files += 1;
    p.bytes += size;
    if size > max {
      p.large.push((rel, size));
      continue;
    }
    if size <= MAX_SCAN_BYTES
      && scanned + size <= MAX_SCAN_TOTAL
      && let Ok(bytes) = std::fs::read(dir.join(&rel))
      && !bytes.contains(&0)
    {
      scanned += size;
      scan::scan_text(&scan_cfg, &rel, &String::from_utf8_lossy(&bytes), &mut p.secrets);
    }
  }
  Ok(p)
}

// Files a first save would pick up: existing .gitignore rules plus `extra_ignore`.
fn list_files(dir: &Path, extra_ignore: &str) -> Result<Vec<(String, u64)>> {
  let mut gb = GitignoreBuilder::new(dir);
  for line in extra_ignore.lines() {
    gb.add_line(None, line)?;
  }
  let extra = gb.build()?;
  let mut out = Vec::new();
  let walk = WalkBuilder::new(dir)
    .hidden(false)
    .require_git(false)
    .filter_entry(|e| e.file_name() != ".git")
    .build();
  for entry in walk {
    let Ok(e) = entry else { continue };
    if !e.file_type().is_some_and(|t| t.is_file()) {
      continue;
    }
    if extra.matched_path_or_any_parents(e.path(), false).is_ignore() {
      continue;
    }
    let rel = e
      .path()
      .strip_prefix(dir)
      .unwrap_or(e.path())
      .to_string_lossy()
      .replace('\\', "/");
    out.push((rel, e.metadata().map(|m| m.len()).unwrap_or(0)));
    if out.len() >= MAX_PREVIEW_FILES {
      break;
    }
  }
  Ok(out)
}

pub struct GithubOptions {
  pub name: String,
  pub private: bool,
  pub ssh: bool,
}

#[derive(Default)]
pub struct CreateOptions {
  pub dir: PathBuf,
  pub gitignore: Option<String>,
  pub save: bool,
  pub message: Option<String>,
  pub force: bool,
  pub remote: Option<String>,
  pub github: Option<GithubOptions>,
}

#[derive(Default)]
pub struct Outcome {
  pub dir: PathBuf,
  pub created: bool,
  pub wrote_gitignore: bool,
  pub saved: Option<Oid>,
  pub warnings: Vec<String>,
  pub blocked: Option<SaveBlocked>,
  pub github: Option<Created>,
  pub pushed: usize,
  pub notes: Vec<String>,
}

pub fn create(opts: CreateOptions) -> Result<Outcome> {
  let dir = opts.dir;
  std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
  let mut out = Outcome {
    dir: dir.clone(),
    ..Default::default()
  };
  if !existing(&dir)? {
    if let Some(g) = &opts.gitignore
      && !g.trim().is_empty()
      && !dir.join(".gitignore").exists()
    {
      std::fs::write(dir.join(".gitignore"), g)?;
      out.wrote_gitignore = true;
    }
    repo::init(&dir)?;
    out.created = true;
  }
  let repo = repo::open(&dir)?;

  if let Some(url) = &opts.remote {
    add_remote(&repo, "origin", url)?;
  }
  if let Some(gh) = &opts.github {
    out.github = Some(create_github(&repo, gh)?);
  }

  if opts.save {
    let msg = opts.message.as_deref().unwrap_or("Initial commit");
    match oplog::record(&repo, "save", || ops::save(&repo, Some(msg), &[], opts.force)) {
      Ok(s) => {
        out.saved = Some(s.oid);
        out.warnings = s.warnings;
      }
      Err(e) => match e.downcast::<SaveBlocked>() {
        Ok(b) => out.blocked = Some(b),
        Err(e) => out.notes.push(format!("not saved: {e:#}")),
      },
    }
  }

  if out.github.is_some() {
    match push_initial(&repo, opts.force) {
      Ok(n) => out.pushed = n,
      Err(e) => out.notes.push(e),
    }
  }
  Ok(out)
}

// Create the GitHub repository and register it as origin.
pub fn create_github(repo: &Repository, gh: &GithubOptions) -> Result<Created> {
  if repo.find_remote("origin").is_ok() {
    bail!("origin is already set; remove it with `mig remote rm origin` first");
  }
  let created = github::create_repo(&gh.name, gh.private, "")?;
  let url = if gh.ssh { &created.ssh_url } else { &created.clone_url };
  add_remote(repo, "origin", url)?;
  Ok(created)
}

// Push what exists so far; Ok(0) when there is nothing to push yet.
pub fn push_initial(repo: &Repository, force: bool) -> Result<usize, String> {
  if repo::head_commit(repo).ok().flatten().is_none() {
    return Ok(0);
  }
  oplog::record(repo, "sync", || ops::sync(repo, force))
    .map(|r| r.pushed)
    .map_err(|e| format!("push failed: {e:#}; run `mig sync` later"))
}

pub fn remotes(repo: &Repository) -> Result<Vec<(String, String)>> {
  let names = repo.remotes()?;
  let mut out = Vec::new();
  for n in names.iter().flatten().flatten() {
    let r = repo.find_remote(n)?;
    out.push((n.to_string(), r.url().unwrap_or("").to_string()));
  }
  Ok(out)
}

pub fn add_remote(repo: &Repository, name: &str, url: &str) -> Result<()> {
  if repo.find_remote(name).is_ok() {
    bail!("remote {name} already exists");
  }
  repo.remote(name, url)?;
  Ok(())
}

pub fn remove_remote(repo: &Repository, name: &str) -> Result<()> {
  repo
    .remote_delete(name)
    .with_context(|| format!("no remote named {name}"))?;
  Ok(())
}

pub fn default_name(dir: &Path) -> String {
  dir
    .canonicalize()
    .ok()
    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    .unwrap_or_else(|| "repo".into())
}
