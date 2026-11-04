// The "Monitor" page: which watches AI agents hold on the open folders (what they listen for, where
// the news is pushed), and the events that reached them, newest first.
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::events::{self, Logged, WatchInfo};

const REFRESH: Duration = Duration::from_millis(700);
const SHOWN_EVENTS: usize = 150;

struct Watch {
  folder: String,
  w: WatchInfo,
}

struct Ev {
  folder: String,
  e: Logged,
}

#[derive(Default)]
pub struct MonitorView {
  watches: Vec<Watch>,
  events: Vec<Ev>,
  loaded: Option<Instant>,
  kind: Option<String>,
}

fn clock(secs: i64) -> String {
  chrono::DateTime::from_timestamp(secs, 0)
    .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
    .unwrap_or_default()
}

fn kind_color(k: &str) -> Color32 {
  match k {
    "message" => Color32::from_rgb(110, 200, 130),
    "agent" => Color32::from_rgb(110, 170, 235),
    "run" | "job" => Color32::from_rgb(225, 170, 80),
    "commit" => Color32::from_rgb(170, 140, 220),
    "file" => Color32::from_rgb(200, 200, 200),
    _ => Color32::from_rgb(180, 180, 180),
  }
}

impl MonitorView {
  pub fn count(&self) -> usize {
    self.watches.len()
  }

  pub fn refresh(&mut self, folders: &[(PathBuf, String)]) {
    if self.loaded.is_some_and(|t| t.elapsed() < REFRESH) {
      return;
    }
    self.loaded = Some(Instant::now());
    let (mut watches, mut events) = (Vec::new(), Vec::new());
    for (root, folder) in folders {
      let git = root.join(".git");
      for w in events::watches(&git) {
        watches.push(Watch {
          folder: folder.clone(),
          w,
        });
      }
      for e in events::read_log(&git, SHOWN_EVENTS) {
        events.push(Ev {
          folder: folder.clone(),
          e,
        });
      }
    }
    events.sort_by_key(|e| std::cmp::Reverse(e.e.ts));
    events.truncate(SHOWN_EVENTS);
    self.watches = watches;
    self.events = events;
  }

  pub fn ui(&mut self, ui: &mut egui::Ui, folders: &[(PathBuf, String)]) {
    self.refresh(folders);
    ui.ctx().request_repaint_after(REFRESH);
    egui::CentralPanel::default_margins().show(ui, |ui| {
      ui.heading("Monitor");
      ui.label(
        RichText::new(
          "What agents are listening for, and what they were told. Set with the watch tool; wait_events collects it.",
        )
        .weak(),
      );
      ui.add_space(6.0);
      egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.label(RichText::new("Watching").strong());
        if self.watches.is_empty() {
          ui.label(RichText::new("No agent is connected to the open folders.").weak());
        }
        let now = chrono::Utc::now().timestamp();
        for w in &self.watches {
          egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
              ui.label(
                RichText::new(w.w.agent.as_deref().unwrap_or("connected, no session"))
                  .strong()
                  .color(Color32::from_rgb(140, 180, 230)),
              );
              ui.label(RichText::new(format!("{}  ·  pid {}", w.folder, w.w.pid)).weak());
              ui.label(RichText::new(format!("since {}", clock(w.w.started))).weak());
              if now - w.w.ts > 8 {
                ui.label(RichText::new("not answering").color(Color32::from_rgb(225, 170, 80)));
              }
            });
            if w.w.kinds.is_empty() {
              ui.label(RichText::new("not watching anything (unwatch)").weak());
            } else {
              ui.horizontal_wrapped(|ui| {
                for k in &w.w.kinds {
                  ui.label(RichText::new(k).color(kind_color(k)).monospace());
                }
              });
            }
            let mut notes: Vec<String> = Vec::new();
            if !w.w.paths.is_empty() {
              notes.push(format!("files: {}", w.w.paths.join(", ")));
            }
            notes.push(if w.w.push {
              "pushed as a channel".into()
            } else {
              "wait_events only".into()
            });
            if w.w.include_own {
              notes.push("includes its own doings".into());
            }
            if let Some(e) = w.w.every_secs {
              notes.push(format!("timer every {e}s"));
            }
            if w.w.webhook {
              notes.push("webhook open".into());
            }
            ui.label(RichText::new(notes.join("  ·  ")).weak());
          });
          ui.add_space(4.0);
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
          ui.label(RichText::new("Events").strong());
          let kinds: std::collections::BTreeSet<String> = self.events.iter().map(|e| e.e.kind.clone()).collect();
          if ui.selectable_label(self.kind.is_none(), "all").clicked() {
            self.kind = None;
          }
          for k in kinds {
            if ui
              .selectable_label(
                self.kind.as_deref() == Some(k.as_str()),
                RichText::new(&k).color(kind_color(&k)),
              )
              .clicked()
            {
              self.kind = Some(k);
            }
          }
        });
        let mut any = false;
        for e in self
          .events
          .iter()
          .filter(|e| self.kind.as_ref().is_none_or(|k| *k == e.e.kind))
        {
          any = true;
          ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(clock(e.e.ts)).weak().monospace());
            ui.label(
              RichText::new(format!("{:<8}", e.e.kind))
                .color(kind_color(&e.e.kind))
                .monospace(),
            );
            let to = e.e.by.as_deref().map(|b| format!(" → {b}")).unwrap_or_default();
            ui.label(RichText::new(format!("{}{to}", e.folder)).weak());
            let text: String = e.e.text.lines().next().unwrap_or("").chars().take(200).collect();
            ui.label(RichText::new(text).monospace());
          });
        }
        if !any {
          ui.label(RichText::new("Nothing has been heard yet.").weak());
        }
      });
    });
  }
}
