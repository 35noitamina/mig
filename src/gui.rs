use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Color32, RichText};
use git2::{DiffOptions, Oid, Repository};

use crate::repo::Change;
use crate::scan::{Finding, SecretsFound};
use crate::{oplog, ops, repo, rev, scan};

const HISTORY: usize = 300;
const REFRESH: Duration = Duration::from_secs(3);

struct Row {
  oid: Oid,
  summary: String,
  time: String,
}

#[derive(PartialEq)]
enum View {
  Changes,
  Commit(usize),
}

struct App {
  repo: Repository,
  branch: String,
  rows: Vec<Row>,
  changes: Vec<Change>,
  findings: Vec<Finding>,
  alerted: Vec<Finding>,
  selected: BTreeSet<usize>,
  view: View,
  diff: Vec<(Color32, String)>,
  message: String,
  status: Result<String, String>,
  confirm_save: Option<Vec<Finding>>,
  confirm_sync: Option<Vec<Finding>>,
  reword: Option<(usize, String)>,
  last_refresh: Instant,
}

pub fn run(dir: PathBuf) -> Result<()> {
  let repo = repo::open(&dir)?;
  let title = format!(
    "mig — {}",
    dir.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
  );
  let opts = eframe::NativeOptions {
    viewport: egui::ViewportBuilder::default()
      .with_title(title)
      .with_inner_size([1100.0, 720.0]),
    ..Default::default()
  };
  eframe::run_native(
    "mig",
    opts,
    Box::new(|cc| {
      load_cjk_font(&cc.egui_ctx);
      let mut app = App {
        repo,
        branch: String::new(),
        rows: vec![],
        changes: vec![],
        findings: vec![],
        alerted: vec![],
        selected: BTreeSet::new(),
        view: View::Changes,
        diff: vec![],
        message: String::new(),
        status: Ok(String::new()),
        confirm_save: None,
        confirm_sync: None,
        reword: None,
        last_refresh: Instant::now(),
      };
      app.refresh();
      Ok(Box::new(app))
    }),
  )
  .map_err(|e| anyhow::anyhow!("{e}"))
}

// egui's bundled fonts have no CJK glyphs; borrow one from the OS when available.
fn load_cjk_font(ctx: &egui::Context) {
  let candidates = [
    "C:/Windows/Fonts/YuGothM.ttc",
    "C:/Windows/Fonts/meiryo.ttc",
    "C:/Windows/Fonts/msgothic.ttc",
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
  ];
  let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
    return;
  };
  let mut fonts = egui::FontDefinitions::default();
  fonts
    .font_data
    .insert("cjk".into(), Arc::new(egui::FontData::from_owned(bytes)));
  for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
    fonts.families.entry(family).or_default().push("cjk".into());
  }
  ctx.set_fonts(fonts);
}

impl App {
  fn refresh(&mut self) {
    self.last_refresh = Instant::now();
    self.branch = repo::branch_name(&self.repo).unwrap_or_else(|| "(detached)".into());
    self.rows = rev::chain(&self.repo, HISTORY)
      .map(|cs| {
        cs.iter()
          .map(|c| Row {
            oid: c.id(),
            summary: repo::summary(c),
            time: crate::cli::fmt_time(c.time().seconds()),
          })
          .collect()
      })
      .unwrap_or_default();
    self.selected.retain(|&i| i < self.rows.len());
    self.changes = repo::changes(&self.repo).unwrap_or_default();
    self.findings = scan::scan_pending(&self.repo).unwrap_or_default();
    let fresh: Vec<&Finding> = self.findings.iter().filter(|f| !self.alerted.contains(f)).collect();
    if !fresh.is_empty() {
      notify(
        &format!("{} possible secret(s) in your changes", fresh.len()),
        &fresh[0].path,
      );
    }
    self.alerted = self.findings.clone();
    self.load_diff();
  }

  fn load_diff(&mut self) {
    let d = match self.view {
      View::Changes => {
        let head = repo::head_commit(&self.repo).ok().flatten().and_then(|c| c.tree().ok());
        let mut opts = DiffOptions::new();
        opts
          .include_untracked(true)
          .recurse_untracked_dirs(true)
          .show_untracked_content(true);
        self
          .repo
          .diff_tree_to_workdir_with_index(head.as_ref(), Some(&mut opts))
      }
      View::Commit(i) => {
        let Some(row) = self.rows.get(i) else { return };
        let c = self.repo.find_commit(row.oid).unwrap();
        let parent = c.parents().next().and_then(|p| p.tree().ok());
        self
          .repo
          .diff_tree_to_tree(parent.as_ref(), c.tree().ok().as_ref(), None)
      }
    };
    let text = d
      .map_err(anyhow::Error::from)
      .and_then(|d| crate::cli::render_diff(&d, false))
      .unwrap_or_default();
    self.diff = text
      .lines()
      .take(5000)
      .map(|l| {
        let col = if l.starts_with("diff --git") || l.starts_with("+++") || l.starts_with("---") {
          Color32::from_rgb(200, 200, 120)
        } else if l.starts_with('+') {
          Color32::from_rgb(110, 200, 110)
        } else if l.starts_with('-') {
          Color32::from_rgb(230, 110, 110)
        } else if l.starts_with("@@") {
          Color32::from_rgb(100, 180, 220)
        } else {
          Color32::GRAY
        };
        (col, l.to_string())
      })
      .collect();
  }

  fn done(&mut self, r: Result<String>) {
    self.status = r.map_err(|e| format!("{e:#}"));
    self.refresh();
  }

  fn save(&mut self, force: bool) {
    let msg = std::mem::take(&mut self.message);
    let r = oplog::record(&self.repo, "save", || ops::save(&self.repo, Some(&msg), &[], force));
    match r {
      Ok(oid) => self.done(Ok(format!("saved {}", repo::short(oid)))),
      Err(e) => {
        self.message = msg;
        match e.downcast::<SecretsFound>() {
          Ok(SecretsFound(f)) => self.confirm_save = Some(f),
          Err(e) => self.done(Err(e)),
        }
      }
    }
  }

  fn sync(&mut self, force: bool) {
    match oplog::record(&self.repo, "sync", || ops::sync(&self.repo, force)) {
      Ok(r) => self.done(Ok(format!("synced: {} pushed, {} rebased", r.pushed, r.rebased))),
      Err(e) => match e.downcast::<SecretsFound>() {
        Ok(SecretsFound(f)) => {
          notify(
            "Push blocked",
            &format!("{} possible secret(s) in unpushed commits", f.len()),
          );
          self.confirm_sync = Some(f);
        }
        Err(e) => self.done(Err(e)),
      },
    }
  }

  fn fold_selected(&mut self) {
    let (Some(&a), Some(&b)) = (self.selected.first(), self.selected.last()) else {
      return;
    };
    let b = if a == b { a + 1 } else { b };
    let r = oplog::record(&self.repo, &format!("fold {}..{}", a + 1, b + 1), || {
      ops::fold(&self.repo, a, b, None)
    });
    self.selected.clear();
    self.view = View::Changes;
    self.done(r.map(|_| format!("folded {} commits", b - a + 1)));
  }

  fn drop_commit(&mut self, i: usize) {
    let r = oplog::record(&self.repo, &format!("drop {}", i + 1), || {
      ops::drop_commit(&self.repo, i)
    });
    self.selected.clear();
    self.view = View::Changes;
    self.done(r.map(|_| format!("dropped #{}", i + 1)));
  }

  fn toolbar(&mut self, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
      ui.label(RichText::new(format!("branch: {}", self.branch)).strong());
      ui.separator();
      ui.add(
        egui::TextEdit::singleline(&mut self.message)
          .hint_text("message (optional)")
          .desired_width(320.0),
      );
      if ui
        .add_enabled(!self.changes.is_empty(), egui::Button::new("Save"))
        .clicked()
      {
        self.save(false);
      }
      if ui.button("Sync").clicked() {
        self.sync(false);
      }
      ui.separator();
      if ui.button("Undo").clicked() {
        let r = oplog::undo(&self.repo, false).map(|op| format!("undid: {}", op.cmd));
        self.done(r);
      }
      if ui.button("Redo").clicked() {
        let r = oplog::redo(&self.repo, false).map(|op| format!("redid: {}", op.cmd));
        self.done(r);
      }
      if ui.button("Refresh").clicked() {
        self.refresh();
      }
    });
  }

  fn banner(&mut self, ui: &mut egui::Ui) {
    if self.findings.is_empty() {
      return;
    }
    egui::Frame::new()
      .fill(Color32::from_rgb(120, 20, 20))
      .inner_margin(6.0)
      .show(ui, |ui| {
        ui.label(
          RichText::new(format!("⚠ {} possible secret(s) in your changes", self.findings.len()))
            .color(Color32::WHITE)
            .strong(),
        );
        for f in self.findings.iter().take(5) {
          ui.label(RichText::new(format!("{}:{}  {}  {}", f.path, f.line, f.rule, f.masked)).color(Color32::WHITE));
        }
      });
  }

  fn sidebar(&mut self, ui: &mut egui::Ui) {
    let changes_label = format!("Changes ({})", self.changes.len());
    if ui
      .selectable_label(self.view == View::Changes, RichText::new(changes_label).strong())
      .clicked()
    {
      self.view = View::Changes;
      self.selected.clear();
      self.load_diff();
    }
    for c in &self.changes {
      ui.label(RichText::new(format!("  {} {}", c.kind, c.path)).monospace().small());
    }
    ui.separator();
    ui.horizontal(|ui| {
      ui.label(RichText::new("History").strong());
      let n = self.selected.len();
      let label = if n > 1 {
        format!("Fold {n}")
      } else {
        "Fold into parent".into()
      };
      if ui.add_enabled(n >= 1, egui::Button::new(label)).clicked() {
        self.fold_selected();
      }
    });
    ui.label(
      RichText::new("Ctrl/Cmd-click or Shift-click to select several")
        .small()
        .weak(),
    );
    let mut clicked = None;
    let mut action = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
      for (i, row) in self.rows.iter().enumerate() {
        let text = format!("{:>3}  {}  {}", i + 1, &row.oid.to_string()[..7], row.summary);
        let resp = ui.selectable_label(self.selected.contains(&i), RichText::new(text).monospace());
        let resp = resp.on_hover_text(&row.time);
        if resp.clicked() {
          clicked = Some(i);
        }
        resp.context_menu(|ui| {
          if ui.button("Reword…").clicked() {
            action = Some(("reword", i));
            ui.close();
          }
          if ui.button("Fold into parent").clicked() {
            action = Some(("fold", i));
            ui.close();
          }
          if ui.button("Drop").clicked() {
            action = Some(("drop", i));
            ui.close();
          }
        });
      }
    });
    if let Some(i) = clicked {
      let m = ui.input(|inp| inp.modifiers);
      if m.command {
        if !self.selected.remove(&i) {
          self.selected.insert(i);
        }
      } else if m.shift && !self.selected.is_empty() {
        let a = *self.selected.first().unwrap();
        let b = *self.selected.last().unwrap();
        self.selected = (a.min(i)..=b.max(i)).collect();
      } else {
        self.selected = BTreeSet::from([i]);
      }
      self.view = View::Commit(i);
      self.load_diff();
    }
    match action {
      Some(("reword", i)) => {
        let msg = self
          .repo
          .find_commit(self.rows[i].oid)
          .map(|c| repo::message(&c))
          .unwrap_or_default();
        self.reword = Some((i, msg));
      }
      Some(("fold", i)) => {
        self.selected = BTreeSet::from([i]);
        self.fold_selected();
      }
      Some(("drop", i)) => self.drop_commit(i),
      _ => {}
    }
  }

  fn dialogs(&mut self, ctx: &egui::Context) {
    if let Some(found) = self.confirm_save.clone() {
      let mut close = false;
      egui::Window::new("Secrets detected")
        .collapsible(false)
        .show(ctx, |ui| {
          findings_list(ui, &found);
          ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
              close = true;
            }
            if ui.button(RichText::new("Save anyway").color(Color32::RED)).clicked() {
              close = true;
              self.confirm_save = None;
              self.save(true);
            }
          });
        });
      if close {
        self.confirm_save = None;
      }
    }
    if let Some(found) = self.confirm_sync.clone() {
      let mut close = false;
      egui::Window::new("Push blocked").collapsible(false).show(ctx, |ui| {
        ui.label("Unpushed commits contain possible secrets. Remove them (e.g. Drop) before syncing.");
        findings_list(ui, &found);
        ui.horizontal(|ui| {
          if ui.button("OK").clicked() {
            close = true;
          }
          if ui.button(RichText::new("Push anyway").color(Color32::RED)).clicked() {
            close = true;
            self.confirm_sync = None;
            self.sync(true);
          }
        });
      });
      if close {
        self.confirm_sync = None;
      }
    }
    if let Some((i, mut msg)) = self.reword.take() {
      let mut keep = true;
      let mut apply = false;
      egui::Window::new(format!("Reword #{}", i + 1))
        .collapsible(false)
        .show(ctx, |ui| {
          ui.add(egui::TextEdit::multiline(&mut msg).desired_rows(6).desired_width(420.0));
          ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
              keep = false;
            }
            if ui.button("Apply").clicked() {
              apply = true;
            }
          });
        });
      if apply {
        let r = oplog::record(&self.repo, &format!("reword {}", i + 1), || {
          ops::reword(&self.repo, i, &msg)
        });
        self.done(r.map(|_| format!("reworded #{}", i + 1)));
      } else if keep {
        self.reword = Some((i, msg));
      }
    }
  }
}

fn findings_list(ui: &mut egui::Ui, found: &[Finding]) {
  for f in found.iter().take(10) {
    ui.label(RichText::new(format!("{}:{}  {}  {}", f.path, f.line, f.rule, f.masked)).monospace());
  }
  if found.len() > 10 {
    ui.label(format!("…and {} more", found.len() - 10));
  }
}

fn notify(summary: &str, body: &str) {
  let _ = notify_rust::Notification::new()
    .appname("mig")
    .summary(&format!("mig: {summary}"))
    .body(body)
    .show();
}

impl eframe::App for App {
  fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    let ctx = ui.ctx().clone();
    if self.last_refresh.elapsed() >= REFRESH {
      self.refresh();
    }
    ctx.request_repaint_after(REFRESH);

    egui::Panel::top("toolbar").show(ui, |ui| {
      ui.add_space(4.0);
      self.toolbar(ui);
      ui.add_space(4.0);
    });
    egui::Panel::bottom("status").show(ui, |ui| match &self.status {
      Ok(s) => {
        ui.label(s);
      }
      Err(e) => {
        ui.label(RichText::new(e).color(Color32::from_rgb(230, 110, 110)));
      }
    });
    egui::Panel::left("side")
      .resizable(true)
      .default_size(380.0)
      .show(ui, |ui| self.sidebar(ui));
    egui::CentralPanel::default_margins().show(ui, |ui| {
      self.banner(ui);
      egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        for (col, line) in &self.diff {
          ui.label(RichText::new(line).monospace().color(*col));
        }
      });
    });
    self.dialogs(&ctx);
  }
}
