// AI features in the GUI. Requests run on worker threads with their own repository handle.
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use anyhow::Result;
use eframe::egui::{self, Color32, RichText};
use git2::Repository;

use crate::ai::AiConfig;
use crate::assist::{self, ContextOpts, Group};
use crate::{agent, repo};

pub enum Out {
  Message(String),
  Text { title: String, body: String },
  Tidy(Vec<Group>),
  Copy(String),
  Done(String),
}

struct Job {
  label: String,
  rx: Receiver<Result<(Out, usize)>>,
}

struct FinishDialog {
  message: String,
}

pub struct AiPanel {
  root: PathBuf,
  ctx: egui::Context,
  job: Option<Job>,
  result: Option<(String, String)>,
  tidy: Option<Vec<Group>>,
  finish: Option<FinishDialog>,
  md: egui_commonmark::CommonMarkCache,
  // A generated commit message for the toolbar to pick up.
  pub message_out: Option<String>,
}

impl AiPanel {
  pub fn new(root: PathBuf, ctx: egui::Context) -> AiPanel {
    AiPanel {
      root,
      ctx,
      job: None,
      result: None,
      tidy: None,
      finish: None,
      md: Default::default(),
      message_out: None,
    }
  }

  pub fn busy(&self) -> Option<&str> {
    self.job.as_ref().map(|j| j.label.as_str())
  }

  pub fn spawn(
    &mut self,
    label: &str,
    f: impl FnOnce(&Repository, &AiConfig) -> Result<(Out, usize)> + Send + 'static,
  ) {
    if self.job.is_some() {
      return;
    }
    let (tx, rx) = mpsc::channel();
    let root = self.root.clone();
    let ctx = self.ctx.clone();
    std::thread::spawn(move || {
      let r = repo::open(&root).and_then(|repo| f(&repo, &AiConfig::load()));
      let _ = tx.send(r);
      ctx.request_repaint();
    });
    self.job = Some(Job {
      label: label.to_string(),
      rx,
    });
  }

  pub fn commit_message(&mut self) {
    self.spawn("writing a commit message…", |repo, cfg| {
      assist::commit_message(repo, cfg).map(|r| (Out::Message(r.text), r.redacted))
    });
  }

  pub fn explain(&mut self, rev: Option<String>, review: bool) {
    let what = rev
      .as_ref()
      .map_or("current changes".to_string(), |r| format!("commit #{r}"));
    let title = format!("{} {what}", if review { "Review of" } else { "About" });
    self.spawn(
      if review { "reviewing…" } else { "explaining…" },
      move |repo, cfg| {
        let r = if review {
          assist::review(repo, cfg, rev.as_deref())?
        } else {
          assist::explain(repo, cfg, rev.as_deref())?
        };
        Ok((Out::Text { title, body: r.text }, r.redacted))
      },
    );
  }

  pub fn tidy(&mut self) {
    self.spawn("planning a tidier history…", |repo, cfg| {
      Ok((Out::Tidy(assist::tidy_plan(repo, cfg, 20)?), 0))
    });
  }

  pub fn copy_context(&mut self) {
    self.spawn("collecting context…", |repo, _| {
      let (text, n) = assist::context(
        repo,
        &ContextOpts {
          diff: true,
          files: vec![],
          commits: 15,
        },
      )?;
      Ok((Out::Copy(text), n))
    });
  }

  pub fn open_finish(&mut self) {
    self.finish = Some(FinishDialog { message: String::new() });
  }

  // Returns a status line when a job ends.
  pub fn poll(&mut self) -> Option<Result<String, String>> {
    let job = self.job.as_ref()?;
    let r = match job.rx.try_recv() {
      Ok(r) => r,
      Err(mpsc::TryRecvError::Empty) => return None,
      Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("the AI request stopped unexpectedly")),
    };
    self.job = None;
    let (out, redacted) = match r {
      Ok(v) => v,
      Err(e) => return Some(Err(format!("{e:#}"))),
    };
    let note = if redacted > 0 {
      format!(" ({redacted} possible secret(s) were redacted before sending)")
    } else {
      String::new()
    };
    let status = match out {
      Out::Message(m) => {
        self.message_out = Some(m);
        format!("commit message suggested{note}")
      }
      Out::Text { title, body } => {
        self.result = Some((title, body));
        format!("AI answer ready{note}")
      }
      Out::Tidy(groups) if groups.is_empty() => "the history already looks tidy".into(),
      Out::Tidy(groups) => {
        self.tidy = Some(groups);
        "tidy plan ready".into()
      }
      Out::Copy(text) => {
        self.ctx.copy_text(text);
        format!("repository context copied to the clipboard{note}")
      }
      Out::Done(s) => s,
    };
    Some(Ok(status))
  }

  // Draws the AI windows. Returns a status line and whether history changed.
  pub fn windows(&mut self, ctx: &egui::Context, repo: &Repository) -> Option<(Result<String, String>, bool)> {
    let mut outcome = None;
    if let Some((title, body)) = &self.result {
      let mut open = true;
      egui::Window::new(title.as_str())
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .open(&mut open)
        .default_size([620.0, 480.0])
        .resizable(true)
        .show(ctx, |ui| {
          egui::ScrollArea::vertical().show(ui, |ui| {
            egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.md, body);
          });
        });
      if !open {
        self.result = None;
      }
    }

    if let Some(groups) = self.tidy.clone() {
      let mut keep = true;
      egui::Window::new("Tidy history")
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .collapsible(false)
        .show(ctx, |ui| {
          ui.label("The AI suggests folding these commits (numbers as in the History list):");
          ui.add_space(4.0);
          for g in &groups {
            ui.label(RichText::new(format!("#{}–#{}", g.from, g.to)).strong());
            ui.label(RichText::new(g.message.trim()).monospace());
            ui.add_space(4.0);
          }
          ui.horizontal(|ui| {
            if ui
              .button("Cancel")
              .on_hover_text("Leave the history as it is")
              .clicked()
            {
              keep = false;
            }
            if ui
              .button("Apply")
              .on_hover_text("Fold the commits as shown (mig undo reverts it)")
              .clicked()
            {
              keep = false;
              outcome = Some((
                assist::apply_tidy(repo, &groups)
                  .map(|_| format!("folded {} group(s)", groups.len()))
                  .map_err(|e| format!("{e:#}")),
                true,
              ));
            }
          });
        });
      if !keep {
        self.tidy = None;
      }
    }

    if let Some(d) = &mut self.finish {
      let mut close = false;
      let mut go: Option<Option<String>> = None;
      egui::Window::new("Finish agent session")
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .collapsible(false)
        .show(ctx, |ui| {
          ui.label("Everything since the session started becomes one commit.");
          ui.add(
            egui::TextEdit::multiline(&mut d.message)
              .hint_text("commit message (leave empty to let the AI write it)")
              .desired_rows(4)
              .desired_width(420.0),
          );
          ui.horizontal(|ui| {
            if ui.button("Cancel").on_hover_text("Keep the session running").clicked() {
              close = true;
            }
            let label = if d.message.trim().is_empty() {
              "Finish with AI message"
            } else {
              "Finish"
            };
            if ui
              .button(label)
              .on_hover_text("Squash the session's work into one commit")
              .clicked()
            {
              go = Some(Some(d.message.trim().to_string()).filter(|m| !m.is_empty()));
            }
          });
        });
      if let Some(msg) = go {
        self.finish = None;
        self.spawn("finishing the agent session…", move |repo, _| {
          let f = agent::finish(repo, msg.as_deref(), msg.is_none())?;
          Ok((
            Out::Done(format!(
              "agent session finished; HEAD is {}",
              f.head.map(repo::short).unwrap_or_default()
            )),
            0,
          ))
        });
      } else if close {
        self.finish = None;
      }
    }
    if let Some(label) = self.busy() {
      egui::Area::new(egui::Id::new("ai-busy"))
        .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -32.0])
        .show(ctx, |ui| {
          egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
              ui.spinner();
              ui.label(RichText::new(label).color(Color32::from_rgb(140, 180, 230)));
            });
          });
        });
    }
    outcome
  }
}
