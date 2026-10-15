use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Color32, RichText};
use git2::{DiffOptions, Oid, Repository};

use crate::editor::{self, Editor};
use crate::guard::{self, SaveBlocked};
use crate::repo::Change;
use crate::scan::{Finding, SecretsFound};
use crate::settings::{self, Settings, SettingsDialog};
use crate::welcome::{self, Opened, Welcome};
use crate::{absorb, config, github, oplog, ops, repo, rev, scan, setup, split};

const HISTORY: usize = 300;
const REFRESH: Duration = Duration::from_secs(3);
const DIFF_LINES: usize = 5000;
const LIST_LIMIT: usize = 500;

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

struct PublishDialog {
  name: String,
  private: bool,
  ssh: bool,
  token_ok: bool,
}

struct App {
  repo: Repository,
  root: PathBuf,
  leave: bool,
  publish: Option<PublishDialog>,
  settings: Option<SettingsDialog>,
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
  ctx: egui::Context,
  loading: Option<Receiver<Result<Update>>>,
  scan_cache: Option<scan::Cache>,
  loaded: bool,
  refresh_again: bool,
  refresh_every: Duration,
  workdir_lines: Vec<(Color32, String)>,
  changed_lines: usize,
  auto_pending: bool,
  saving: Option<SaveJob>,
}

struct SaveJob {
  rx: Receiver<Result<ops::Saved>>,
  auto: bool,
  message: String,
}

// Pieces of the screen, computed off the UI thread and sent cheapest first.
enum Update {
  History {
    branch: String,
    rows: Vec<Row>,
  },
  Changes {
    changes: Vec<Change>,
    files: Vec<String>,
  },
  Details {
    findings: Vec<Finding>,
    workdir_lines: Vec<(Color32, String)>,
    changed_lines: Option<usize>,
    cache: scan::Cache,
  },
  Done(Duration),
}

fn load_snapshot(
  root: &Path,
  want_stats: bool,
  mut cache: scan::Cache,
  tx: &mpsc::Sender<Result<Update>>,
) -> Result<()> {
  let started = Instant::now();
  let repo = repo::open(root)?;
  let rows = rev::chain(&repo, HISTORY)?
    .iter()
    .map(|c| Row {
      oid: c.id(),
      summary: repo::summary(c),
      time: crate::cli::fmt_time(c.time().seconds()),
    })
    .collect();
  let branch = repo::branch_name(&repo).unwrap_or_else(|| "(detached)".into());
  let _ = tx.send(Ok(Update::History { branch, rows }));

  let changes = repo::changes(&repo)?;
  let files = tracked_files(&repo, &changes);
  let untracked: Vec<String> = changes
    .iter()
    .filter(|c| c.kind == '?')
    .map(|c| c.path.clone())
    .collect();
  let _ = tx.send(Ok(Update::Changes { changes, files }));

  let findings = scan::scan_pending_with(&repo, &untracked, &mut cache)?;
  let diff = workdir_diff(&repo)?;
  let workdir_lines = diff_lines(&diff, DIFF_LINES);
  // Line stats read every changed file, so only compute them when auto-commit asks.
  let changed_lines = want_stats.then(|| diff.stats().map(|s| s.insertions() + s.deletions()).unwrap_or(0));
  let _ = tx.send(Ok(Update::Details {
    findings,
    workdir_lines,
    changed_lines,
    cache,
  }));
  let _ = tx.send(Ok(Update::Done(started.elapsed())));
  Ok(())
}

fn line_color(l: &str) -> Color32 {
  if l.starts_with("diff --git") || l.starts_with("+++") || l.starts_with("---") {
    Color32::from_rgb(200, 200, 120)
  } else if l.starts_with('+') {
    Color32::from_rgb(110, 200, 110)
  } else if l.starts_with('-') {
    Color32::from_rgb(230, 110, 110)
  } else if l.starts_with("@@") {
    Color32::from_rgb(100, 180, 220)
  } else {
    Color32::GRAY
  }
}

// Render at most `max` lines; libgit2 stops generating the patch once the callback says so.
fn diff_lines(d: &git2::Diff<'_>, max: usize) -> Vec<(Color32, String)> {
  let mut out: Vec<(Color32, String)> = Vec::new();
  let _ = d.print(git2::DiffFormat::Patch, |_, _, line| {
    let prefix = match line.origin() {
      c @ ('+' | '-' | ' ') => c.to_string(),
      _ => String::new(),
    };
    let text = String::from_utf8_lossy(line.content());
    for l in format!("{prefix}{text}").lines() {
      out.push((line_color(l), l.to_string()));
    }
    out.len() < max
  });
  if out.len() >= max {
    out.truncate(max);
    out.push((Color32::GRAY, format!("… diff truncated after {max} lines")));
  }
  out
}

// Plain `mig gui` starts on the welcome screen; with `open` it goes straight to that
// folder's repository, falling back to the welcome screen when there is none.
pub fn run(dir: PathBuf, open: Option<PathBuf>) -> Result<()> {
  let start = open
    .as_ref()
    .and_then(|p| repo::open(p).ok())
    .and_then(|r| r.workdir().map(|w| w.to_path_buf()));
  let note = match (&open, &start) {
    (Some(p), None) => Some(format!(
      "{} is not a repository yet. Use \"Set up this folder…\" to make it one.",
      p.display()
    )),
    _ => None,
  };
  let dir = open.unwrap_or(dir);
  let opts = eframe::NativeOptions {
    viewport: egui::ViewportBuilder::default()
      .with_title("mig")
      .with_inner_size([1200.0, 780.0]),
    ..Default::default()
  };
  eframe::run_native(
    "mig",
    opts,
    Box::new(move |cc| {
      load_cjk_font(&cc.egui_ctx);
      Settings::load().apply(&cc.egui_ctx);
      egui_extras::install_image_loaders(&cc.egui_ctx);
      let mut shell = Shell {
        app: None,
        welcome: Welcome::new(Some(dir), None).with_info(note),
      };
      if let Some(root) = start {
        shell.open(
          &cc.egui_ctx,
          Opened {
            dir: root,
            status: String::new(),
            blocked: None,
          },
        );
      }
      Ok(Box::new(shell))
    }),
  )
  .map_err(|e| anyhow::anyhow!("{e}"))
}

struct Shell {
  app: Option<App>,
  welcome: Welcome,
}

impl Shell {
  fn open(&mut self, ctx: &egui::Context, o: Opened) {
    match App::open(&o.dir, ctx) {
      Ok(mut app) => {
        welcome::remember(&app.root);
        let name = app
          .root
          .file_name()
          .map(|n| n.to_string_lossy().into_owned())
          .unwrap_or_default();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("mig — {name}")));
        if !o.status.is_empty() {
          app.status = Ok(o.status);
        }
        app.confirm_save = o.blocked;
        self.app = Some(app);
      }
      Err(e) => self.welcome = Welcome::new(Some(o.dir), Some(format!("{e:#}"))),
    }
  }
}

impl eframe::App for Shell {
  fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    let ctx = ui.ctx().clone();
    if let Some(app) = &mut self.app {
      app.frame(ui, &ctx);
      if app.leave {
        let root = app.root.clone();
        self.app = None;
        self.welcome = Welcome::new(root.parent().map(Path::to_path_buf), None);
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("mig".into()));
      }
    } else if let Some(o) = self.welcome.ui(ui) {
      self.open(&ctx, o);
    }
  }
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
  // Files over 2 MB are treated as binary so big data files are never read whole.
  opts
    .include_untracked(true)
    .recurse_untracked_dirs(true)
    .show_untracked_content(true)
    .max_size(2 * 1024 * 1024);
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
  fn open(dir: &Path, ctx: &egui::Context) -> Result<App> {
    let repo = repo::open(dir)?;
    let root = repo.workdir().unwrap_or(dir).to_path_buf();
    let cfg = config::load_dir(&root).unwrap_or_default();
    let mut editor = Editor::new(root.clone());
    (editor.check_cmd, editor.run_cmd) = config::commands(&root, &cfg.editor);
    let mut app = App {
      repo,
      root,
      leave: false,
      publish: None,
      settings: None,
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
      ctx: ctx.clone(),
      loading: None,
      scan_cache: Some(scan::Cache::new()),
      loaded: false,
      refresh_again: false,
      refresh_every: REFRESH,
      workdir_lines: vec![],
      changed_lines: 0,
      auto_pending: false,
      saving: None,
    };
    app.refresh();
    Ok(app)
  }

  fn reload_config(&mut self) {
    let cfg = config::load_dir(&self.root).unwrap_or_default();
    self.auto_lines = cfg.editor.auto_commit_lines;
    (self.editor.check_cmd, self.editor.run_cmd) = config::commands(&self.root, &cfg.editor);
  }

  // Starts a background refresh; results are applied by poll_refresh as they arrive.
  fn refresh(&mut self) {
    self.last_refresh = Instant::now();
    if self.loading.is_some() {
      self.refresh_again = true;
      return;
    }
    let (tx, rx) = mpsc::channel();
    let root = self.root.clone();
    let ctx = self.ctx.clone();
    let want_stats = self.auto_pending;
    let cache = self.scan_cache.take().unwrap_or_default();
    std::thread::spawn(move || {
      if let Err(e) = load_snapshot(&root, want_stats, cache, &tx) {
        let _ = tx.send(Err(e));
      }
      ctx.request_repaint();
    });
    self.loading = Some(rx);
  }

  fn poll_refresh(&mut self, ctx: &egui::Context) {
    loop {
      let Some(rx) = &self.loading else { return };
      let update = match rx.try_recv() {
        Ok(u) => u,
        Err(mpsc::TryRecvError::Empty) => return,
        Err(mpsc::TryRecvError::Disconnected) => Ok(Update::Done(REFRESH)),
      };
      match update {
        Ok(Update::History { branch, rows }) => {
          self.branch = branch;
          self.rows = rows;
          self.selected.retain(|&i| i < self.rows.len());
          self.loaded = true;
        }
        Ok(Update::Changes { changes, files }) => {
          let changed = changes.iter().map(|c| c.path.clone()).collect();
          self.changes = changes;
          self.editor.set_files(files, changed);
        }
        Ok(Update::Details {
          findings,
          workdir_lines,
          changed_lines,
          cache,
        }) => {
          self.scan_cache = Some(cache);
          self.findings = findings;
          let fresh: Vec<&Finding> = self.findings.iter().filter(|f| !self.alerted.contains(f)).collect();
          if !fresh.is_empty() {
            notify(
              &format!("{} possible secret(s) in your changes", fresh.len()),
              &fresh[0].path,
            );
          }
          self.alerted = self.findings.clone();
          self.workdir_lines = workdir_lines;
          self.load_diff();
          if let Some(n) = changed_lines {
            self.changed_lines = n;
            if std::mem::take(&mut self.auto_pending) {
              self.maybe_auto_commit(ctx);
            }
          }
        }
        Ok(Update::Done(took)) => {
          // Slow trees get refreshed less often so the machine isn't kept busy.
          self.refresh_every = REFRESH.max(took * 4);
          self.finish_refresh();
        }
        Err(e) => {
          self.status = Err(format!("{e:#}"));
          self.finish_refresh();
        }
      }
    }
  }

  fn finish_refresh(&mut self) {
    self.loading = None;
    self.loaded = true;
    self.last_refresh = Instant::now();
    if self.scan_cache.is_none() {
      self.scan_cache = Some(scan::Cache::new());
    }
    if std::mem::take(&mut self.refresh_again) || self.auto_pending {
      self.refresh();
    }
  }

  fn load_diff(&mut self) {
    self.diff = match self.view {
      View::Changes => self.workdir_lines.clone(),
      View::Commit(i) => {
        let Some(row) = self.rows.get(i) else { return };
        let d = (|| -> Result<git2::Diff<'_>> {
          let c = self.repo.find_commit(row.oid)?;
          let parent = c.parents().next().map(|p| p.tree()).transpose()?;
          let mut opts = DiffOptions::new();
          opts.max_size(2 * 1024 * 1024);
          Ok(
            self
              .repo
              .diff_tree_to_tree(parent.as_ref(), Some(&c.tree()?), Some(&mut opts))?,
          )
        })();
        d.map(|d| diff_lines(&d, DIFF_LINES)).unwrap_or_default()
      }
    };
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
    self.start_save(msg, force, false);
  }

  // Saving a big tree can take a while, so it runs on its own thread with its own handle.
  fn start_save(&mut self, message: String, force: bool, auto: bool) {
    if self.saving.is_some() {
      return;
    }
    let (tx, rx) = mpsc::channel();
    let root = self.root.clone();
    let ctx = self.ctx.clone();
    let msg = message.clone();
    std::thread::spawn(move || {
      let r = repo::open(&root).and_then(|repo| {
        let text = if auto {
          format!(
            "auto: {}",
            ops::pending_summary(&repo).unwrap_or_else(|_| "Update".into())
          )
        } else {
          msg
        };
        let cmd = if auto { "save (auto)" } else { "save" };
        oplog::record(&repo, cmd, || ops::save(&repo, Some(&text), &[], force))
      });
      let _ = tx.send(r);
      ctx.request_repaint();
    });
    self.saving = Some(SaveJob { rx, auto, message });
    self.status = Ok(if auto {
      "auto-committing…".into()
    } else {
      "committing…".into()
    });
  }

  fn poll_save(&mut self) {
    let Some(job) = &self.saving else { return };
    let r = match job.rx.try_recv() {
      Ok(r) => r,
      Err(mpsc::TryRecvError::Empty) => return,
      Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("save failed")),
    };
    let job = self.saving.take().unwrap();
    match (r, job.auto) {
      (Ok(s), false) => {
        let mut text = format!("saved {}", repo::short(s.oid));
        for w in s.warnings {
          text.push_str(&format!(" · warning: {w}"));
        }
        self.done(Ok(text));
      }
      (Ok(s), true) => {
        let c = self
          .repo
          .find_commit(s.oid)
          .map(|c| repo::summary(&c))
          .unwrap_or_default();
        self
          .editor
          .output
          .push(&format!("[mig] auto-committed {} {c}\n", repo::short(s.oid)));
        self.done(Ok(format!("auto-committed {}", repo::short(s.oid))));
      }
      (Err(e), false) => {
        self.message = job.message;
        match e.downcast::<SaveBlocked>() {
          Ok(b) => {
            self.confirm_save = Some(b);
            self.status = Ok(String::new());
          }
          Err(e) => self.done(Err(e)),
        }
      }
      (Err(e), true) => {
        let text = format!("auto-commit skipped: {e:#}");
        self.editor.output.push(&format!("[mig] {text}\n"));
        if e.is::<SaveBlocked>() {
          notify("Auto-commit skipped", &e.to_string());
        }
        self.done(Err(anyhow::anyhow!(text)));
      }
    }
  }

  // Called after the editor writes files; the decision waits for fresh line counts.
  fn after_save(&mut self) {
    self.auto_pending = true;
    self.refresh();
  }

  fn maybe_auto_commit(&mut self, ctx: &egui::Context) {
    if self.auto_lines == 0 || self.editor.checking() || self.saving.is_some() {
      return;
    }
    if self.changed_lines < self.auto_lines {
      return;
    }
    match self.editor.check_cmd.clone() {
      Some(cmd) => self.editor.start_check(&cmd, ctx),
      None => self.start_save(String::new(), false, true),
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
      let _ = ctx;
      self.after_save();
    } else {
      self.start_save(String::new(), false, true);
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
      ui.menu_button("Repo", |ui| {
        if ui
          .button("Open / new repository…")
          .on_hover_text("Go back to the welcome screen")
          .clicked()
        {
          self.leave = true;
          ui.close();
        }
        let has_origin = self.repo.find_remote("origin").is_ok();
        if ui
          .add_enabled(!has_origin, egui::Button::new("Publish to GitHub…"))
          .on_hover_text("Create this repository on GitHub, add it as origin and push")
          .on_disabled_hover_text("origin is already set")
          .clicked()
        {
          self.publish = Some(PublishDialog {
            name: setup::default_name(&self.root).replace(' ', "-"),
            private: true,
            ssh: false,
            token_ok: github::token().is_some(),
          });
          ui.close();
        }
        ui.separator();
        match setup::remotes(&self.repo) {
          Ok(list) if !list.is_empty() => {
            for (name, url) in list {
              ui.label(RichText::new(format!("{name}  {url}")).monospace().small());
            }
          }
          _ => {
            ui.label(RichText::new("no remotes").weak());
          }
        }
      });
      if ui
        .button("Settings")
        .on_hover_text("Appearance, commit identity and this repository's options")
        .clicked()
      {
        self.settings = Some(SettingsDialog::new(Some(&self.root)));
      }
      ui.separator();
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
        .add_enabled(
          self.saving.is_none() && (!self.changes.is_empty() || dirty),
          egui::Button::new("Commit"),
        )
        .on_hover_text("Save open files and commit every change (mig save)")
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
      if ui
        .button("Sync")
        .on_hover_text("Fetch, put your commits on top of the remote and push (mig sync)")
        .clicked()
      {
        self.sync(false);
      }
      ui.separator();
      if ui
        .button("Undo")
        .on_hover_text("Undo the last operation (mig undo)")
        .clicked()
      {
        let r = oplog::undo(&self.repo, false).map(|op| format!("undid: {}", op.cmd));
        self.done(r);
      }
      if ui
        .button("Redo")
        .on_hover_text("Redo what was undone (mig redo)")
        .clicked()
      {
        let r = oplog::redo(&self.repo, false).map(|op| format!("redid: {}", op.cmd));
        self.done(r);
      }
      if ui
        .button("Refresh")
        .on_hover_text("Reload the working tree and history")
        .clicked()
      {
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
    egui::ScrollArea::vertical()
      .id_salt("changes")
      .max_height(180.0)
      .show(ui, |ui| {
        for c in self.changes.iter().take(LIST_LIMIT) {
          ui.label(RichText::new(format!("  {} {}", c.kind, c.path)).monospace().small());
        }
        if self.changes.len() > LIST_LIMIT {
          ui.label(
            RichText::new(format!("  …and {} more", self.changes.len() - LIST_LIMIT))
              .small()
              .weak(),
          );
        }
      });
    ui.separator();
    ui.horizontal(|ui| {
      ui.label(RichText::new("History").strong());
      let n = self.selected.len();
      let label = if n > 1 {
        format!("Fold {n}")
      } else {
        "Fold into parent".into()
      };
      if ui
        .add_enabled(n >= 1, egui::Button::new(label))
        .on_hover_text("Combine the selected commits (or one commit and its parent) into one")
        .on_disabled_hover_text("Select commits in the list first")
        .clicked()
      {
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
    egui::ScrollArea::vertical().id_salt("history").show(ui, |ui| {
      for (i, row) in self.rows.iter().enumerate() {
        let text = format!("{:>3}  {}  {}", i + 1, &row.oid.to_string()[..7], row.summary);
        let resp = ui
          .selectable_label(self.selected.contains(&i), RichText::new(text).monospace())
          .on_hover_text(&row.time);
        if resp.clicked() {
          clicked = Some(i);
        }
        resp.context_menu(|ui| {
          for (name, key, tip) in [
            ("Reword…", "reword", "Change this commit's message"),
            ("Split…", "split", "Split this commit in two by file"),
            ("Fold into parent", "fold", "Combine this commit with the one before it"),
            ("Drop", "drop", "Remove this commit from history"),
          ] {
            if ui.button(name).on_hover_text(tip).clicked() {
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

  fn publish_dialog(&mut self, ctx: &egui::Context) {
    let Some(mut d) = self.publish.take() else { return };
    let mut keep = true;
    let mut go = false;
    egui::Window::new("Publish to GitHub")
      .collapsible(false)
      .show(ctx, |ui| {
        ui.horizontal(|ui| {
          ui.label("Name");
          ui.add(egui::TextEdit::singleline(&mut d.name).hint_text("name or org/name"));
        });
        ui.checkbox(&mut d.private, "Private");
        ui.checkbox(&mut d.ssh, "Use SSH URL");
        if !d.token_ok {
          ui.label(
            RichText::new("No GitHub token found: set GITHUB_TOKEN or run `gh auth login`")
              .color(Color32::from_rgb(230, 170, 90)),
          );
        }
        ui.horizontal(|ui| {
          if ui.button("Cancel").on_hover_text("Close without publishing").clicked() {
            keep = false;
          }
          if ui
            .add_enabled(d.token_ok && !d.name.trim().is_empty(), egui::Button::new("Publish"))
            .on_hover_text("Create the GitHub repository and push")
            .clicked()
          {
            go = true;
          }
        });
      });
    if go {
      let gh = setup::GithubOptions {
        name: d.name.trim().to_string(),
        private: d.private,
        ssh: d.ssh,
      };
      let r = setup::create_github(&self.repo, &gh).map(|c| match setup::push_initial(&self.repo, false) {
        Ok(n) => format!("published {} · pushed {n} commit(s)", c.html_url),
        Err(e) => format!("published {} · {e}", c.html_url),
      });
      self.done(r);
    } else if keep {
      self.publish = Some(d);
    }
  }

  fn dialogs(&mut self, ctx: &egui::Context) {
    self.publish_dialog(ctx);
    if let Some(d) = &mut self.settings {
      match d.ui(ctx) {
        settings::Outcome::Open => {}
        settings::Outcome::Closed => self.settings = None,
        settings::Outcome::Saved => {
          self.settings = None;
          self.reload_config();
          self.status = Ok("settings saved".into());
          self.refresh();
        }
      }
    }
    if let Some(blocked) = self.confirm_save.take() {
      let mut keep = true;
      let mut force = false;
      egui::Window::new("Commit blocked").collapsible(false).show(ctx, |ui| {
        findings_list(ui, &blocked.secrets);
        for (path, size) in &blocked.large {
          ui.label(RichText::new(format!("{path}: {} (too large)", guard::fmt_size(*size))).monospace());
        }
        ui.horizontal(|ui| {
          if ui.button("Cancel").on_hover_text("Don't commit").clicked() {
            keep = false;
          }
          if ui
            .button(RichText::new("Commit anyway").color(Color32::RED))
            .on_hover_text("Commit even though problems were found")
            .clicked()
          {
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
          if ui.button("OK").on_hover_text("Don't push").clicked() {
            keep = false;
          }
          if ui
            .button(RichText::new("Push anyway").color(Color32::RED))
            .on_hover_text("Push even though possible secrets were found")
            .clicked()
          {
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
            if ui.button("Cancel").on_hover_text("Keep the old message").clicked() {
              keep = false;
            }
            if ui.button("Apply").on_hover_text("Save the new message").clicked() {
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
            if ui.button("Cancel").on_hover_text("Close without splitting").clicked() {
              keep = false;
            }
            if ui
              .button("Split")
              .on_hover_text("Split the commit with the checked files first")
              .clicked()
            {
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
      // Only the visible rows are laid out, so long diffs stay cheap.
      let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
      egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, row_h, self.diff.len(), |ui, range| {
          for (col, line) in &self.diff[range] {
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

impl App {
  fn frame(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
    let ctx = ctx.clone();
    self.poll_refresh(&ctx);
    self.poll_save();
    if self.loading.is_none() && self.last_refresh.elapsed() >= self.refresh_every {
      self.refresh();
    }
    self.poll_check(&ctx);
    ctx.request_repaint_after(
      if self.editor.checking() || self.editor.running() || self.saving.is_some() {
        Duration::from_millis(200)
      } else {
        REFRESH
      },
    );

    egui::Panel::top("toolbar").show(ui, |ui| {
      ui.add_space(4.0);
      self.toolbar(ui);
      ui.add_space(4.0);
    });
    egui::Panel::bottom("status").show(ui, |ui| {
      ui.horizontal(|ui| {
        if self.loading.is_some() || self.saving.is_some() {
          ui.spinner();
          if !self.loaded {
            ui.label("loading repository…");
          }
        }
        match &self.status {
          Ok(s) => {
            ui.label(s);
          }
          Err(e) => {
            ui.label(RichText::new(e).color(Color32::from_rgb(230, 110, 110)));
          }
        }
      });
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
          self.after_save();
        }
      }
    }
    self.dialogs(&ctx);
  }
}
