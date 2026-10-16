// Folders open in the GUI window. The MCP server may only work in these (and in the folder it was
// started in), so this list doubles as the allow-list for AI tools.
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::repo;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Workspace {
  pub name: String,
  pub path: PathBuf,
}

fn file() -> Option<PathBuf> {
  dirs::config_dir().map(|d| d.join("mig").join("workspaces.json"))
}

pub fn load() -> Vec<Workspace> {
  file()
    .and_then(|f| std::fs::read_to_string(f).ok())
    .and_then(|s| serde_json::from_str(&s).ok())
    .unwrap_or_default()
}

fn save(list: &[Workspace]) -> Result<()> {
  let f = file().ok_or_else(|| anyhow!("no configuration folder on this system"))?;
  if let Some(dir) = f.parent() {
    std::fs::create_dir_all(dir)?;
  }
  let tmp = f.with_extension("json.tmp");
  std::fs::write(&tmp, serde_json::to_string_pretty(list)?)?;
  std::fs::rename(tmp, f)?;
  Ok(())
}

pub fn same(a: &Path, b: &Path) -> bool {
  match (a.canonicalize(), b.canonicalize()) {
    (Ok(x), Ok(y)) => x == y,
    _ => a == b,
  }
}

fn folder_name(p: &Path) -> String {
  p.file_name()
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_else(|| "repo".into())
}

fn unique(list: &[Workspace], base: &str) -> String {
  let mut name = base.to_string();
  let mut n = 2;
  while list.iter().any(|w| w.name == name) {
    name = format!("{base}-{n}");
    n += 1;
  }
  name
}

// By name, or by a path inside the repository.
pub fn find<'a>(list: &'a [Workspace], key: &str) -> Option<&'a Workspace> {
  list.iter().find(|w| w.name == key).or_else(|| {
    let p = Path::new(key);
    list.iter().find(|w| {
      same(&w.path, p)
        || p
          .canonicalize()
          .is_ok_and(|c| w.path.canonicalize().is_ok_and(|r| c.starts_with(r)))
    })
  })
}

// Adds the repository containing `path`; returns the existing entry when it is already listed.
pub fn add(path: &Path, name: Option<&str>) -> Result<Workspace> {
  let r = repo::open(path)?;
  let root = r
    .workdir()
    .ok_or_else(|| anyhow!("{} has no working folder", path.display()))?
    .to_path_buf();
  let root = root.canonicalize().unwrap_or(root);
  let mut list = load();
  if let Some(w) = list.iter().find(|w| same(&w.path, &root)) {
    return Ok(w.clone());
  }
  let wanted = name.map_or_else(|| folder_name(&root), str::to_string);
  if name.is_some() && list.iter().any(|w| w.name == wanted) {
    bail!("the name {wanted} is already used");
  }
  let w = Workspace {
    name: unique(&list, &wanted),
    path: root,
  };
  list.push(w.clone());
  save(&list)?;
  Ok(w)
}

pub fn remove(key: &str) -> Result<Workspace> {
  let mut list = load();
  let w = find(&list, key)
    .cloned()
    .ok_or_else(|| anyhow!("no folder named {key}"))?;
  list.retain(|x| x != &w);
  save(&list)?;
  Ok(w)
}

pub fn rename(key: &str, new: &str) -> Result<()> {
  let mut list = load();
  if list.iter().any(|w| w.name == new) {
    bail!("the name {new} is already used");
  }
  let w = find(&list, key)
    .cloned()
    .ok_or_else(|| anyhow!("no folder named {key}"))?;
  for x in &mut list {
    if *x == w {
      x.name = new.to_string();
    }
  }
  save(&list)
}

pub fn names(list: &[Workspace]) -> String {
  list.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(", ")
}

// The repository an AI tool call should use: `key` (a name or a path) when given, otherwise the
// folder the server was started in, otherwise the only open folder.
pub fn pick(launch: Option<&Path>, key: Option<&str>) -> Result<PathBuf> {
  let list = load();
  match key {
    Some(k) => {
      if let Some(w) = find(&list, k) {
        return Ok(w.path.clone());
      }
      if let Some(l) = launch
        && (k == folder_name(l) || same(l, Path::new(k)))
      {
        return Ok(l.to_path_buf());
      }
      bail!(
        "{k} is not an open folder; use one of: {}",
        names(&list).if_empty_then("(none; add folders in the GUI or with `mig ws add`)")
      )
    }
    None => match (launch, list.as_slice()) {
      (Some(l), _) => Ok(l.to_path_buf()),
      (None, [only]) => Ok(only.path.clone()),
      (None, []) => bail!("no repository: start the server inside one, or add folders in the GUI or with `mig ws add`"),
      (None, _) => bail!("several folders are open; pass repo, one of: {}", names(&list)),
    },
  }
}

trait IfEmpty {
  fn if_empty_then(self, other: &str) -> String;
}

impl IfEmpty for String {
  fn if_empty_then(self, other: &str) -> String {
    if self.is_empty() { other.to_string() } else { self }
  }
}
