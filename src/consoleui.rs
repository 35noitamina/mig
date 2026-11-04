// The "Console" page: the output of every run (by an agent or by you) in the open folders, live while
// it goes. Pick a run on the left, read it on the right, filter it with a regular expression, and stop
// it if it is still going.
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use regex::Regex;

use crate::outlog::{self, Meta};

const REFRESH: Duration = Duration::from_millis(500);
const SHOWN_LINES: usize = 1500;
const LISTED: usize = 25;

struct Entry {
  root: PathBuf,
  folder: String,
  meta: Meta,
}

#[derive(Default)]
pub struct ConsoleView {
  entries: Vec<Entry>,
  sel: Option<(PathBuf, String)>,
  lines: Vec<String>,
  filter: String,
  follow: Option<bool>,
  loaded: Option<Instant>,
  status: Option<String>,
}

fn clock(ms: i64) -> String {
  chrono::DateTime::from_timestamp(ms / 1000, 0)
    .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
    .unwrap_or_default()
}

impl ConsoleView {
  pub fn running(&self) -> usize {
    self.entries.iter().filter(|e| e.meta.running()).count()
  }

  pub fn refresh(&mut self, folders: &[(PathBuf, String)]) {
    if self.loaded.is_some_and(|t| t.elapsed() < REFRESH) {
      return;
    }
    self.loaded = Some(Instant::now());
    let mut entries = Vec::new();
    for (root, folder) in folders {
      for meta in outlog::list(&root.join(".git")).into_iter().take(LISTED) {
        entries.push(Entry {
          root: root.clone(),
          folder: folder.clone(),
          meta,
        });
      }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.meta.ts));
    self.entries = entries;
    // Show the newest run until the person picks one.
    if self.sel.is_none()
      && let Some(e) = self.entries.first()
    {
      self.sel = Some((e.root.clone(), e.meta.id.clone()));
    }
    if let Some((root, id)) = &self.sel {
      self.lines = outlog::lines(&root.join(".git"), id).unwrap_or_default();
    }
  }

  pub fn ui(&mut self, ui: &mut egui::Ui, folders: &[(PathBuf, String)]) {
    self.refresh(folders);
    ui.ctx().request_repaint_after(REFRESH);
    let mut pick: Option<(PathBuf, String)> = None;
    egui::Panel::left("console-list")
      .resizable(true)
      .default_size(300.0)
      .show(ui, |ui| {
        ui.heading("Console");
        if self.entries.is_empty() {
          ui.label(RichText::new("No runs yet.").weak());
          ui.label(
            RichText::new("Runs by agents (and `mig run`) show up here while they go.")
              .weak()
              .small(),
          );
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
          for e in &self.entries {
            let m = &e.meta;
            let (mark, color) = if m.running() {
              ("●", Color32::from_rgb(110, 200, 130))
            } else if m.exit == Some(0) {
              ("✓", ui.visuals().weak_text_color())
            } else {
              ("×", Color32::from_rgb(230, 110, 110))
            };
            let selected = self.sel.as_ref().is_some_and(|(r, i)| *r == e.root && *i == m.id);
            let mut s: String = m.summary.chars().take(60).collect();
            if m.summary.chars().count() > 60 {
              s.push('…');
            }
            ui.horizontal(|ui| {
              ui.label(RichText::new(mark).color(color));
              let who = m.agent.as_deref().unwrap_or("you");
              let r = ui
                .selectable_label(selected, RichText::new(s).monospace().small())
                .on_hover_text(format!("{} · {who} · {} · {} lines", e.folder, clock(m.ts), m.lines));
              if r.clicked() {
                pick = Some((e.root.clone(), m.id.clone()));
              }
            });
          }
        });
      });
    if pick.is_some() {
      self.sel = pick;
      self.loaded = None;
      self.refresh(folders);
    }
    egui::CentralPanel::default_margins().show(ui, |ui| {
      let Some((root, id)) = self.sel.clone() else { return };
      let meta = self
        .entries
        .iter()
        .find(|e| e.root == root && e.meta.id == id)
        .map(|e| e.meta.clone());
      let Some(meta) = meta else {
        ui.label(RichText::new("That run is gone.").weak());
        return;
      };
      ui.horizontal(|ui| {
        let state = if meta.running() {
          RichText::new("running").color(Color32::from_rgb(110, 200, 130))
        } else {
          RichText::new(format!("{} · {:.1}s", meta.headline(), meta.secs)).weak()
        };
        ui.label(state);
        ui.label(RichText::new(format!("{} lines", self.lines.len())).weak().small());
        if meta.running()
          && ui
            .button(RichText::new("Stop").color(Color32::from_rgb(230, 110, 110)))
            .on_hover_text("Stop the program and everything it started")
            .clicked()
        {
          self.status = Some(match outlog::request_stop(&root.join(".git"), &id) {
            Ok(()) => "Stopping…".into(),
            Err(e) => format!("{e:#}"),
          });
        }
        let mut follow = self.follow.unwrap_or(true);
        ui.checkbox(&mut follow, "Follow").on_hover_text("Keep the end in view");
        self.follow = Some(follow);
        ui.add(
          egui::TextEdit::singleline(&mut self.filter)
            .hint_text("filter (regular expression)")
            .desired_width(220.0),
        );
        if let Some(s) = &self.status {
          ui.label(RichText::new(s).weak().small());
        }
      });
      ui.label(RichText::new(&meta.summary).monospace().small().weak());
      ui.separator();
      let re = (!self.filter.is_empty()).then(|| Regex::new(&self.filter)).transpose();
      let shown: Vec<&String> = match &re {
        Ok(Some(re)) => self.lines.iter().filter(|l| re.is_match(l)).collect(),
        _ => self.lines.iter().collect(),
      };
      if re.is_err() {
        ui.label(
          RichText::new("not a valid regular expression")
            .color(Color32::from_rgb(230, 110, 110))
            .small(),
        );
      }
      let skip = shown.len().saturating_sub(SHOWN_LINES);
      let mut text = String::new();
      if skip > 0 {
        text.push_str(&format!("… {skip} earlier lines (`mig run out {id}` has them) …\n"));
      }
      for l in &shown[skip..] {
        text.push_str(l);
        text.push('\n');
      }
      egui::ScrollArea::both()
        .auto_shrink([false, false])
        .stick_to_bottom(self.follow.unwrap_or(true))
        .show(ui, |ui| {
          ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true));
        });
    });
  }
}
