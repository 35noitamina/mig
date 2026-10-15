use std::fmt;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;
use git2::{DiffFormat, Repository, Tree};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;

#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
  pub path: String,
  pub line: usize,
  pub rule: &'static str,
  pub masked: String,
}

#[derive(Debug)]
pub struct SecretsFound(pub Vec<Finding>);

impl fmt::Display for SecretsFound {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{} possible secret(s) found", self.0.len())
  }
}

impl std::error::Error for SecretsFound {}

struct Rule {
  name: &'static str,
  re: Regex,
  entropy: bool,
}

fn rules() -> &'static [Rule] {
  static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
  RULES.get_or_init(|| {
    let r = |name, pat: &str, entropy| Rule {
      name,
      re: Regex::new(pat).unwrap(),
      entropy,
    };
    vec![
      r("AWS access key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", false),
      r(
        "GitHub token",
        r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})",
        false,
      ),
      r("Anthropic API key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}", false),
      r("OpenAI API key", r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}", false),
      r("Slack token", r"\bxox[abprs]-[A-Za-z0-9\-]{10,}", false),
      r("Google API key", r"\bAIza[0-9A-Za-z_\-]{35}", false),
      r("Stripe secret key", r"\b[sr]k_live_[0-9A-Za-z]{24,}", false),
      r("Private key", r"-----BEGIN (?:[A-Z]+ )*PRIVATE KEY-----", false),
      r(
        "Generic secret",
        r#"(?i)(?:api[_-]?key|secret|token|passw(?:or)?d)["']?\s*[:=]\s*["']?(?P<s>[A-Za-z0-9_\-+/=.]{16,})"#,
        true,
      ),
    ]
  })
}

pub struct Config {
  ignore: GlobSet,
  allow: Vec<String>,
}

pub use crate::config::CONFIG_FILE;

impl Config {
  pub fn load(repo: &Repository) -> Result<Config> {
    let fc = crate::config::load(repo)?;
    let mut b = GlobSetBuilder::new();
    for g in &fc.scan.ignore_paths {
      b.add(Glob::new(g)?);
    }
    Ok(Config {
      ignore: b.build()?,
      allow: fc.scan.allow,
    })
  }

  fn ignored(&self, path: &str) -> bool {
    path == CONFIG_FILE || self.ignore.is_match(path)
  }
}

fn entropy(s: &str) -> f64 {
  let mut counts = [0usize; 256];
  for b in s.bytes() {
    counts[b as usize] += 1;
  }
  let n = s.len() as f64;
  counts
    .iter()
    .filter(|&&c| c > 0)
    .map(|&c| {
      let p = c as f64 / n;
      -p * p.log2()
    })
    .sum()
}

fn mask(s: &str) -> String {
  let head: String = s.chars().take(6).collect();
  format!("{head}… ({} chars)", s.chars().count())
}

pub fn scan_line(cfg: &Config, path: &str, lineno: usize, line: &str, out: &mut Vec<Finding>) {
  if line.contains("mig:allow") {
    return;
  }
  for rule in rules() {
    for caps in rule.re.captures_iter(line) {
      let m = caps.name("s").unwrap_or_else(|| caps.get(0).unwrap());
      let secret = m.as_str();
      if rule.name == "OpenAI API key" && secret.starts_with("sk-ant-") {
        continue;
      }
      if rule.entropy && (entropy(secret) < 3.5 || out.iter().any(|f| f.path == path && f.line == lineno)) {
        continue;
      }
      if cfg.allow.iter().any(|a| a == secret) {
        continue;
      }
      let f = Finding {
        path: path.to_string(),
        line: lineno,
        rule: rule.name,
        masked: mask(secret),
      };
      if !out.contains(&f) {
        out.push(f);
      }
    }
  }
}

pub fn scan_text(cfg: &Config, path: &str, text: &str, out: &mut Vec<Finding>) {
  if cfg.ignored(path) {
    return;
  }
  for (i, line) in text.lines().enumerate() {
    scan_line(cfg, path, i + 1, line, out);
  }
}

// Only added lines are checked.
pub fn scan_diff(repo: &Repository, diff: &git2::Diff<'_>) -> Result<Vec<Finding>> {
  let cfg = Config::load(repo)?;
  let mut out = Vec::new();
  diff.print(DiffFormat::Patch, |delta, _, line| {
    if line.origin() == '+' {
      let path = delta
        .new_file()
        .path()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
      if !cfg.ignored(&path) {
        let text = String::from_utf8_lossy(line.content());
        scan_line(
          &cfg,
          &path,
          line.new_lineno().unwrap_or(0) as usize,
          text.trim_end(),
          &mut out,
        );
      }
    }
    true
  })?;
  Ok(out)
}

pub fn scan_trees(repo: &Repository, old: Option<&Tree<'_>>, new: &Tree<'_>) -> Result<Vec<Finding>> {
  let diff = repo.diff_tree_to_tree(old, Some(new), None)?;
  scan_diff(repo, &diff)
}

// Uncommitted changes (including untracked files) relative to HEAD.
pub fn scan_pending(repo: &Repository) -> Result<Vec<Finding>> {
  let head = crate::repo::head_commit(repo)?;
  let tree = head.as_ref().map(|c| c.tree()).transpose()?;
  let mut opts = git2::DiffOptions::new();
  opts
    .include_untracked(true)
    .recurse_untracked_dirs(true)
    .show_untracked_content(true);
  let diff = repo.diff_tree_to_workdir_with_index(tree.as_ref(), Some(&mut opts))?;
  scan_diff(repo, &diff)
}

// Every tracked and untracked file currently in the working tree.
pub fn scan_worktree(repo: &Repository) -> Result<Vec<Finding>> {
  let cfg = Config::load(repo)?;
  let Some(root) = repo.workdir() else { return Ok(vec![]) };
  let mut paths: Vec<String> = repo
    .index()?
    .iter()
    .map(|e| String::from_utf8_lossy(&e.path).into_owned())
    .collect();
  for c in crate::repo::changes(repo)? {
    if c.kind == '?' {
      paths.push(c.path);
    }
  }
  paths.sort();
  paths.dedup();
  let mut out = Vec::new();
  for p in paths {
    if let Some(text) = read_text(&root.join(&p)) {
      scan_text(&cfg, &p, &text, &mut out);
    }
  }
  Ok(out)
}

fn read_text(path: &Path) -> Option<String> {
  let meta = std::fs::metadata(path).ok()?;
  if !meta.is_file() || meta.len() > 2 * 1024 * 1024 {
    return None;
  }
  let bytes = std::fs::read(path).ok()?;
  if bytes.contains(&0) {
    return None;
  }
  Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn cfg() -> Config {
    Config {
      ignore: GlobSetBuilder::new().build().unwrap(),
      allow: vec![],
    }
  }

  fn rules_hit(line: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    scan_line(&cfg(), "a.txt", 1, line, &mut out);
    out.into_iter().map(|f| f.rule).collect()
  }

  #[test]
  fn detects_known_keys() {
    assert_eq!(rules_hit("key = AKIAABCDEFGHIJKLMNOP"), vec!["AWS access key"]);
    assert_eq!(
      rules_hit("sk-ant-api03-abcdefghijklmnopqrstuvwxyz"),
      vec!["Anthropic API key"]
    );
    assert_eq!(
      rules_hit("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
      vec!["GitHub token"]
    );
    assert_eq!(rules_hit("-----BEGIN OPENSSH PRIVATE KEY-----"), vec!["Private key"]);
  }

  #[test]
  fn generic_needs_entropy() {
    assert!(rules_hit("password = aaaaaaaaaaaaaaaaaaaa").is_empty());
    assert_eq!(rules_hit("api_key: \"Zx9fQ2pLm7Rt4KwB8nVy\""), vec!["Generic secret"]);
  }

  #[test]
  fn allow_marker() {
    assert!(rules_hit("AKIAABCDEFGHIJKLMNOP # mig:allow").is_empty());
  }
}
