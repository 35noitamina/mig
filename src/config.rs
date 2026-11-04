use std::path::Path;

use anyhow::{Context, Result};
use git2::Repository;
use serde::Deserialize;

pub const CONFIG_FILE: &str = ".mig.toml";

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Config {
  pub scan: ScanSection,
  pub save: SaveSection,
  pub editor: EditorSection,
  pub run: RunSection,
}

// How mig treats commands and scripts that an AI agent asks it to run.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
  // Run everything except what the check blocks.
  #[default]
  Auto,
  // Also ask a person before running anything the check warns about.
  Ask,
  // Ask before anything that is not a routine command.
  Strict,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct RunSection {
  pub mode: RunMode,
  pub timeout_secs: u64,
  pub max_timeout_secs: u64,
  pub output_limit_kb: usize,
  pub approval_wait_secs: u64,
  // Command patterns (`*` matches anything) that are always fine / never allowed.
  pub allow: Vec<String>,
  pub deny: Vec<String>,
  // Rule ids whose "block" is lowered to a warning, for example "remote-package".
  pub allow_rules: Vec<String>,
  // Environment variables to remove (name patterns), and ones to keep although they look secret.
  pub env_drop: Vec<String>,
  pub env_keep: Vec<String>,
}

impl Default for RunSection {
  fn default() -> Self {
    RunSection {
      mode: RunMode::Auto,
      timeout_secs: 120,
      max_timeout_secs: 600,
      output_limit_kb: 60,
      approval_wait_secs: 120,
      allow: Vec::new(),
      deny: Vec::new(),
      allow_rules: Vec::new(),
      env_drop: Vec::new(),
      env_keep: Vec::new(),
    }
  }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ScanSection {
  pub ignore_paths: Vec<String>,
  pub allow: Vec<String>,
}

#[derive(Deserialize)]
#[serde(default)]
pub struct SaveSection {
  pub max_file_mb: f64,
}

impl Default for SaveSection {
  fn default() -> Self {
    SaveSection { max_file_mb: 10.0 }
  }
}

#[derive(Deserialize)]
#[serde(default)]
pub struct EditorSection {
  pub check: Option<String>,
  pub run: Option<String>,
  pub auto_commit_lines: usize,
}

impl Default for EditorSection {
  fn default() -> Self {
    EditorSection {
      check: None,
      run: None,
      auto_commit_lines: 200,
    }
  }
}

pub fn load(repo: &Repository) -> Result<Config> {
  match repo.workdir() {
    Some(dir) => load_dir(dir),
    None => Ok(Config::default()),
  }
}

pub fn load_dir(dir: &Path) -> Result<Config> {
  let p = dir.join(CONFIG_FILE);
  if !p.exists() {
    return Ok(Config::default());
  }
  toml::from_str(&std::fs::read_to_string(&p)?).with_context(|| format!("invalid {CONFIG_FILE}"))
}

// (check, run) commands: explicit config first, then a guess from the project files.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn commands(dir: &Path, cfg: &EditorSection) -> (Option<String>, Option<String>) {
  let (check, run) = detect(dir);
  (cfg.check.clone().or(check), cfg.run.clone().or(run))
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
  Rust,
  Node,
  Go,
  Python,
}

impl Kind {
  pub fn name(self) -> &'static str {
    match self {
      Kind::Rust => "Rust",
      Kind::Node => "Node.js",
      Kind::Go => "Go",
      Kind::Python => "Python",
    }
  }
}

pub fn project_kind(dir: &Path) -> Option<Kind> {
  let has = |f: &str| dir.join(f).exists();
  if has("Cargo.toml") {
    Some(Kind::Rust)
  } else if has("package.json") {
    Some(Kind::Node)
  } else if has("go.mod") {
    Some(Kind::Go)
  } else if has("pyproject.toml") || has("main.py") || has_py(dir) {
    Some(Kind::Python)
  } else {
    None
  }
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn detect(dir: &Path) -> (Option<String>, Option<String>) {
  let s = |a: &str, b: &str| (Some(a.to_string()), Some(b.to_string()));
  match project_kind(dir) {
    Some(Kind::Rust) => s("cargo check", "cargo run"),
    Some(Kind::Node) => s("npm run build --if-present", "npm start"),
    Some(Kind::Go) => s("go build ./...", "go run ."),
    // Parse every file instead of compileall, which would leave __pycache__ behind.
    Some(Kind::Python) => s(
      "python -c \"import ast,pathlib;[ast.parse(p.read_bytes(),str(p)) for p in pathlib.Path('.').rglob('*.py') if not {'.venv','venv','node_modules'} & set(p.parts)]\"",
      "python main.py",
    ),
    None => (None, None),
  }
}

fn has_py(dir: &Path) -> bool {
  std::fs::read_dir(dir)
    .map(|it| it.flatten().any(|e| e.path().extension().is_some_and(|x| x == "py")))
    .unwrap_or(false)
}
