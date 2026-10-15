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

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn detect(dir: &Path) -> (Option<String>, Option<String>) {
  let has = |f: &str| dir.join(f).exists();
  let s = |a: &str, b: &str| (Some(a.to_string()), Some(b.to_string()));
  if has("Cargo.toml") {
    s("cargo check", "cargo run")
  } else if has("package.json") {
    s("npm run build --if-present", "npm start")
  } else if has("go.mod") {
    s("go build ./...", "go run .")
  } else if has("pyproject.toml") || has("main.py") || has_py(dir) {
    // Parse every file instead of compileall, which would leave __pycache__ behind.
    s(
      "python -c \"import ast,pathlib;[ast.parse(p.read_bytes(),str(p)) for p in pathlib.Path('.').rglob('*.py') if not {'.venv','venv','node_modules'} & set(p.parts)]\"",
      "python main.py",
    )
  } else {
    (None, None)
  }
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn has_py(dir: &Path) -> bool {
  std::fs::read_dir(dir)
    .map(|it| it.flatten().any(|e| e.path().extension().is_some_and(|x| x == "py")))
    .unwrap_or(false)
}
