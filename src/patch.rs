use anyhow::Result;
use git2::build::TreeUpdateBuilder;
use git2::{Diff, FileMode, Oid, Patch, Repository, Tree};

pub struct Hunk {
  // 1-based; for pure insertions (old_lines == 0) this is the line the insertion follows.
  pub old_start: usize,
  pub old_lines: usize,
  pub new_lines: Vec<Vec<u8>>,
  pub text: String,
}

pub enum Body {
  Text { old: Vec<u8>, hunks: Vec<Hunk> },
  // Added, deleted or binary files are handled as a single unit.
  Whole { text: String },
}

pub struct FileChange {
  pub path: String,
  pub new: Option<(Oid, FileMode)>,
  pub body: Body,
}

pub fn collect(repo: &Repository, diff: &Diff<'_>) -> Result<Vec<FileChange>> {
  let mut out = Vec::new();
  for i in 0..diff.deltas().len() {
    let d = diff.get_delta(i).unwrap();
    let path = d
      .new_file()
      .path()
      .or(d.old_file().path())
      .map(|p| p.to_string_lossy().replace('\\', "/"))
      .unwrap_or_default();
    let new = d.new_file().exists().then(|| (d.new_file().id(), d.new_file().mode()));
    let patch = Patch::from_diff(diff, i)?;
    let text_patch = patch
      .as_ref()
      .filter(|_| d.status() == git2::Delta::Modified && !d.flags().is_binary());
    let body = match text_patch {
      Some(p) => {
        let old = repo.find_blob(d.old_file().id())?.content().to_vec();
        let mut hunks = Vec::new();
        for h in 0..p.num_hunks() {
          let (hunk, n) = p.hunk(h)?;
          let mut text = String::from_utf8_lossy(hunk.header()).into_owned();
          let mut new_lines = Vec::new();
          for j in 0..n {
            let line = p.line_in_hunk(h, j)?;
            let origin = line.origin();
            if matches!(origin, ' ' | '+') {
              new_lines.push(line.content().to_vec());
            }
            if matches!(origin, ' ' | '+' | '-') {
              text.push(origin);
              text.push_str(&String::from_utf8_lossy(line.content()));
              if !text.ends_with('\n') {
                text.push('\n');
              }
            }
          }
          hunks.push(Hunk {
            old_start: hunk.old_start() as usize,
            old_lines: hunk.old_lines() as usize,
            new_lines,
            text,
          });
        }
        Body::Text { old, hunks }
      }
      None => {
        let what = match d.status() {
          git2::Delta::Added | git2::Delta::Untracked => "new file",
          git2::Delta::Deleted => "deleted",
          _ => "binary change",
        };
        Body::Whole {
          text: format!("{what}: {path}\n"),
        }
      }
    };
    out.push(FileChange { path, new, body });
  }
  Ok(out)
}

pub fn apply_hunks(old: &[u8], hunks: &[&Hunk]) -> Vec<u8> {
  let lines: Vec<&[u8]> = old.split_inclusive(|&b| b == b'\n').collect();
  let mut sorted: Vec<&&Hunk> = hunks.iter().collect();
  sorted.sort_by_key(|h| h.old_start);
  let mut out = Vec::with_capacity(old.len());
  let mut pos = 0;
  for h in sorted {
    let start = if h.old_lines == 0 { h.old_start } else { h.old_start - 1 };
    for l in &lines[pos..start] {
      out.extend_from_slice(l);
    }
    for l in &h.new_lines {
      out.extend_from_slice(l);
    }
    pos = start + h.old_lines;
  }
  for l in &lines[pos.min(lines.len())..] {
    out.extend_from_slice(l);
  }
  out
}

pub enum Edit {
  Write(String, Vec<u8>, FileMode),
  Set(String, Option<(Oid, FileMode)>),
}

pub fn build_tree(repo: &Repository, base: &Tree<'_>, edits: Vec<Edit>) -> Result<Oid> {
  let mut b = TreeUpdateBuilder::new();
  for e in edits {
    match e {
      Edit::Write(path, data, mode) => {
        let id = repo.blob(&data)?;
        b.upsert(path, id, mode);
      }
      Edit::Set(path, Some((id, mode))) => {
        b.upsert(path, id, mode);
      }
      Edit::Set(path, None) => {
        b.remove(path);
      }
    }
  }
  Ok(b.create_updated(repo, base)?)
}

pub fn text_mode(fc: &FileChange) -> FileMode {
  fc.new.map(|(_, m)| m).unwrap_or(FileMode::Blob)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn hunk(old_start: usize, old_lines: usize, new: &[&str]) -> Hunk {
    Hunk {
      old_start,
      old_lines,
      new_lines: new.iter().map(|s| s.as_bytes().to_vec()).collect(),
      text: String::new(),
    }
  }

  #[test]
  fn applies_replace_insert_delete() {
    let old = b"a\nb\nc\nd\n";
    let r = hunk(2, 1, &["B\n"]);
    let i = hunk(3, 0, &["x\n"]);
    let d = hunk(4, 1, &[]);
    assert_eq!(apply_hunks(old, &[&r, &i, &d]), b"a\nB\nc\nx\n");
    assert_eq!(apply_hunks(old, &[&i]), b"a\nb\nc\nx\nd\n");
    assert_eq!(apply_hunks(old, &[&hunk(0, 0, &["top\n"])]), b"top\na\nb\nc\nd\n");
  }
}
