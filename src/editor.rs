use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::text::{CCursor, CCursorRange, LayoutSection};
use eframe::egui::{self, Color32, Key, Modifiers, RichText};
use egui_extras::syntax_highlighting::CodeTheme;

use crate::finder::{self, Hit, Query};
use crate::{highlight, viewer};

const MAX_OPEN_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 32 * 1024 * 1024;
const MAX_OUTPUT: usize = 200_000;
const MAX_LISTED: usize = 1000;

enum Body {
  Text { text: String, saved: String },
  // Read-only; `total` is the file size when only the start was loaded.
  Bytes { data: Arc<[u8]>, total: u64 },
}

pub struct Tab {
  pub path: String,
  body: Body,
  kind: Option<viewer::Kind>,
  // Show the dedicated viewer instead of the plain text / hex view.
  rich: bool,
  db: Option<viewer::DbView>,
}

impl Tab {
  pub fn dirty(&self) -> bool {
    matches!(&self.body, Body::Text { text, saved } if text != saved)
  }
}

#[derive(Clone)]
pub struct Output(Arc<Mutex<String>>);

impl Output {
  pub fn push(&self, s: &str) {
    let mut o = self.0.lock().unwrap();
    o.push_str(s);
    if o.len() > MAX_OUTPUT {
      let cut = o.len() - MAX_OUTPUT;
      let cut = (cut..o.len()).find(|&i| o.is_char_boundary(i)).unwrap_or(0);
      o.drain(..cut);
    }
  }

  fn text(&self) -> String {
    self.0.lock().unwrap().clone()
  }

  fn clear(&self) {
    self.0.lock().unwrap().clear();
  }
}

// A shell command running in the background; its output goes to the shared panel.
pub struct Job {
  child: Arc<Mutex<Option<Child>>>,
  result: Arc<Mutex<Option<bool>>>,
}

impl Job {
  fn spawn(root: &Path, cmd: &str, out: &Output, ctx: &egui::Context) -> Job {
    let result = Arc::new(Mutex::new(None));
    let child = Arc::new(Mutex::new(None));
    out.push(&format!("$ {cmd}\n"));
    let mut c = if cfg!(windows) {
      let mut c = Command::new("cmd");
      c.args(["/C", cmd]);
      c
    } else {
      let mut c = Command::new("sh");
      c.args(["-c", cmd]);
      c
    };
    c.current_dir(root)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());
    match c.spawn() {
      Err(e) => {
        out.push(&format!("failed to start: {e}\n"));
        *result.lock().unwrap() = Some(false);
      }
      Ok(mut ch) => {
        pipe(ch.stdout.take(), out.clone(), ctx.clone());
        pipe(ch.stderr.take(), out.clone(), ctx.clone());
        *child.lock().unwrap() = Some(ch);
        let (child, result, out, ctx) = (child.clone(), result.clone(), out.clone(), ctx.clone());
        std::thread::spawn(move || {
          loop {
            let status = match child.lock().unwrap().as_mut() {
              Some(ch) => ch.try_wait(),
              None => break,
            };
            match status {
              Ok(Some(st)) => {
                out.push(&format!(
                  "[exit: {}]\n",
                  st.code().map_or("killed".into(), |c| c.to_string())
                ));
                *result.lock().unwrap() = Some(st.success());
                break;
              }
              Ok(None) => std::thread::sleep(Duration::from_millis(100)),
              Err(e) => {
                out.push(&format!("[error: {e}]\n"));
                *result.lock().unwrap() = Some(false);
                break;
              }
            }
          }
          ctx.request_repaint();
        });
      }
    }
    Job { child, result }
  }

  fn finished(&self) -> Option<bool> {
    *self.result.lock().unwrap()
  }

  fn kill(&self) {
    if let Some(c) = self.child.lock().unwrap().as_mut() {
      let _ = c.kill();
    }
  }
}

fn pipe(src: Option<impl Read + Send + 'static>, out: Output, ctx: egui::Context) {
  let Some(src) = src else { return };
  std::thread::spawn(move || {
    let mut r = BufReader::new(src);
    let mut buf = Vec::new();
    while r.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
      out.push(&String::from_utf8_lossy(&buf));
      buf.clear();
      ctx.request_repaint();
    }
  });
}

pub enum Event {
  Saved,
  Error(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Bottom {
  Output,
  Search,
}

// Ctrl+F: search inside the open file.
#[derive(Default)]
struct FindBar {
  open: bool,
  q: Query,
  matches: Vec<Range<usize>>,
  current: usize,
  // What `matches` was computed for: the query and a hash of the text.
  stamp: u64,
  last_query: Query,
  error: Option<String>,
  focus: bool,
  // Scroll to the current match on the next frame.
  reveal: bool,
}

impl FindBar {
  fn refresh(&mut self, text: &str, caret: usize) {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    self.q.text.hash(&mut h);
    self.q.case.hash(&mut h);
    self.q.regex.hash(&mut h);
    text.hash(&mut h);
    let stamp = h.finish();
    if stamp == self.stamp {
      return;
    }
    self.stamp = stamp;
    let changed = self.q != self.last_query;
    self.last_query = self.q.clone();
    match self.q.compile() {
      Ok(Some(re)) => {
        self.matches = finder::find_all(text, &re);
        self.error = None;
      }
      Ok(None) => {
        self.matches.clear();
        self.error = None;
      }
      Err(e) => {
        self.matches.clear();
        self.error = Some(e);
      }
    }
    if changed {
      self.current = self.matches.iter().position(|m| m.start >= caret).unwrap_or(0);
      self.reveal = !self.matches.is_empty();
    }
    self.current = self.current.min(self.matches.len().saturating_sub(1));
  }

  fn step(&mut self, forward: bool) {
    let n = self.matches.len();
    if n == 0 {
      return;
    }
    self.current = if forward {
      (self.current + 1) % n
    } else {
      (self.current + n - 1) % n
    };
    self.reveal = true;
  }
}

// Ctrl+Shift+F: search in every file of the repository.
#[derive(Default)]
struct SearchPanel {
  q: Query,
  glob: String,
  job: Option<finder::Search>,
  hits: Vec<Hit>,
  rows: Vec<SearchRow>,
  status: Option<Result<String, String>>,
  focus: bool,
}

enum SearchRow {
  File(String, usize),
  Hit(usize),
}

pub struct Editor {
  root: PathBuf,
  files: Node,
  changed: HashSet<String>,
  pub tabs: Vec<Tab>,
  active: usize,
  new_file: String,
  pub output: Output,
  run: Option<Job>,
  check: Option<(Job, u64)>,
  pub save_gen: u64,
  md_cache: egui_commonmark::CommonMarkCache,
  pub check_cmd: Option<String>,
  pub run_cmd: Option<String>,
  find_close: bool,
  bottom: Bottom,
  find: FindBar,
  search: SearchPanel,
  // A place to select and show in the open file, set by a search result.
  // The number is how many more frames to keep asking: a click elsewhere in the same moment can
  // take the focus away again, so one request is not always enough.
  select: Option<(String, Range<usize>, u8)>,
}

impl Editor {
  pub fn new(root: PathBuf) -> Editor {
    Editor {
      root,
      files: Node::default(),
      changed: HashSet::new(),
      tabs: vec![],
      active: 0,
      new_file: String::new(),
      output: Output(Arc::new(Mutex::new(String::new()))),
      run: None,
      check: None,
      save_gen: 0,
      md_cache: Default::default(),
      check_cmd: None,
      run_cmd: None,
      find_close: false,
      bottom: Bottom::Output,
      find: FindBar::default(),
      search: SearchPanel::default(),
      select: None,
    }
  }

  pub fn set_files(&mut self, files: Vec<String>, changed: Vec<String>) {
    self.files = Node::build(&files);
    self.changed = changed.into_iter().collect();
    // Pick up changes made on disk (undo, drop, switch...) in tabs without unsaved edits.
    let root = self.root.clone();
    self.tabs.retain_mut(|t| {
      let full = root.join(&t.path);
      match &mut t.body {
        Body::Text { text, saved } if text == saved => match std::fs::read_to_string(&full) {
          Ok(s) => {
            if s != *saved {
              *text = s.clone();
              *saved = s;
            }
            true
          }
          Err(_) => false,
        },
        Body::Text { .. } => true,
        Body::Bytes { .. } => full.exists(),
      }
    });
    self.active = self.active.min(self.tabs.len().saturating_sub(1));
  }

  pub fn open(&mut self, path: &str) -> Result<(), String> {
    if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
      self.active = i;
      return Ok(());
    }
    let full = self.root.join(path);
    let (body, head) = if full.exists() {
      let total = std::fs::metadata(&full).map_err(|e| e.to_string())?.len();
      let mut data = Vec::new();
      std::fs::File::open(&full)
        .and_then(|f| f.take(MAX_BINARY_BYTES).read_to_end(&mut data))
        .map_err(|e| format!("{path}: {e}"))?;
      let head = data[..data.len().min(16)].to_vec();
      // Small, NUL-free UTF-8 is edited as text; everything else is shown read-only.
      let text = (total <= MAX_OPEN_BYTES && !data.contains(&0))
        .then(|| String::from_utf8(data.clone()).ok())
        .flatten();
      let body = match text {
        Some(t) => Body::Text {
          saved: t.clone(),
          text: t,
        },
        None => Body::Bytes {
          data: data.into(),
          total,
        },
      };
      (body, head)
    } else {
      (
        Body::Text {
          text: String::new(),
          saved: String::new(),
        },
        vec![],
      )
    };
    let kind = viewer::Kind::detect(path, &head);
    self.tabs.push(Tab {
      path: path.to_string(),
      body,
      kind,
      rich: kind.is_some(),
      db: None,
    });
    self.active = self.tabs.len() - 1;
    Ok(())
  }

  fn write(&mut self, i: usize) -> Result<bool, String> {
    let full = self.root.join(&self.tabs[i].path);
    let t = &mut self.tabs[i];
    let Body::Text { text, saved } = &mut t.body else {
      return Ok(false);
    };
    if text == saved && full.exists() {
      return Ok(false);
    }
    if let Some(dir) = full.parent() {
      std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&full, &*text).map_err(|e| format!("{}: {e}", t.path))?;
    *saved = text.clone();
    Ok(true)
  }

  pub fn save_all(&mut self) -> Result<bool, String> {
    let mut any = false;
    for i in 0..self.tabs.len() {
      any |= self.write(i)?;
    }
    if any {
      self.save_gen += 1;
    }
    Ok(any)
  }

  fn save_active(&mut self) -> Result<bool, String> {
    if self.tabs.is_empty() {
      return Ok(false);
    }
    let saved = self.write(self.active)?;
    if saved {
      self.save_gen += 1;
    }
    Ok(saved)
  }

  pub fn running(&self) -> bool {
    self.run.as_ref().is_some_and(|j| j.finished().is_none())
  }

  pub fn checking(&self) -> bool {
    self.check.is_some()
  }

  pub fn start_run(&mut self, ctx: &egui::Context) {
    if self.running() {
      return;
    }
    match self.run_cmd.clone() {
      Some(cmd) => self.run = Some(Job::spawn(&self.root, &cmd, &self.output, ctx)),
      None => self.output.push("no run command; set [editor] run in .mig.toml\n"),
    }
  }

  pub fn stop(&mut self) {
    if let Some(j) = &self.run {
      j.kill();
    }
  }

  pub fn start_check(&mut self, cmd: &str, ctx: &egui::Context) {
    self.output.push("[mig] checking before auto-commit\n");
    self.check = Some((Job::spawn(&self.root, cmd, &self.output, ctx), self.save_gen));
  }

  // Some((passed, save generation the check ran against)) once the check is done.
  pub fn poll_check(&mut self) -> Option<(bool, u64)> {
    let ok = self.check.as_ref()?.0.finished()?;
    let (_, generation) = self.check.take().unwrap();
    Some((ok, generation))
  }

  pub fn ui(&mut self, ui: &mut egui::Ui) -> Vec<Event> {
    let mut events = Vec::new();
    let ctx = ui.ctx().clone();
    let (save, run, stop) = ctx.input_mut(|i| {
      (
        i.consume_key(Modifiers::COMMAND, Key::S),
        i.consume_key(Modifiers::NONE, Key::F5),
        i.consume_key(Modifiers::SHIFT, Key::F5),
      )
    });
    // The Shift variant goes first: a plain Ctrl+F pattern would also match Ctrl+Shift+F.
    let (find_all, find_here, next, prev, esc) = ctx.input_mut(|i| {
      (
        i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::F),
        i.consume_key(Modifiers::COMMAND, Key::F),
        i.consume_key(Modifiers::NONE, Key::F3),
        i.consume_key(Modifiers::SHIFT, Key::F3),
        self.find.open && i.consume_key(Modifiers::NONE, Key::Escape),
      )
    });
    if find_all {
      let text = self.selected_text(&ctx);
      self.bottom = Bottom::Search;
      self.search.focus = true;
      if let Some(t) = text {
        self.search.q.text = t;
      }
    }
    if find_here && !find_all && self.text_tab_for_find() {
      if let Some(t) = self.selected_text(&ctx) {
        self.find.q.text = t;
      }
      self.find.open = true;
      self.find.focus = true;
    }
    if self.find.open {
      if next {
        self.find.step(true);
      }
      if prev {
        self.find.step(false);
      }
    }
    if esc {
      self.close_find(&ctx);
    }
    self.poll_search(&ctx);
    if save {
      match self.save_active() {
        Ok(true) => events.push(Event::Saved),
        Ok(false) => {}
        Err(e) => events.push(Event::Error(e)),
      }
    }
    if stop {
      self.stop();
    }
    if run {
      self.run_with_save(&ctx, &mut events);
    }

    egui::Panel::left("files")
      .resizable(true)
      .default_size(220.0)
      .show(ui, |ui| {
        ui.horizontal(|ui| {
          ui.add(
            egui::TextEdit::singleline(&mut self.new_file)
              .hint_text("new file path")
              .desired_width(150.0),
          );
          if ui
            .button("+")
            .on_hover_text("Open this file, creating it on save if it doesn't exist")
            .clicked()
            && !self.new_file.trim().is_empty()
          {
            let p = self.new_file.trim().replace('\\', "/");
            self.new_file.clear();
            if let Err(e) = self.open(&p) {
              events.push(Event::Error(e));
            }
          }
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
          if let Some(p) = self.tree(ui, &self.files)
            && let Err(e) = self.open(&p)
          {
            events.push(Event::Error(e));
          }
        });
      });

    let mut open_hit: Option<Hit> = None;
    egui::Panel::bottom("output")
      .resizable(true)
      .default_size(180.0)
      .show(ui, |ui| {
        ui.horizontal(|ui| {
          ui.selectable_value(&mut self.bottom, Bottom::Output, RichText::new("Output").strong());
          let found = self.search.hits.len();
          let label = if found > 0 {
            format!("Search ({found})")
          } else {
            "Search".to_string()
          };
          ui.selectable_value(&mut self.bottom, Bottom::Search, RichText::new(label).strong())
            .on_hover_text("Search the whole repository (Ctrl+Shift+F)");
        });
        if self.bottom == Bottom::Search {
          open_hit = self.search_ui(ui);
          return;
        }
        ui.horizontal(|ui| {
          let label = if self.running() { "Running…" } else { "Run (F5)" };
          if ui
            .add_enabled(!self.running(), egui::Button::new(label))
            .on_hover_text("Save all files and run the run command (F5)")
            .clicked()
          {
            self.run_with_save(&ctx, &mut events);
          }
          if ui
            .add_enabled(self.running(), egui::Button::new("Stop"))
            .on_hover_text("Stop the running command (Shift+F5)")
            .clicked()
          {
            self.stop();
          }
          if ui.button("Clear").on_hover_text("Clear the output").clicked() {
            self.output.clear();
          }
          if self.checking() {
            ui.spinner();
            ui.label("checking…");
          }
          let cmd = self.run_cmd.as_deref().unwrap_or("(no run command)");
          ui.label(RichText::new(cmd).weak().monospace());
        });
        egui::ScrollArea::vertical()
          .stick_to_bottom(true)
          .auto_shrink([false, false])
          .show(ui, |ui| {
            ui.label(RichText::new(self.output.text()).monospace().size(12.0));
          });
      });

    if let Some(h) = open_hit {
      match self.open(&h.path) {
        Ok(()) => {
          if let Some(t) = self.tabs.get_mut(self.active)
            && t.kind == Some(viewer::Kind::Markdown)
          {
            t.rich = false;
          }
          self.select = Some((h.path, h.at, 3));
        }
        Err(e) => events.push(Event::Error(e)),
      }
    }

    egui::CentralPanel::default_margins().show(ui, |ui| {
      if self.tabs.is_empty() {
        ui.centered_and_justified(|ui| ui.label(RichText::new("Open a file from the list").weak()));
        return;
      }
      let mut close = None;
      ui.horizontal_wrapped(|ui| {
        for (i, t) in self.tabs.iter().enumerate() {
          let name = t.path.rsplit('/').next().unwrap_or(&t.path);
          let label = if t.dirty() {
            format!("● {name}")
          } else {
            name.to_string()
          };
          if ui
            .selectable_label(i == self.active, label)
            .on_hover_text(&t.path)
            .clicked()
          {
            self.active = i;
          }
          if ui.small_button("x").on_hover_text("Close this tab").clicked() {
            close = Some(i);
          }
          ui.separator();
        }
      });
      if let Some(i) = close {
        self.tabs.remove(i);
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        return;
      }
      let root = self.root.clone();
      let tab = &mut self.tabs[self.active];
      let text_view = matches!(tab.body, Body::Text { .. }) && (!tab.rich || tab.kind == Some(viewer::Kind::Markdown));
      if self.find.open && text_view {
        tab.rich = false;
      }
      let find_active = self.find.open && matches!(tab.body, Body::Text { .. }) && !tab.rich;
      if find_active && let Body::Text { text, .. } = &tab.body {
        let caret = caret_byte(ui.ctx(), &tab.path, text);
        self.find.refresh(text, caret);
        find_bar(ui, &mut self.find, &mut self.find_close);
      }
      ui.horizontal(|ui| {
        if let Some(k) = tab.kind {
          ui.selectable_value(&mut tab.rich, false, "Plain")
            .on_hover_text("Show the raw contents (text or hex)");
          ui.selectable_value(&mut tab.rich, true, k.label())
            .on_hover_text(match k {
              viewer::Kind::Markdown => "Show the rendered Markdown",
              viewer::Kind::Sqlite => "Browse tables and run read-only queries",
              viewer::Kind::Image => "Show the image",
            });
          ui.separator();
        }
        if let Body::Bytes { total, .. } = &tab.body {
          ui.label(RichText::new(format!("binary, read-only · {}", crate::guard::fmt_size(*total))).weak());
        }
      });
      ui.separator();
      let path = tab.path.clone();
      match (tab.rich, tab.kind, &mut tab.body) {
        (true, Some(viewer::Kind::Markdown), Body::Text { text, .. }) => {
          viewer::markdown(ui, &mut self.md_cache, text);
        }
        (true, Some(viewer::Kind::Sqlite), _) => {
          tab
            .db
            .get_or_insert_with(|| viewer::DbView::open(&root.join(&path)))
            .ui(ui);
        }
        (true, Some(viewer::Kind::Image), Body::Bytes { data, .. }) => viewer::image(ui, &path, data),
        (_, _, Body::Bytes { data, total }) => {
          let truncated = (*total > data.len() as u64).then_some(*total);
          viewer::hex(ui, data, truncated);
        }
        (_, _, Body::Text { text, .. }) => {
          let select = match &mut self.select {
            Some((p, r, n)) if *p == path => {
              *n -= 1;
              let r = r.clone();
              if *n == 0 {
                self.select = None;
              }
              ui.ctx().request_repaint();
              Some(r)
            }
            _ => None,
          };
          let reveal = (find_active && std::mem::take(&mut self.find.reveal))
            .then(|| self.find.matches.get(self.find.current).cloned())
            .flatten();
          let overlay = Overlay {
            matches: if find_active { &self.find.matches } else { &[] },
            current: self.find.current,
            select,
            reveal,
          };
          code_editor(ui, &path, text, overlay);
        }
      }
    });
    if std::mem::take(&mut self.find_close) {
      self.close_find(&ctx);
    }
    events
  }

  fn text_tab_for_find(&self) -> bool {
    self
      .tabs
      .get(self.active)
      .is_some_and(|t| matches!(t.body, Body::Text { .. }) && (!t.rich || t.kind == Some(viewer::Kind::Markdown)))
  }

  fn selected_text(&self, ctx: &egui::Context) -> Option<String> {
    let t = self.tabs.get(self.active)?;
    let Body::Text { text, .. } = &t.body else { return None };
    let st = egui::TextEdit::load_state(ctx, code_id(&t.path))?;
    let r = st.cursor.char_range()?;
    let (p, q): (usize, usize) = (r.primary.index.into(), r.secondary.index.into());
    let (a, b) = (p.min(q), p.max(q));
    let picked: String = text.chars().skip(a).take(b - a).collect();
    (!picked.is_empty() && !picked.contains('\n') && picked.chars().count() <= 200).then_some(picked)
  }

  // Closing the bar leaves the current match selected in the editor.
  fn close_find(&mut self, ctx: &egui::Context) {
    if !self.find.open {
      return;
    }
    self.find.open = false;
    let range = self.find.matches.get(self.find.current).cloned();
    if let (Some(range), Some(t)) = (range, self.tabs.get(self.active)) {
      self.select = Some((t.path.clone(), range, 3));
    }
    ctx.request_repaint();
  }

  fn poll_search(&mut self, ctx: &egui::Context) {
    let Some(job) = &self.search.job else { return };
    let mut done = None;
    while let Ok(m) = job.rx.try_recv() {
      match m {
        finder::Msg::Hits(h) => {
          if let Some(first) = h.first() {
            self.search.rows.push(SearchRow::File(first.path.clone(), h.len()));
          }
          for hit in h {
            self.search.rows.push(SearchRow::Hit(self.search.hits.len()));
            self.search.hits.push(hit);
          }
        }
        finder::Msg::Done { files, capped } => done = Some((files, capped)),
      }
    }
    match done {
      Some((files, capped)) => {
        self.search.job = None;
        let n = self.search.hits.len();
        self.search.status = Some(Ok(if n == 0 {
          "No results".to_string()
        } else {
          format!(
            "{n} result(s) in {files} file(s){}",
            if capped {
              format!("; stopped at {}", finder::MAX_HITS)
            } else {
              String::new()
            }
          )
        }));
      }
      None => ctx.request_repaint_after(Duration::from_millis(60)),
    }
  }

  fn start_search(&mut self) {
    self.search.hits.clear();
    self.search.rows.clear();
    self.search.job = None;
    let open: HashMap<String, String> = self
      .tabs
      .iter()
      .filter_map(|t| match &t.body {
        Body::Text { text, .. } => Some((t.path.clone(), text.clone())),
        Body::Bytes { .. } => None,
      })
      .collect();
    match finder::search(&self.root, &self.search.q, &self.search.glob, open) {
      Ok(job) => {
        self.search.job = Some(job);
        self.search.status = Some(Ok("Searching…".into()));
      }
      Err(e) if e == "empty query" => self.search.status = None,
      Err(e) => self.search.status = Some(Err(e)),
    }
  }

  // The Search tab of the bottom panel; returns the result that was clicked.
  fn search_ui(&mut self, ui: &mut egui::Ui) -> Option<Hit> {
    let mut go = false;
    ui.horizontal(|ui| {
      let id = egui::Id::new("repo-search-input");
      let r = ui.add(
        egui::TextEdit::singleline(&mut self.search.q.text)
          .id(id)
          .hint_text("Search in all files")
          .desired_width(260.0),
      );
      if std::mem::take(&mut self.search.focus) {
        r.request_focus();
        select_all(ui.ctx(), id, &self.search.q.text);
      }
      go |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
      ui.toggle_value(&mut self.search.q.case, "Aa")
        .on_hover_text("Match case");
      ui.toggle_value(&mut self.search.q.regex, ".*")
        .on_hover_text("Regular expression");
      ui.add(
        egui::TextEdit::singleline(&mut self.search.glob)
          .hint_text("files, e.g. *.rs")
          .desired_width(120.0),
      )
      .on_hover_text("Only files whose path matches this pattern (Enter to search)");
      go |= ui.button("Search").clicked();
      if self.search.job.is_some() {
        ui.spinner();
      }
      match &self.search.status {
        Some(Ok(s)) => {
          ui.label(RichText::new(s).weak());
        }
        Some(Err(e)) => {
          ui.label(RichText::new(e).color(Color32::from_rgb(230, 110, 110)));
        }
        None => {}
      }
    });
    if go {
      self.start_search();
    }
    let mut clicked = None;
    let font = egui::FontId::monospace(12.0);
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
    let weak = ui.visuals().weak_text_color();
    let text = ui.visuals().text_color();
    let mark = if ui.visuals().dark_mode {
      Color32::from_rgb(110, 90, 20)
    } else {
      Color32::from_rgb(255, 235, 150)
    };
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
      ui,
      row_h,
      self.search.rows.len(),
      |ui, range| {
        for row in &self.search.rows[range] {
          match row {
            SearchRow::File(path, n) => {
              ui.label(RichText::new(format!("{path}  ({n})")).strong());
            }
            SearchRow::Hit(i) => {
              let h = &self.search.hits[*i];
              let mut job = egui::text::LayoutJob::default();
              let fmt = |color: Color32, bg: Color32| egui::TextFormat {
                font_id: font.clone(),
                color,
                background: bg,
                ..Default::default()
              };
              job.append(&format!("  {:>5}  ", h.line), 0.0, fmt(weak, Color32::TRANSPARENT));
              let s = &h.snippet;
              let (a, b) = (h.span.start.min(s.len()), h.span.end.min(s.len()));
              if s.is_char_boundary(a) && s.is_char_boundary(b) {
                job.append(&s[..a], 0.0, fmt(text, Color32::TRANSPARENT));
                job.append(&s[a..b], 0.0, fmt(text, mark));
                job.append(&s[b..], 0.0, fmt(text, Color32::TRANSPARENT));
              } else {
                job.append(s, 0.0, fmt(text, Color32::TRANSPARENT));
              }
              let r = ui.add(egui::Label::new(job).sense(egui::Sense::click()).truncate());
              if r.on_hover_text(format!("{}:{}", h.path, h.line)).clicked() {
                clicked = Some(h.clone());
              }
            }
          }
        }
      },
    );
    clicked
  }

  fn run_with_save(&mut self, ctx: &egui::Context, events: &mut Vec<Event>) {
    match self.save_all() {
      Ok(true) => events.push(Event::Saved),
      Ok(false) => {}
      Err(e) => {
        events.push(Event::Error(e));
        return;
      }
    }
    self.start_run(ctx);
  }

  // Draws a directory level; returns a file path when one is clicked.
  // Collapsed folders draw nothing, so huge trees cost only what is open.
  fn tree(&self, ui: &mut egui::Ui, node: &Node) -> Option<String> {
    let mut clicked = None;
    for (name, child) in &node.dirs {
      let r = egui::CollapsingHeader::new(name.as_str())
        .id_salt(&child.path)
        .show(ui, |ui| self.tree(ui, child));
      if let Some(Some(p)) = r.body_returned {
        clicked = Some(p);
      }
    }
    for (name, full) in node.files.iter().take(MAX_LISTED) {
      let open = self.tabs.get(self.active).is_some_and(|t| &t.path == full);
      let mut text = RichText::new(name.as_str());
      if self.changed.contains(full) {
        text = text.color(Color32::from_rgb(220, 180, 90));
      }
      if ui.selectable_label(open, text).clicked() {
        clicked = Some(full.clone());
      }
    }
    if node.files.len() > MAX_LISTED {
      ui.label(RichText::new(format!("…and {} more files", node.files.len() - MAX_LISTED)).weak());
    }
    clicked
  }
}

#[derive(Default)]
struct Node {
  path: String,
  dirs: BTreeMap<String, Node>,
  // (name, full path)
  files: Vec<(String, String)>,
}

impl Node {
  fn build(paths: &[String]) -> Node {
    let mut root = Node::default();
    for p in paths {
      let mut node = &mut root;
      let mut parts = p.split('/').peekable();
      while let Some(part) = parts.next() {
        if parts.peek().is_none() {
          node.files.push((part.to_string(), p.clone()));
        } else {
          let path = format!("{}{part}/", node.path);
          node = node.dirs.entry(part.to_string()).or_insert_with(|| Node {
            path,
            ..Default::default()
          });
        }
      }
    }
    root
  }
}

fn code_id(path: &str) -> egui::Id {
  egui::Id::new(("code-editor", path))
}

fn select_all(ctx: &egui::Context, id: egui::Id, text: &str) {
  if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
    st.cursor.set_char_range(Some(CCursorRange::two(
      CCursor::new(0),
      CCursor::new(text.chars().count()),
    )));
    st.store(ctx, id);
  }
}

// Where the caret is, as a byte offset into `text`.
fn caret_byte(ctx: &egui::Context, path: &str, text: &str) -> usize {
  let Some(r) = egui::TextEdit::load_state(ctx, code_id(path)).and_then(|s| s.cursor.char_range()) else {
    return 0;
  };
  text
    .char_indices()
    .nth(usize::from(r.primary.index).min(usize::from(r.secondary.index)))
    .map_or(text.len(), |(i, _)| i)
}

fn char_index(text: &str, byte: usize) -> usize {
  let mut b = byte.min(text.len());
  while !text.is_char_boundary(b) {
    b -= 1;
  }
  text[..b].chars().count()
}

// The Ctrl+F bar above the editor.
fn find_bar(ui: &mut egui::Ui, f: &mut FindBar, close: &mut bool) {
  ui.horizontal(|ui| {
    let id = egui::Id::new("find-input");
    let r = ui.add(
      egui::TextEdit::singleline(&mut f.q.text)
        .id(id)
        .hint_text("Find")
        .desired_width(240.0),
    );
    if std::mem::take(&mut f.focus) {
      r.request_focus();
      select_all(ui.ctx(), id, &f.q.text);
    }
    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
      let back = ui.input(|i| i.modifiers.shift);
      f.step(!back);
      r.request_focus();
    }
    ui.toggle_value(&mut f.q.case, "Aa").on_hover_text("Match case");
    ui.toggle_value(&mut f.q.regex, ".*")
      .on_hover_text("Regular expression");
    if ui
      .small_button("‹")
      .on_hover_text("Previous match (Shift+F3)")
      .clicked()
    {
      f.step(false);
    }
    if ui.small_button("›").on_hover_text("Next match (F3, Enter)").clicked() {
      f.step(true);
    }
    match (&f.error, f.q.text.is_empty(), f.matches.len()) {
      (Some(e), _, _) => {
        ui.label(RichText::new(format!("invalid: {e}")).color(Color32::from_rgb(230, 110, 110)));
      }
      (None, true, _) => {}
      (None, false, 0) => {
        ui.label(RichText::new("No results").weak());
      }
      (None, false, n) => {
        ui.label(RichText::new(format!("{}/{n}", f.current + 1)).weak());
      }
    }
    if ui.small_button("x").on_hover_text("Close (Esc)").clicked() {
      *close = true;
    }
  });
}

struct Overlay<'a> {
  matches: &'a [Range<usize>],
  current: usize,
  // Select this range and move the focus to the editor.
  select: Option<Range<usize>>,
  // Scroll to this range without taking the focus.
  reveal: Option<Range<usize>>,
}

// Paints a background behind every match; the current one is stronger.
fn mark_matches(job: &mut egui::text::LayoutJob, matches: &[Range<usize>], current: usize, dark: bool) {
  use egui::text::ByteIndex;
  let (soft, strong) = if dark {
    (Color32::from_rgb(88, 74, 24), Color32::from_rgb(170, 110, 20))
  } else {
    (Color32::from_rgb(255, 238, 170), Color32::from_rgb(255, 190, 90))
  };
  let valid = |m: &Range<usize>| {
    m.end <= job.text.len() && job.text.is_char_boundary(m.start) && job.text.is_char_boundary(m.end)
  };
  let list: Vec<(Range<usize>, bool)> = matches
    .iter()
    .enumerate()
    .filter(|(_, m)| valid(m))
    .map(|(i, m)| (m.clone(), i == current))
    .collect();
  if list.is_empty() {
    return;
  }
  let old = std::mem::take(&mut job.sections);
  let mut out = Vec::with_capacity(old.len() + list.len() * 2);
  let mut mi = 0;
  for sec in old {
    let (first, end) = (sec.byte_range.start.0, sec.byte_range.end.0);
    let mut at = first;
    while at < end {
      while mi < list.len() && list[mi].0.end <= at {
        mi += 1;
      }
      let (stop, bg) = match list.get(mi) {
        Some((m, cur)) if m.start <= at => (m.end.min(end), Some(if *cur { strong } else { soft })),
        Some((m, _)) if m.start < end => (m.start, None),
        _ => (end, None),
      };
      let mut format = sec.format.clone();
      if let Some(b) = bg {
        format.background = b;
      }
      out.push(LayoutSection {
        leading_space: if at == first { sec.leading_space } else { 0.0 },
        byte_range: ByteIndex(at)..ByteIndex(stop),
        format,
      });
      at = stop;
    }
  }
  job.sections = out;
}

fn code_editor(ui: &mut egui::Ui, path: &str, text: &mut String, overlay: Overlay) {
  let first_line = text.lines().next().unwrap_or("").to_string();
  let syntax = highlight::syntax_name(path, &first_line);
  let theme = CodeTheme::from_memory(ui.ctx(), ui.style());
  let dark = ui.visuals().dark_mode;
  let (matches, current) = (overlay.matches, overlay.current);
  let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
    let mut job = highlight::layout(ui, &theme, buf.as_str(), syntax.as_deref());
    job.wrap.max_width = f32::INFINITY;
    mark_matches(&mut job, matches, current, dark);
    ui.ctx().fonts_mut(|f| f.layout_job(job))
  };
  let id = code_id(path);
  if let Some(range) = &overlay.select {
    let (a, b) = (char_index(text, range.start), char_index(text, range.end));
    let mut st = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
    st.cursor
      .set_char_range(Some(CCursorRange::two(CCursor::new(a), CCursor::new(b))));
    st.store(ui.ctx(), id);
    ui.memory_mut(|m| m.request_focus(id));
  }
  let target = overlay.select.clone().or(overlay.reveal.clone());
  egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
    ui.horizontal_top(|ui| {
      let lines = text.lines().count().max(1) + usize::from(text.ends_with('\n'));
      let nums: String = (1..=lines).map(|n| format!("{n}\n")).collect();
      ui.label(
        RichText::new(nums.trim_end())
          .monospace()
          .color(Color32::from_gray(110)),
      );
      let fill = ui.available_size();
      let out = egui::TextEdit::multiline(text)
        .id(id)
        .code_editor()
        .frame(egui::Frame::NONE)
        .desired_width(f32::INFINITY)
        .min_size(fill)
        .lock_focus(true)
        .layouter(&mut layouter)
        .show(ui);
      if let Some(range) = target {
        let at = CCursor::new(char_index(text, range.start));
        let rect = out.galley.pos_from_cursor(at).translate(out.galley_pos.to_vec2());
        ui.scroll_to_rect(rect.expand2(egui::vec2(0.0, 40.0)), Some(egui::Align::Center));
      }
    });
  });
}
