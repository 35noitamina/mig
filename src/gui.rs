use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Color32, RichText};
use git2::{DiffOptions, Oid, Repository};

use crate::editor::{self, Editor};
use crate::guard::{self, SaveBlocked};
use crate::repo::Change;
use crate::scan::{Finding, SecretsFound};
use crate::{absorb, config, oplog, ops, repo, rev, scan, split};

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

#[derive(PartialEq, Clone, Copy)]
enum Mode {
  Editor,
  History,
}

struct SplitDialog {
  pos: usize,
  files: Vec<(String, bool)>,
  message: String,
}

struct App {
  repo: Repository,
  mode: Mode,
  editor: Editor,
  auto_lines: usize,
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
  confirm_save: Option<SaveBlocked>,
  confirm_sync: Option<Vec<Finding>>,
  reword: Option<(usize, String)>,
  split: Option<SplitDialog>,
  last_refresh: Instant,
}

pub fn run(dir: PathBuf) -> Result<()> {
  let repo = repo::open(&dir)?;
  let root = repo.workdir().unwrap_or(&dir).to_path_buf();
  let title = format!(
    "mig — {}",
    root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
  );
  let opts = eframe::NativeOptions {
    viewport: egui::ViewportBuilder::default()
      .with_title(title)
      .with_inner_size([1200.0, 780.0]),
    ..Default::default()
  };
  eframe::run_native(
    "mig",
    opts,
    Box::new(|cc| {
      load_cjk_font(&cc.egui_ctx);
      let cfg = config::load_dir(&root).unwrap_or_default();
      let mut editor = Editor::new(root.clone());
      (editor.check_cmd, editor.run_cmd) = config::commands(&root, &cfg.editor);
      let mut app = App {
        repo,
        mode: Mode::Editor,
        editor,
        auto_lines: cfg.editor.auto_commit_lines,
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
        split: None,
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

fn workdir_diff(repo: &Repository) -> Result<git2::Diff<'_>> {
  let head = repo::head_commit(repo)?.map(|c| c.tree()).transpose()?;
  let mut opts = DiffOptions::new();
  opts
    .include_untracked(true)
    .recurse_untracked_dirs(true)
    .show_untracked_content(true);
  Ok(repo.diff_tree_to_workdir_with_index(head.as_ref(), Some(&mut opts))?)
}

fn tracked_files(repo: &Repository, changes: &[Change]) -> Vec<String> {
  let mut files: Vec<String> = repo
    .index()
    .map(|idx| {
      idx
        .iter()
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect()
    })
    .unwrap_or_default();
  files.extend(changes.iter().filter(|c| c.kind == '?').map(|c| c.path.clone()));
  let deleted: Vec<&str> = changes
    .iter()
    .filter(|c| c.kind == 'D')
    .map(|c| c.path.as_str())
    .collect();
  files.retain(|f| !deleted.contains(&f.as_str()));
  files.sort();
  files.dedup();
  files
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
    self.editor.set_files(
      tracked_files(&self.repo, &self.changes),
      self.changes.iter().map(|c| c.path.clone()).collect(),
    );
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
      View::Changes => workdir_diff(&self.repo),
      View::Commit(i) => {
        let Some(row) = self.rows.get(i) else { return };
        (|| {
          let c = self.repo.find_commit(row.oid)?;
          let parent = c.parents().next().map(|p| p.tree()).transpose()?;
          Ok(self.repo.diff_tree_to_tree(parent.as_ref(), Some(&c.tree()?), None)?)
        })()
      }
    };
    let text = d.and_then(|d| crate::cli::render_diff(&d, false)).unwrap_or_default();
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

  fn commit(&mut self, force: bool) {
    if let Err(e) = self.editor.save_all() {
      self.status = Err(e);
      return;
    }
    let msg = std::mem::take(&mut self.message);
    let r = oplog::record(&self.repo, "save", || ops::save(&self.repo, Some(&msg), &[], force));
    match r {
      Ok(s) => {
        let mut text = format!("saved {}", repo::short(s.oid));
        for w in s.warnings {
          text.push_str(&format!(" · warning: {w}"));
        }
        self.done(Ok(text));
      }
      Err(e) => {
        self.message = msg;
        match e.downcast::<SaveBlocked>() {
          Ok(b) => self.confirm_save = Some(b),
          Err(e) => self.done(Err(e)),
        }
      }
    }
  }

  fn changed_lines(&self) -> usize {
    workdir_diff(&self.repo)
      .and_then(|d| Ok(d.stats()?))
      .map(|s| s.insertions() + s.deletions())
      .unwrap_or(0)
  }

  // Called after the editor writes files: auto-commit once enough has changed and the check passes.
  fn after_save(&mut self, ctx: &egui::Context) {
    self.refresh();
    if self.auto_lines == 0 || self.editor.checking() {
      return;
    }
    let n = self.changed_lines();
    if n < self.auto_lines {
      return;
    }
    match self.editor.check_cmd.clone() {
      Some(cmd) => self.editor.start_check(&cmd, ctx),
      None => self.auto_commit(),
    }
  }

  fn auto_commit(&mut self) {
    let msg = format!(
      "auto: {}",
      ops::pending_summary(&self.repo).unwrap_or_else(|_| "Update".into())
    );
    let r = oplog::record(&self.repo, "save (auto)", || {
      ops::save(&self.repo, Some(&msg), &[], false)
    });
    match r {
      Ok(s) => {
        self
          .editor
          .output
          .push(&format!("[mig] auto-committed {} {msg}\n", repo::short(s.oid)));
        self.done(Ok(format!("auto-committed {}", repo::short(s.oid))));
      }
      Err(e) => {
        let text = format!("auto-commit skipped: {e:#}");
        self.editor.output.push(&format!("[mig] {text}\n"));
        if e.is::<SaveBlocked>() {
          notify("Auto-commit skipped", &e.to_string());
        }
        self.done(Err(anyhow::anyhow!(text)));
      }
    }
  }

  fn poll_check(&mut self, ctx: &egui::Context) {
    let Some((ok, generation)) = self.editor.poll_check() else {
      return;
    };
    if !ok {
      self.editor.output.push("[mig] check failed; not committing\n");
      self.status = Err("check failed; auto-commit postponed".into());
    } else if generation != self.editor.save_gen {
      // Files changed while checking: check again.
      self.after_save(ctx);
    } else {
      self.auto_commit();
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

  fn absorb(&mut self) {
    if let Err(e) = self.editor.save_all() {
      self.status = Err(e);
      return;
    }
    let r = oplog::record(&self.repo, "absorb", || absorb::absorb(&self.repo, false));
    self.done(r.map(|p| format!("absorbed {} hunk(s); {} left", p.moves.len(), p.left)));
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
      ui.selectable_value(&mut self.mode, Mode::Editor, RichText::new("Editor").strong());
      ui.selectable_value(&mut self.mode, Mode::History, RichText::new("History").strong());
      ui.separator();
      ui.label(format!("branch: {}", self.branch));
      ui.separator();
      ui.add(
        egui::TextEdit::singleline(&mut self.message)
          .hint_text("commit message (optional)")
          .desired_width(260.0),
      );
      let dirty = self.editor.tabs.iter().any(|t| t.dirty());
      if ui
        .add_enabled(!self.changes.is_empty() || dirty, egui::Button::new("Commit"))
        .clicked()
      {
        self.commit(false);
      }
      if ui
        .add_enabled(!self.changes.is_empty() || dirty, egui::Button::new("Absorb"))
        .on_hover_text("Move edits into the unpushed commits that last touched those lines")
        .clicked()
      {
        self.absorb();
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
      if self.auto_lines > 0 {
        ui.label(
          RichText::new(format!("auto-commit at {} lines", self.auto_lines))
            .weak()
            .small(),
        )
        .on_hover_text("Set [editor] auto_commit_lines in .mig.toml (0 = off)");
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
      RichText::new("Ctrl/Cmd-click or Shift-click to select several; right-click for more")
        .small()
        .weak(),
    );
    let mut clicked = None;
    let mut action = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
      for (i, row) in self.rows.iter().enumerate() {
        let text = format!("{:>3}  {}  {}", i + 1, &row.oid.to_string()[..7], row.summary);
        let resp = ui
          .selectable_label(self.selected.contains(&i), RichText::new(text).monospace())
          .on_hover_text(&row.time);
        if resp.clicked() {
          clicked = Some(i);
        }
        resp.context_menu(|ui| {
          for (name, key) in [
            ("Reword…", "reword"),
            ("Split…", "split"),
            ("Fold into parent", "fold"),
            ("Drop", "drop"),
          ] {
            if ui.button(name).clicked() {
              action = Some((key, i));
              ui.close();
            }
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
      Some(("split", i)) => match split::files(&self.repo, i) {
        Ok(files) => {
          self.split = Some(SplitDialog {
            pos: i,
            files: files.into_iter().map(|f| (f, false)).collect(),
            message: String::new(),
          })
        }
        Err(e) => self.status = Err(format!("{e:#}")),
      },
      Some(("fold", i)) => {
        self.selected = BTreeSet::from([i]);
        self.fold_selected();
      }
      Some(("drop", i)) => self.drop_commit(i),
      _ => {}
    }
  }

  fn dialogs(&mut self, ctx: &egui::Context) {
    if let Some(blocked) = self.confirm_save.take() {
      let mut keep = true;
      let mut force = false;
      egui::Window::new("Commit blocked").collapsible(false).show(ctx, |ui| {
        findings_list(ui, &blocked.secrets);
        for (path, size) in &blocked.large {
          ui.label(RichText::new(format!("{path}: {} (too large)", guard::fmt_size(*size))).monospace());
        }
        ui.horizontal(|ui| {
          if ui.button("Cancel").clicked() {
            keep = false;
          }
          if ui.button(RichText::new("Commit anyway").color(Color32::RED)).clicked() {
            keep = false;
            force = true;
          }
        });
      });
      if force {
        self.commit(true);
      } else if keep {
        self.confirm_save = Some(blocked);
      }
    }
    if let Some(found) = self.confirm_sync.take() {
      let mut keep = true;
      let mut force = false;
      egui::Window::new("Push blocked").collapsible(false).show(ctx, |ui| {
        ui.label("Unpushed commits contain possible secrets. Remove them (e.g. Drop) before syncing.");
        findings_list(ui, &found);
        ui.horizontal(|ui| {
          if ui.button("OK").clicked() {
            keep = false;
          }
          if ui.button(RichText::new("Push anyway").color(Color32::RED)).clicked() {
            keep = false;
            force = true;
          }
        });
      });
      if force {
        self.sync(true);
      } else if keep {
        self.confirm_sync = Some(found);
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
    if let Some(mut d) = self.split.take() {
      let mut keep = true;
      let mut apply = false;
      egui::Window::new(format!("Split #{}", d.pos + 1))
        .collapsible(false)
        .show(ctx, |ui| {
          ui.label("Checked files go into the first commit; the rest stay in the second.");
          egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
            for (f, on) in &mut d.files {
              ui.checkbox(on, RichText::new(f.as_str()).monospace());
            }
          });
          ui.add(egui::TextEdit::singleline(&mut d.message).hint_text("first commit message (optional)"));
          ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
              keep = false;
            }
            if ui.button("Split").clicked() {
              apply = true;
            }
          });
        });
      if apply {
        let paths: Vec<String> = d.files.iter().filter(|(_, on)| *on).map(|(f, _)| f.clone()).collect();
        let msg = Some(d.message.trim()).filter(|m| !m.is_empty());
        let r = oplog::record(&self.repo, &format!("split {}", d.pos + 1), || {
          split::split_paths(&self.repo, d.pos, &paths, msg)
        });
        self.selected.clear();
        self.view = View::Changes;
        self.done(r.map(|_| format!("split #{}", d.pos + 1)));
      } else if keep {
        self.split = Some(d);
      }
    }
  }

  fn history(&mut self, ui: &mut egui::Ui) {
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
    self.poll_check(&ctx);
    ctx.request_repaint_after(if self.editor.checking() || self.editor.running() {
      Duration::from_millis(200)
    } else {
      REFRESH
    });

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
    match self.mode {
      Mode::History => self.history(ui),
      Mode::Editor => {
        if !self.findings.is_empty() {
          egui::Panel::top("editor-banner").show(ui, |ui| self.banner(ui));
        }
        let mut saved = false;
        for ev in self.editor.ui(ui) {
          match ev {
            editor::Event::Saved => saved = true,
            editor::Event::Error(e) => self.status = Err(e),
          }
        }
        if saved {
          self.after_save(&ctx);
        }
      }
    }
    self.dialogs(&ctx);
  }
}
