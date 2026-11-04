// What the Live view shows besides file writes: runs starting and ending, runs waiting for a person,
// and the events agents hear about (the Monitor). They are read from the files mig keeps under
// .git/mig, so they show up whichever process caused them.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::{approval, events, outlog};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Src {
  Run,
  Approval,
  Monitor,
}

impl Src {
  pub fn label(self) -> &'static str {
    match self {
      Src::Run => "run",
      Src::Approval => "approval",
      Src::Monitor => "monitor",
    }
  }
}

#[derive(Clone, Debug)]
pub struct Act {
  pub src: Src,
  pub title: String,
  pub who: Option<String>,
  pub text: String,
  // The stored output of a run, so the detail view can show it as it grows.
  pub run: Option<String>,
}

#[derive(Default)]
pub struct Tracker {
  inited: HashSet<PathBuf>,
  runs: HashMap<(PathBuf, String), bool>,
  approvals: HashSet<(PathBuf, String)>,
  events: HashSet<(PathBuf, u32, u64)>,
}

fn first_line(s: &str, n: usize) -> String {
  // The first two lines that say something, so "job abc:\n<output>" does not show only the header.
  let joined: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).take(2).collect();
  let joined = joined.join(" · ");
  let l = joined.as_str();
  let mut out: String = l.chars().take(n).collect();
  if l.chars().count() > n {
    out.push('…');
  }
  out
}

impl Tracker {
  // What is new in `root` since the last call. The first call only takes note of what is already there.
  pub fn poll(&mut self, root: &Path) -> Vec<Act> {
    let git = root.join(".git");
    let first = self.inited.insert(root.to_path_buf());
    let mut out = Vec::new();

    for m in outlog::list(&git) {
      let key = (root.to_path_buf(), m.id.clone());
      let done_seen = self.runs.get(&key).copied();
      let running = m.running();
      let who = m.agent.clone();
      match (done_seen, running) {
        (None, true) => {
          self.runs.insert(key, false);
          if !first {
            out.push(Act {
              src: Src::Run,
              title: format!("▶ {}", first_line(&m.summary, 90)),
              who,
              text: m.summary.clone(),
              run: Some(m.id.clone()),
            });
          }
        }
        (None | Some(false), false) => {
          self.runs.insert(key, true);
          if !first {
            let mark = if m.exit == Some(0) && !m.timed_out && !m.cancelled {
              "✓"
            } else {
              "×"
            };
            out.push(Act {
              src: Src::Run,
              title: format!(
                "{mark} {}  ({}, {:.1}s)",
                first_line(&m.summary, 80),
                m.headline(),
                m.secs
              ),
              who,
              text: m.summary.clone(),
              run: Some(m.id.clone()),
            });
          }
        }
        _ => {}
      }
    }

    for p in approval::pending(&git) {
      if self.approvals.insert((root.to_path_buf(), p.id.clone())) && !first {
        out.push(Act {
          src: Src::Approval,
          title: format!("needs approval: {}", first_line(&p.code, 90)),
          who: p.agent.clone(),
          text: {
            let notes: Vec<String> = p.notes.iter().map(|n| format!("- {} [{}]", n.what, n.rule)).collect();
            format!("{}\n\n{}", p.code, notes.join("\n"))
          },
          run: None,
        });
      }
    }

    let log = events::read_log(&git, 300);
    let seen_now: HashSet<(PathBuf, u32, u64)> = log.iter().map(|e| (root.to_path_buf(), e.pid, e.id)).collect();
    for e in &log {
      let key = (root.to_path_buf(), e.pid, e.id);
      // The output of a run is already shown as the run itself.
      if !self.events.contains(&key) && !first && e.kind != "job" {
        out.push(Act {
          src: Src::Monitor,
          title: format!("{}: {}", e.kind, first_line(&e.text, 100)),
          who: e.actor.clone().or_else(|| e.by.clone()),
          text: e.text.clone(),
          run: None,
        });
      }
    }
    self.events.retain(|k| k.0 != root);
    self.events.extend(seen_now);
    // Keep the maps from growing for ever.
    if self.runs.len() > 500 {
      self.runs.clear();
    }
    out
  }
}
