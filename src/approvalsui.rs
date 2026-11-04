// The "Approvals" page: commands and scripts an agent wants to run that wait for a person (when
// .mig.toml has mode = "ask" or "strict"), and the recent runs, including the ones mig refused.
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::approval::{self, Decision, Pending};
use crate::runtool::{self, LogEntry};

const REFRESH: Duration = Duration::from_millis(700);
const SHOWN_RUNS: usize = 40;

struct Row {
  root: PathBuf,
  folder: String,
  p: Pending,
}

struct RunRow {
  folder: String,
  e: LogEntry,
}

#[derive(Default)]
pub struct ApprovalsView {
  rows: Vec<Row>,
  runs: Vec<RunRow>,
  loaded: Option<Instant>,
  seen: HashSet<String>,
  first_load: bool,
  open_run: Option<(String, i64)>,
  status: Option<Result<String, String>>,
}

fn notify(summary: &str, body: &str) {
  let _ = notify_rust::Notification::new()
    .appname("mig")
    .summary(summary)
    .body(body)
    .show();
}

fn clock(secs: i64) -> String {
  chrono::DateTime::from_timestamp(secs, 0)
    .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
    .unwrap_or_default()
}

impl ApprovalsView {
  pub fn count(&self) -> usize {
    self.rows.len()
  }

  pub fn refresh(&mut self, folders: &[(PathBuf, String)]) {
    if self.loaded.is_some_and(|t| t.elapsed() < REFRESH) {
      return;
    }
    self.loaded = Some(Instant::now());
    let mut rows = Vec::new();
    let mut runs = Vec::new();
    for (root, folder) in folders {
      let git = root.join(".git");
      for p in approval::pending(&git) {
        rows.push(Row {
          root: root.clone(),
          folder: folder.clone(),
          p,
        });
      }
      for e in runtool::recent(&git, SHOWN_RUNS) {
        runs.push(RunRow {
          folder: folder.clone(),
          e,
        });
      }
    }
    rows.sort_by_key(|r| r.p.ts);
    runs.sort_by_key(|r| std::cmp::Reverse(r.e.ts));
    runs.truncate(SHOWN_RUNS);
    for r in &rows {
      if self.seen.insert(r.p.id.clone()) && self.first_load {
        let who = r.p.agent.as_deref().unwrap_or("An agent");
        let what: String = r.p.code.lines().next().unwrap_or("").chars().take(120).collect();
        notify(
          "mig: a run needs your approval",
          &format!("{who} ({}): {what}", r.folder),
        );
      }
    }
    self.first_load = true;
    self.rows = rows;
    self.runs = runs;
  }

  pub fn ui(&mut self, ui: &mut egui::Ui, folders: &[(PathBuf, String)]) {
    self.refresh(folders);
    ui.ctx().request_repaint_after(REFRESH);
    let mut decision: Option<(PathBuf, String, Decision)> = None;
    egui::CentralPanel::default_margins().show(ui, |ui| {
      ui.heading("Approvals");
      match &self.status {
        Some(Ok(s)) => {
          ui.label(s);
        }
        Some(Err(e)) => {
          ui.label(RichText::new(e).color(Color32::from_rgb(230, 110, 110)));
        }
        None => {}
      }
      egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if self.rows.is_empty() {
          ui.label(RichText::new("Nothing is waiting for you.").weak());
          ui.label(
            RichText::new(
              "Agents run commands on their own; mig only refuses the dangerous ones. To be asked first, set mode = \"ask\" under [run] in .mig.toml.",
            )
            .weak()
            .small(),
          );
        }
        let now = chrono::Utc::now().timestamp();
        for r in &self.rows {
          let amber = Color32::from_rgb(225, 170, 80);
          egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
              ui.label(
                RichText::new(r.p.agent.as_deref().unwrap_or("an agent"))
                  .strong()
                  .color(Color32::from_rgb(140, 180, 230)),
              );
              ui.label(RichText::new(format!("{}  ·  {}", r.folder, if r.p.cwd.is_empty() { "." } else { &r.p.cwd })).weak());
              let left = (r.p.ts + r.p.wait_secs as i64 - now).max(0);
              ui.label(RichText::new(format!("{left}s left")).weak().small());
              ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                  .button(RichText::new("Deny").color(Color32::from_rgb(230, 110, 110)))
                  .on_hover_text("Refuse; the agent is told nothing was run")
                  .clicked()
                {
                  decision = Some((r.root.clone(), r.p.id.clone(), Decision::Deny));
                }
                if ui
                  .button("Always allow")
                  .on_hover_text("Run it, and run this exact command without asking from now on")
                  .clicked()
                {
                  decision = Some((r.root.clone(), r.p.id.clone(), Decision::Always));
                }
                if ui
                  .button(RichText::new("Run").strong())
                  .on_hover_text("Run it once")
                  .clicked()
                {
                  decision = Some((r.root.clone(), r.p.id.clone(), Decision::Approve));
                }
              });
            });
            for n in &r.p.notes {
              let color = if n.level == "warn" { amber } else { ui.visuals().weak_text_color() };
              ui.label(RichText::new(format!("{}  [{}]{}", n.what, n.rule, if n.line > 0 { format!("  line {}", n.line) } else { String::new() })).color(color).small());
            }
            let flagged: HashSet<usize> = r.p.notes.iter().filter(|n| n.line > 0).map(|n| n.line).collect();
            let kind = match &r.p.lang {
              Some(l) => format!("{l} script"),
              None => "command".to_string(),
            };
            ui.label(RichText::new(kind).weak().small());
            egui::ScrollArea::vertical()
              .id_salt(("code", &r.p.id))
              .max_height(220.0)
              .show(ui, |ui| {
                for (i, line) in r.p.code.lines().enumerate() {
                  let mut t = RichText::new(format!("{:>3}  {line}", i + 1)).monospace();
                  if flagged.contains(&(i + 1)) {
                    t = t.background_color(Color32::from_rgb(100, 70, 20));
                  }
                  ui.label(t);
                }
              });
          });
          ui.add_space(4.0);
        }

        ui.add_space(10.0);
        ui.label(RichText::new("Recent runs").strong());
        if self.runs.is_empty() {
          ui.label(RichText::new("No runs yet.").weak());
        }
        for r in &self.runs {
          let (label, color) = match (r.e.verdict.as_str(), r.e.exit, r.e.timed_out) {
            (_, _, true) => ("timed out".to_string(), Color32::from_rgb(225, 170, 80)),
            ("ran", Some(0), _) => ("ok".to_string(), Color32::from_rgb(110, 200, 130)),
            ("ran", Some(c), _) => (format!("exit {c}"), Color32::from_rgb(230, 110, 110)),
            (v, _, _) => (v.to_string(), Color32::from_rgb(230, 110, 110)),
          };
          let key = (r.folder.clone(), r.e.ts);
          let open = self.open_run.as_ref() == Some(&key);
          let who = r.e.agent.as_deref().unwrap_or("(you)");
          let head = ui.horizontal(|ui| {
            ui.label(RichText::new(clock(r.e.ts / 1000)).weak().monospace().small());
            ui.label(RichText::new(format!("{label:<12}")).color(color).monospace().small());
            ui.label(RichText::new(format!("{} · {who}", r.folder)).weak().small());
            let mut s = r.e.summary.clone();
            if s.chars().count() > 110 {
              s = s.chars().take(110).collect::<String>() + "…";
            }
            ui.add(egui::Label::new(RichText::new(s).monospace().small()).sense(egui::Sense::click()))
          });
          if head.inner.on_hover_text("Click to show the output").clicked() {
            self.open_run = if open { None } else { Some(key) };
          }
          if open && !r.e.tail.is_empty() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
              ui.set_width(ui.available_width());
              ui.label(RichText::new(&r.e.tail).monospace().small());
            });
          }
        }
      });
    });
    if let Some((root, id, d)) = decision {
      let r = approval::decide(&root.join(".git"), &id, d);
      self.status = Some(match (r, d) {
        (Ok(()), Decision::Deny) => Ok("Denied.".into()),
        (Ok(()), Decision::Always) => Ok("Running; this exact command will not ask again.".into()),
        (Ok(()), Decision::Approve) => Ok("Running.".into()),
        (Err(e), _) => Err(format!("{e:#}")),
      });
      self.loaded = None;
    }
  }
}
