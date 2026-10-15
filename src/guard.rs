use std::fmt;

use anyhow::Result;
use git2::{Delta, Diff, Repository};

use crate::scan::Finding;

#[derive(Debug, Default)]
pub struct SaveBlocked {
  pub secrets: Vec<Finding>,
  pub large: Vec<(String, u64)>,
}

impl fmt::Display for SaveBlocked {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut parts = Vec::new();
    if !self.secrets.is_empty() {
      parts.push(format!("{} possible secret(s)", self.secrets.len()));
    }
    if !self.large.is_empty() {
      parts.push(format!("{} large file(s)", self.large.len()));
    }
    write!(f, "save blocked: {}", parts.join(", "))
  }
}

impl std::error::Error for SaveBlocked {}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Eol {
  None,
  Lf,
  Crlf,
  Mixed,
}

fn eol(data: &[u8]) -> Eol {
  let crlf = data.windows(2).filter(|w| w == b"\r\n").count();
  let lf = data.iter().filter(|&&b| b == b'\n').count() - crlf;
  match (lf, crlf) {
    (0, 0) => Eol::None,
    (_, 0) => Eol::Lf,
    (0, _) => Eol::Crlf,
    _ => Eol::Mixed,
  }
}

fn name(e: Eol) -> &'static str {
  match e {
    Eol::Crlf => "CRLF",
    _ => "LF",
  }
}

pub fn fmt_size(n: u64) -> String {
  const MB: f64 = 1024.0 * 1024.0;
  if n as f64 >= MB {
    format!("{:.1} MB", n as f64 / MB)
  } else if n >= 1024 {
    format!("{:.1} KB", n as f64 / 1024.0)
  } else {
    format!("{n} B")
  }
}

// Large files block a save; line ending problems are only reported.
pub fn inspect(repo: &Repository, diff: &Diff<'_>, max_bytes: u64) -> Result<(Vec<(String, u64)>, Vec<String>)> {
  let mut large = Vec::new();
  let mut warnings = Vec::new();
  for d in diff.deltas() {
    if !matches!(
      d.status(),
      Delta::Added | Delta::Modified | Delta::Renamed | Delta::Copied
    ) {
      continue;
    }
    let path = d
      .new_file()
      .path()
      .map(|p| p.to_string_lossy().into_owned())
      .unwrap_or_default();
    let Ok(new) = repo.find_blob(d.new_file().id()) else {
      continue;
    };
    let size = new.size() as u64;
    if size > max_bytes {
      large.push((path, size));
      continue;
    }
    if new.is_binary() {
      continue;
    }
    let now = eol(new.content());
    if now == Eol::Mixed {
      warnings.push(format!("{path}: mixed CRLF and LF line endings"));
      continue;
    }
    if d.status() == Delta::Modified
      && let Ok(old) = repo.find_blob(d.old_file().id())
    {
      let before = eol(old.content());
      if matches!(before, Eol::Lf | Eol::Crlf) && matches!(now, Eol::Lf | Eol::Crlf) && before != now {
        warnings.push(format!(
          "{path}: line endings changed from {} to {} (every line will show as changed)",
          name(before),
          name(now)
        ));
      }
    }
  }
  Ok((large, warnings))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn detects_eol() {
    assert_eq!(eol(b"a\nb\n"), Eol::Lf);
    assert_eq!(eol(b"a\r\nb\r\n"), Eol::Crlf);
    assert_eq!(eol(b"a\r\nb\n"), Eol::Mixed);
    assert_eq!(eol(b"ab"), Eol::None);
  }
}
