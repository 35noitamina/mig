use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, Color32, Key, Modifiers, RichText};
use egui_extras::syntax_highlighting::{self, CodeTheme};

const MAX_OPEN_BYTES: u64 = 2 * 1024 * 1024;
const MAX_OUTPUT: usize = 200_000;
const MAX_LISTED: usize = 1000;

pub struct Tab {
  pub path: String,
  pub text: String,
  saved: String,
}

impl Tab {
  pub fn dirty(&self) -> bool {
    self.text != self.saved
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
  pub check_cmd: Option<String>,
  pub run_cmd: Option<String>,
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
      check_cmd: None,
      run_cmd: None,
    }
  }

  pub fn set_files(&mut self, files: Vec<String>, changed: Vec<String>) {
    self.files = Node::build(&files);
    self.changed = changed.into_iter().collect();
    // Pick up changes made on disk (undo, drop, switch...) in tabs without unsaved edits.
    let root = self.root.clone();
    self.tabs.retain_mut(|t| {
      if t.dirty() {
        return true;
      }
      match std::fs::read_to_string(root.join(&t.path)) {
        Ok(s) => {
          if s != t.saved {
            t.text = s.clone();
            t.saved = s;
          }
          true
        }
        Err(_) => false,
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
    let text = if full.exists() {
      let meta = std::fs::metadata(&full).map_err(|e| e.to_string())?;
      if meta.len() > MAX_OPEN_BYTES {
        return Err(format!("{path} is too large to edit"));
      }
      let bytes = std::fs::read(&full).map_err(|e| e.to_string())?;
      if bytes.contains(&0) {
        return Err(format!("{path} looks like a binary file"));
      }
      String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8"))?
    } else {
      String::new()
    };
    self.tabs.push(Tab {
      path: path.to_string(),
      saved: text.clone(),
      text,
    });
    self.active = self.tabs.len() - 1;
    Ok(())
  }

  fn write(&mut self, i: usize) -> Result<bool, String> {
    let t = &mut self.tabs[i];
    if !t.dirty() && self.root.join(&t.path).exists() {
      return Ok(false);
    }
    let full = self.root.join(&t.path);
    if let Some(dir) = full.parent() {
      std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&full, &t.text).map_err(|e| format!("{}: {e}", t.path))?;
    t.saved = t.text.clone();
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
          if ui.button("+").clicked() && !self.new_file.trim().is_empty() {
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

    egui::Panel::bottom("output")
      .resizable(true)
      .default_size(180.0)
      .show(ui, |ui| {
        ui.horizontal(|ui| {
          ui.label(RichText::new("Output").strong());
          let label = if self.running() { "Running…" } else { "Run (F5)" };
          if ui.add_enabled(!self.running(), egui::Button::new(label)).clicked() {
            self.run_with_save(&ctx, &mut events);
          }
          if ui.add_enabled(self.running(), egui::Button::new("Stop")).clicked() {
            self.stop();
          }
          if ui.button("Clear").clicked() {
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
          if ui.small_button("x").clicked() {
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
      ui.separator();
      let tab = &mut self.tabs[self.active];
      let lang = Path::new(&tab.path)
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
      let theme = CodeTheme::from_memory(ui.ctx(), ui.style());
      let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let mut job = syntax_highlighting::highlight(ui.ctx(), ui.style(), &theme, buf.as_str(), &lang);
        job.wrap.max_width = f32::INFINITY;
        ui.ctx().fonts_mut(|f| f.layout_job(job))
      };
      egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal_top(|ui| {
          let lines = tab.text.lines().count().max(1) + usize::from(tab.text.ends_with('\n'));
          let nums: String = (1..=lines).map(|n| format!("{n}\n")).collect();
          ui.label(
            RichText::new(nums.trim_end())
              .monospace()
              .color(Color32::from_gray(110)),
          );
          let fill = ui.available_size();
          ui.add(
            egui::TextEdit::multiline(&mut tab.text)
              .code_editor()
              .frame(egui::Frame::NONE)
              .desired_width(f32::INFINITY)
              .min_size(fill)
              .lock_focus(true)
              .layouter(&mut layouter),
          );
        });
      });
    });
    events
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
