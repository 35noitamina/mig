// The "Console" page: the output of every run (by an agent or by you) in the open folders, live while
// it goes. Pick a run on the left, read it on the right, filter it with a regular expression, and stop
// it if it is still going.
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::outlog::{self, Meta};
use crate::rundetail;

const REFRESH: Duration = Duration::from_millis(500);
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
  state: rundetail::State,
  loaded: Option<Instant>,
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
          ui.label(RichText::new("Runs by agents (and `mig run`) show up here while they go.").weak());
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
              ui.add_sized(
                [14.0, ui.text_style_height(&egui::TextStyle::Body)],
                egui::Label::new(RichText::new(mark).color(color)),
              );
              let who = m.agent.as_deref().unwrap_or("you");
              let r = ui
                .selectable_label(selected, RichText::new(s).monospace())
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
      let folder = self.entries.iter().find(|e| e.root == root).map(|e| e.folder.clone());
      rundetail::show(ui, &root.join(".git"), &id, &mut self.state, folder.as_deref());
    });
  }
}
