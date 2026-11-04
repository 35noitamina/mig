// Notes between people and agents. A person writes to an agent from the GUI (or `mig tell`); the
// MCP server of that agent picks the note up and puts it in front of the AI. An agent can leave a
// note for the person with the `say` tool; the GUI shows it. Both are small files under .git/mig,
// which agents cannot write with their file tools.
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

const KEEP_SECS: i64 = 24 * 3600;
const MAX_TEXT: usize = 8000;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Message {
  pub id: String,
  // Unix seconds.
  pub ts: i64,
  // An agent's name, or "*" for whichever agent is listening.
  pub to: String,
  pub text: String,
  pub file: Option<String>,
  pub lines: Option<(usize, usize)>,
  // What the person had selected, when it was sent from the editor.
  pub selection: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Say {
  pub id: String,
  pub ts: i64,
  pub agent: Option<String>,
  pub text: String,
  // "info" or "attention"
  pub level: String,
}

fn dir(git_dir: &Path, what: &str) -> PathBuf {
  git_dir.join("mig").join(what)
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
  if let Some(d) = path.parent() {
    std::fs::create_dir_all(d)?;
  }
  let tmp = path.with_extension("tmp");
  std::fs::write(&tmp, text)?;
  std::fs::rename(tmp, path)?;
  Ok(())
}

fn clip(s: &str) -> String {
  s.chars().take(MAX_TEXT).collect()
}

fn new_id() -> String {
  crate::approval::new_id()
}

pub fn send(
  git_dir: &Path,
  to: &str,
  text: &str,
  file: Option<String>,
  lines: Option<(usize, usize)>,
  selection: Option<String>,
) -> Result<()> {
  let m = Message {
    id: new_id(),
    ts: chrono::Utc::now().timestamp(),
    to: to.to_string(),
    text: clip(text),
    file,
    lines,
    selection: selection.map(|s| clip(&s)),
  };
  let name = format!("{}-{}.json", crate::live::now_ms(), m.id);
  write_atomic(&dir(git_dir, "inbox").join(name), &serde_json::to_string_pretty(&m)?)
}

// Takes the notes for `me` (and the ones for "*"). A note is taken by exactly one listener.
pub fn claim(git_dir: &Path, me: Option<&str>) -> Vec<Message> {
  let d = dir(git_dir, "inbox");
  let Ok(rd) = std::fs::read_dir(&d) else {
    return Vec::new();
  };
  let now = chrono::Utc::now().timestamp();
  let mut files: Vec<PathBuf> = rd
    .flatten()
    .map(|e| e.path())
    .filter(|p| p.extension().is_some_and(|x| x == "json"))
    .collect();
  files.sort();
  let mut out = Vec::new();
  for f in files {
    let Some(m) = std::fs::read_to_string(&f)
      .ok()
      .and_then(|t| serde_json::from_str::<Message>(&t).ok())
    else {
      continue;
    };
    if now - m.ts > KEEP_SECS {
      let _ = std::fs::remove_file(&f);
      continue;
    }
    if m.to != "*" && Some(m.to.as_str()) != me {
      continue;
    }
    // Renaming is atomic: whoever succeeds owns the note.
    let mine = f.with_extension(format!("taken-{}", std::process::id()));
    if std::fs::rename(&f, &mine).is_ok() {
      let _ = std::fs::remove_file(&mine);
      out.push(m);
    }
  }
  out
}

pub fn say(git_dir: &Path, agent: Option<&str>, text: &str, level: &str) -> Result<()> {
  let s = Say {
    id: new_id(),
    ts: chrono::Utc::now().timestamp(),
    agent: agent.map(str::to_string),
    text: clip(text),
    level: if level == "attention" { "attention" } else { "info" }.into(),
  };
  let name = format!("{}-{}.json", crate::live::now_ms(), s.id);
  write_atomic(&dir(git_dir, "outbox").join(name), &serde_json::to_string_pretty(&s)?)
}

// Notes agents left for the person, oldest first. Old ones are removed.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn says(git_dir: &Path) -> Vec<Say> {
  let d = dir(git_dir, "outbox");
  let Ok(rd) = std::fs::read_dir(&d) else {
    return Vec::new();
  };
  let now = chrono::Utc::now().timestamp();
  let mut files: Vec<PathBuf> = rd
    .flatten()
    .map(|e| e.path())
    .filter(|p| p.extension().is_some_and(|x| x == "json"))
    .collect();
  // The file names start with the time in milliseconds.
  files.sort();
  let mut out: Vec<Say> = Vec::new();
  for p in files {
    let Some(s) = std::fs::read_to_string(&p)
      .ok()
      .and_then(|t| serde_json::from_str::<Say>(&t).ok())
    else {
      continue;
    };
    if now - s.ts > KEEP_SECS {
      let _ = std::fs::remove_file(&p);
    } else {
      out.push(s);
    }
  }
  let drop = out.len().saturating_sub(100);
  out.drain(..drop);
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  fn tmp() -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!(
      "mig-inbox-{}-{}-{n}",
      std::process::id(),
      crate::live::now_ms()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
  }

  #[test]
  fn notes_reach_the_right_listener_once() {
    let g = tmp();
    send(&g, "alice", "for alice", None, None, None).unwrap();
    send(
      &g,
      "bob",
      "for bob",
      Some("src/a.rs".into()),
      Some((3, 5)),
      Some("code".into()),
    )
    .unwrap();
    send(&g, "*", "for anyone", None, None, None).unwrap();
    let a = claim(&g, Some("alice"));
    assert_eq!(
      a.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
      vec!["for alice", "for anyone"]
    );
    assert!(claim(&g, Some("alice")).is_empty());
    let b = claim(&g, Some("bob"));
    assert_eq!(b.len(), 1);
    assert_eq!(
      (b[0].file.as_deref(), b[0].lines, b[0].selection.as_deref()),
      (Some("src/a.rs"), Some((3, 5)), Some("code"))
    );
    let _ = std::fs::remove_dir_all(g);
  }

  #[test]
  fn an_unnamed_listener_only_gets_notes_for_anyone() {
    let g = tmp();
    send(&g, "alice", "x", None, None, None).unwrap();
    send(&g, "*", "y", None, None, None).unwrap();
    assert_eq!(claim(&g, None).len(), 1);
    assert_eq!(claim(&g, Some("alice")).len(), 1);
    let _ = std::fs::remove_dir_all(g);
  }

  #[test]
  fn says_are_kept_in_order() {
    let g = tmp();
    say(&g, Some("alice"), "first", "info").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    say(&g, None, "second", "attention").unwrap();
    let s = says(&g);
    assert_eq!(
      s.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(),
      vec!["first", "second"]
    );
    assert_eq!(s[1].level, "attention");
    let _ = std::fs::remove_dir_all(g);
  }
}
