// Text search for the editor: a query that works in one file (Ctrl+F) and across the repository
// (Ctrl+Shift+F). The repository search runs on a worker thread and honours .gitignore.
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};

use globset::Glob;
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};

const MAX_FILE: u64 = 2 << 20;
pub const MAX_HITS: usize = 2000;
const MAX_IN_FILE: usize = 100_000;
const SNIPPET: usize = 200;

#[derive(Clone, PartialEq, Default)]
pub struct Query {
  pub text: String,
  pub case: bool,
  pub regex: bool,
}

impl Query {
  // None for an empty query; Err when the regular expression is invalid.
  pub fn compile(&self) -> Result<Option<Regex>, String> {
    if self.text.is_empty() {
      return Ok(None);
    }
    let pattern = if self.regex {
      self.text.clone()
    } else {
      regex::escape(&self.text)
    };
    RegexBuilder::new(&pattern)
      .case_insensitive(!self.case)
      .multi_line(true)
      .build()
      .map(Some)
      .map_err(|e| {
        let msg = e.to_string();
        msg.lines().last().unwrap_or("invalid pattern").trim().to_string()
      })
  }
}

// Byte ranges of every non-empty match.
pub fn find_all(text: &str, re: &Regex) -> Vec<Range<usize>> {
  re.find_iter(text)
    .filter(|m| !m.is_empty())
    .take(MAX_IN_FILE)
    .map(|m| m.range())
    .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
  pub path: String,
  // 1-based.
  pub line: usize,
  // The line without leading blanks, cut to a readable length.
  pub snippet: String,
  // Where the match sits inside `snippet`.
  pub span: Range<usize>,
  // Byte range of the match inside the whole file.
  pub at: Range<usize>,
}

pub enum Msg {
  Hits(Vec<Hit>),
  Done { files: usize, capped: bool },
}

pub struct Search {
  pub rx: Receiver<Msg>,
  cancel: Arc<AtomicBool>,
}

impl Drop for Search {
  fn drop(&mut self) {
    self.cancel.store(true, Ordering::Relaxed);
  }
}

fn snippet_of(line: &str, from: usize, to: usize) -> (String, Range<usize>) {
  let lead = line.len() - line.trim_start().len();
  let trimmed = line.trim_end();
  let (from, to) = (
    from.saturating_sub(lead),
    to.saturating_sub(lead).min(trimmed.len() - lead),
  );
  let body = &trimmed[lead..];
  if body.chars().count() <= SNIPPET {
    return (body.to_string(), from..to);
  }
  // Keep the match in view: start a little before it.
  let start_char = body[..from].chars().count().saturating_sub(40);
  let start = body.char_indices().nth(start_char).map_or(0, |(i, _)| i);
  let cut: String = body[start..].chars().take(SNIPPET).collect();
  let prefix = if start > 0 { "…" } else { "" };
  let shift = prefix.len();
  let a = (from - start + shift).min(prefix.len() + cut.len());
  let b = (to - start + shift).min(prefix.len() + cut.len());
  (format!("{prefix}{cut}"), a..b.max(a))
}

pub fn hits_in(path: &str, text: &str, re: &Regex, out: &mut Vec<Hit>, room: usize) {
  let mut line_start = 0;
  let mut taken = 0;
  for (i, line) in text.split('\n').enumerate() {
    let content = line.strip_suffix('\r').unwrap_or(line);
    for m in re.find_iter(content).filter(|m| !m.is_empty()) {
      if taken >= room {
        return;
      }
      let (snippet, span) = snippet_of(content, m.start(), m.end());
      out.push(Hit {
        path: path.to_string(),
        line: i + 1,
        snippet,
        span,
        at: line_start + m.start()..line_start + m.end(),
      });
      taken += 1;
    }
    line_start += line.len() + 1;
  }
}

fn is_binary(bytes: &[u8]) -> bool {
  bytes[..bytes.len().min(8000)].contains(&0)
}

// Searches the repository under `root`. `open` holds the text of files open in the editor, which
// takes the place of what is on disk (it may have unsaved edits).
pub fn search(root: &Path, q: &Query, glob: &str, open: HashMap<String, String>) -> Result<Search, String> {
  let re = q.compile()?.ok_or("empty query")?;
  let matcher = match glob.trim() {
    "" => None,
    g => Some(Glob::new(g).map_err(|e| e.to_string())?.compile_matcher()),
  };
  let (tx, rx) = mpsc::channel();
  let cancel = Arc::new(AtomicBool::new(false));
  let flag = cancel.clone();
  let root = root.to_path_buf();
  std::thread::spawn(move || {
    let walk = WalkBuilder::new(&root)
      .hidden(false)
      .filter_entry(|e| e.file_name() != ".git")
      .sort_by_file_path(|a, b| a.cmp(b))
      .build();
    let (mut total, mut files, mut capped) = (0, 0, false);
    for entry in walk.flatten() {
      if flag.load(Ordering::Relaxed) {
        return;
      }
      if !entry.file_type().is_some_and(|t| t.is_file()) {
        continue;
      }
      let rel: String = entry
        .path()
        .strip_prefix(&root)
        .unwrap_or(entry.path())
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
      if matcher.as_ref().is_some_and(|g| !g.is_match(&rel)) {
        continue;
      }
      let text = match open.get(&rel) {
        Some(t) => t.clone(),
        None => {
          if entry.metadata().map_or(true, |m| m.len() > MAX_FILE) {
            continue;
          }
          let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
          };
          if is_binary(&bytes) {
            continue;
          }
          String::from_utf8_lossy(&bytes).into_owned()
        }
      };
      let mut hits = Vec::new();
      hits_in(&rel, &text, &re, &mut hits, MAX_HITS - total);
      if hits.is_empty() {
        continue;
      }
      total += hits.len();
      files += 1;
      if tx.send(Msg::Hits(hits)).is_err() {
        return;
      }
      if total >= MAX_HITS {
        capped = true;
        break;
      }
    }
    let _ = tx.send(Msg::Done { files, capped });
  });
  Ok(Search { rx, cancel })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn q(text: &str, case: bool, regex: bool) -> Query {
    Query {
      text: text.into(),
      case,
      regex,
    }
  }

  #[test]
  fn plain_text_is_escaped_and_case_follows_the_flag() {
    let re = q("a.b", false, false).compile().unwrap().unwrap();
    assert_eq!(find_all("A.B axb a.b", &re), vec![0..3, 8..11]);
    let re = q("a.b", true, false).compile().unwrap().unwrap();
    assert_eq!(find_all("A.B axb a.b", &re), vec![8..11]);
  }

  #[test]
  fn regex_and_errors() {
    let re = q("fo+", true, true).compile().unwrap().unwrap();
    assert_eq!(find_all("foo fo f", &re), vec![0..3, 4..6]);
    assert!(q("(", true, true).compile().is_err());
    assert!(q("", true, true).compile().unwrap().is_none());
    // An empty match never counts.
    let re = q("x*", true, true).compile().unwrap().unwrap();
    assert!(find_all("abc", &re).is_empty());
  }

  #[test]
  fn hits_carry_line_snippet_and_file_offset() {
    let re = q("beta", false, false).compile().unwrap().unwrap();
    let mut out = Vec::new();
    hits_in("f.txt", "alpha\n    beta gamma\r\nbeta", &re, &mut out, 10);
    assert_eq!(out.len(), 2);
    assert_eq!(
      (out[0].line, out[0].snippet.as_str(), out[0].span.clone()),
      (2, "beta gamma", 0..4)
    );
    assert_eq!(out[0].at, 10..14);
    assert_eq!((out[1].line, out[1].at.clone()), (3, 22..26));
  }

  #[test]
  fn long_lines_are_cut_around_the_match() {
    let line = format!("{}needle{}", "x".repeat(300), "y".repeat(300));
    let re = q("needle", true, false).compile().unwrap().unwrap();
    let mut out = Vec::new();
    hits_in("f", &line, &re, &mut out, 10);
    let h = &out[0];
    assert_eq!(&h.snippet[h.span.clone()], "needle");
    assert!(h.snippet.chars().count() <= SNIPPET + 1);
  }

  #[test]
  fn repository_search_skips_ignored_and_prefers_open_buffers() {
    let dir = std::env::temp_dir().join(format!("mig-find-{}-{}", std::process::id(), crate::live::now_ms()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    git2::Repository::init(&dir).unwrap();
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(dir.join("src/a.rs"), "let needle = 1;\n").unwrap();
    std::fs::write(dir.join("src/b.txt"), "needle on disk\n").unwrap();
    std::fs::write(dir.join("target/c.rs"), "needle ignored\n").unwrap();
    std::fs::write(dir.join("bin.dat"), b"needle\0\0").unwrap();
    let open = HashMap::from([("src/b.txt".to_string(), "unsaved needle needle\n".to_string())]);
    let s = search(&dir, &q("needle", true, false), "", open.clone()).unwrap();
    let mut hits = Vec::new();
    while let Ok(m) = s.rx.recv() {
      match m {
        Msg::Hits(h) => hits.extend(h),
        Msg::Done { .. } => break,
      }
    }
    let got: Vec<(String, usize)> = hits.iter().map(|h| (h.path.clone(), h.line)).collect();
    assert_eq!(
      got,
      vec![("src/a.rs".into(), 1), ("src/b.txt".into(), 1), ("src/b.txt".into(), 1)]
    );
    let s = search(&dir, &q("needle", true, false), "*.rs", open).unwrap();
    let mut n = 0;
    while let Ok(m) = s.rx.recv() {
      match m {
        Msg::Hits(h) => n += h.len(),
        Msg::Done { .. } => break,
      }
    }
    assert_eq!(n, 1);
    let _ = std::fs::remove_dir_all(dir);
  }
}
