// The "Agents" page: every agent session in every open folder, with what each one wrote and
// buttons to finish (commit) or abort (undo) its work.
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::{agent, repo};

const REFRESH: Duration = Duration::from_secs(2);

#[derive(PartialEq)]
enum FileState {
  // Still what the agent wrote.
  Yours,
  // Someone changed it after the agent.
  Changed,
}

struct Row {
  root: PathBuf,
  folder: String,
  session: agent::Session,
  files: Vec<(String, FileState)>,
}

struct FinishDialog {
  root: PathBuf,
  id: String,
  name: String,
  message: String,
}

pub enum Action {
  // Show this folder's Live tab, filtered to the agent.
  Live { root: PathBuf, agent: String },
  Changed,
}

#[derive(Default)]
pub struct AgentsView {
  rows: Vec<Row>,
  loaded: Option<Instant>,
  open: HashSet<String>,
  finish: Option<FinishDialog>,
  abort: Option<(PathBuf, String, String)>,
  status: Option<Result<String, String>>,
}

fn time(t: i64) -> String {
  chrono::DateTime::from_timestamp(t, 0)
    .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
    .unwrap_or_default()
}

fn load(folders: &[(PathBuf, String)]) -> Vec<Row> {
  let mut rows = Vec::new();
  for (root, folder) in folders {
    let Ok(r) = repo::open(root) else { continue };
    for session in agent::list(&r) {
      let files = session
        .files
        .iter()
        .map(|(path, t)| {
          let now = std::fs::read(root.join(path)).ok().map(|b| agent::hash(&b));
          let state = if now == t.now {
            FileState::Yours
          } else {
            FileState::Changed
          };
          (path.clone(), state)
        })
        .collect();
      rows.push(Row {
        root: root.clone(),
        folder: folder.clone(),
        session,
        files,
      });
    }
  }
  rows
}

impl AgentsView {
  pub fn count(&self) -> usize {
    self.rows.len()
  }

  pub fn count_in(&self, root: &std::path::Path) -> usize {
    self.rows.iter().filter(|r| r.root == root).count()
  }

  pub fn refresh(&mut self, folders: &[(PathBuf, String)], force: bool) {
    if force || self.loaded.is_none_or(|t| t.elapsed() >= REFRESH) {
      self.rows = load(folders);
      self.loaded = Some(Instant::now());
    }
  }

  pub fn ui(&mut self, ui: &mut egui::Ui, folders: &[(PathBuf, String)]) -> Option<Action> {
    self.refresh(folders, false);
    ui.ctx().request_repaint_after(REFRESH);
    let mut action = None;
    egui::CentralPanel::default_margins().show(ui, |ui| {
      ui.heading("Agents");
      match &self.status {
        Some(Ok(s)) => {
          ui.label(s);
        }
        Some(Err(e)) => {
          ui.label(RichText::new(e).color(Color32::from_rgb(230, 110, 110)));
        }
        None => {}
      }
      ui.add_space(6.0);
      if self.rows.is_empty() {
        ui.label(RichText::new("No agent sessions in the open folders.").weak());
        ui.label(
          RichText::new(
            "An AI client starts one with the MCP tool agent_start; you can also use Agent ▸ Start session in a folder.",
          )
          .weak()
          .small(),
        );
        return;
      }
      egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for row in &self.rows {
          let s = &row.session;
          egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
              ui.label(RichText::new(&s.name).strong().color(Color32::from_rgb(140, 180, 230)));
              ui.label(RichText::new(format!("{}  ·  {}", row.folder, s.branch)).weak());
              if s.shared {
                ui.label(RichText::new("alongside other agents").weak().small())
                  .on_hover_text("Finishing or aborting affects only the files this agent wrote through mig");
              } else {
                ui.label(RichText::new("alone").weak().small())
                  .on_hover_text("Finishing or aborting covers every change in the folder since the session started");
              }
              ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                  .button(RichText::new("Abort").color(Color32::from_rgb(230, 110, 110)))
                  .on_hover_text("Undo this agent's work (kept recoverable)")
                  .clicked()
                {
                  self.abort = Some((row.root.clone(), s.id.clone(), s.name.clone()));
                }
                if ui.button("Finish…").on_hover_text("Commit this agent's work").clicked() {
                  self.finish = Some(FinishDialog {
                    root: row.root.clone(),
                    id: s.id.clone(),
                    name: s.name.clone(),
                    message: String::new(),
                  });
                }
                if ui
                  .button("Live")
                  .on_hover_text("Watch this agent's writes")
                  .clicked()
                {
                  action = Some(Action::Live {
                    root: row.root.clone(),
                    agent: s.name.clone(),
                  });
                }
              });
            });
            ui.label(
              RichText::new(format!(
                "started {}  ·  last active {}  ·  {} write(s)  ·  {} file(s)",
                time(s.started),
                time(s.last_active),
                s.writes,
                row.files.len()
              ))
              .weak()
              .small(),
            );
            if !row.files.is_empty() {
              let key = format!("{}\u{0}{}", row.root.display(), s.id);
              let mut open = self.open.contains(&key);
              let label = if open { "Hide files" } else { "Show files" };
              ui.toggle_value(&mut open, label);
              if open {
                self.open.insert(key);
                for (path, state) in &row.files {
                  ui.horizontal(|ui| {
                    ui.label(RichText::new(path).monospace());
                    if *state == FileState::Changed {
                      ui.label(RichText::new("changed by someone else since").color(Color32::from_rgb(220, 170, 90)).small());
                    }
                  });
                }
              } else {
                self.open.remove(&key);
              }
            }
          });
          ui.add_space(4.0);
        }
      });
    });
    if self.dialogs(ui.ctx()) {
      self.refresh(folders, true);
      action = action.or(Some(Action::Changed));
    }
    action
  }

  // Returns true when a session ended.
  fn dialogs(&mut self, ctx: &egui::Context) -> bool {
    let mut ended = false;
    if let Some(d) = &mut self.finish {
      let (mut go, mut close) = (false, false);
      egui::Window::new(format!("Finish {}", d.name))
        .collapsible(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
          ui.label("The agent's work becomes one commit (next to other agents, only its own files).");
          ui.add(
            egui::TextEdit::multiline(&mut d.message)
              .hint_text(format!("commit message (empty: \"Agent: {}\")", d.name))
              .desired_rows(3)
              .desired_width(420.0),
          );
          ui.horizontal(|ui| {
            close = ui.button("Cancel").clicked();
            go = ui.button("Finish").clicked();
          });
        });
      if go {
        let msg = Some(d.message.trim().to_string()).filter(|m| !m.is_empty());
        let r = repo::open(&d.root).and_then(|r| agent::finish(&r, &d.id, msg.as_deref(), false));
        self.status = Some(match r {
          Ok(f) => Ok(format!(
            "{} finished; HEAD is {}{}",
            d.name,
            f.head.map(repo::short).unwrap_or_default(),
            if f.left > 0 {
              format!(" ({} file(s) of others left uncommitted)", f.left)
            } else {
              String::new()
            }
          )),
          Err(e) => Err(format!("{e:#}")),
        });
        self.finish = None;
        ended = true;
      } else if close {
        self.finish = None;
      }
    }
    if let Some((root, id, name)) = self.abort.clone() {
      let (mut go, mut close) = (false, false);
      egui::Window::new(format!("Abort {name}?"))
        .collapsible(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
          ui.label("The agent's work is undone. It stays recoverable with `mig agent restore`.");
          ui.horizontal(|ui| {
            close = ui.button("Cancel").clicked();
            go = ui
              .button(RichText::new("Abort").color(Color32::from_rgb(230, 110, 110)))
              .clicked();
          });
        });
      if go {
        let r = repo::open(&root).and_then(|r| agent::abort(&r, &id));
        self.status = Some(match r {
          Ok(a) => Ok(format!(
            "{name} aborted; {} file(s) put back{}",
            a.reverted,
            if a.kept.is_empty() {
              String::new()
            } else {
              format!("; left {} changed by others", a.kept.len())
            }
          )),
          Err(e) => Err(format!("{e:#}")),
        });
        self.abort = None;
        ended = true;
      } else if close {
        self.abort = None;
      }
    }
    ended
  }
}
