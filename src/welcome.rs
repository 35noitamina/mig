use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use eframe::egui::{self, Color32, RichText};

use crate::config::Kind;
use crate::guard::{self, SaveBlocked};
use crate::settings::{self, SettingsDialog};
use crate::setup::{self, CreateOptions, GithubOptions, Outcome, Preview};
use crate::{config, github, repo};

const MAX_RECENT: usize = 10;

fn recent_file() -> Option<PathBuf> {
  dirs::config_dir().map(|d| d.join("mig").join("recent.json"))
}

pub fn load_recent() -> Vec<PathBuf> {
  recent_file()
    .and_then(|p| std::fs::read_to_string(p).ok())
    .and_then(|s| serde_json::from_str::<Vec<PathBuf>>(&s).ok())
    .unwrap_or_default()
    .into_iter()
    .filter(|p| p.join(".git").exists())
    .collect()
}

pub fn remember(dir: &Path) {
  let Some(file) = recent_file() else { return };
  let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
  let mut list = load_recent();
  list.retain(|p| p != &dir);
  list.insert(0, dir);
  list.truncate(MAX_RECENT);
  if let Some(parent) = file.parent() {
    let _ = std::fs::create_dir_all(parent);
  }
  let _ = std::fs::write(file, serde_json::to_string_pretty(&list).unwrap_or_default());
}

// What the shell should open after the welcome screen.
pub struct Opened {
  pub dir: PathBuf,
  pub status: String,
  pub blocked: Option<SaveBlocked>,
}

struct InitDialog {
  dir: PathBuf,
  new_folder: bool,
  // None while the folder is still being scanned in the background.
  preview: Option<Result<Preview, String>>,
  preview_rx: Option<Receiver<Result<Preview, String>>>,
  creating: Option<Receiver<anyhow::Result<Outcome>>>,
  kind: Option<Kind>,
  gitignore: String,
  write_gitignore: bool,
  save: bool,
  message: String,
  github: bool,
  gh_name: String,
  private: bool,
  ssh: bool,
  token_ok: bool,
}

impl InitDialog {
  fn new(dir: PathBuf, new_folder: bool, ctx: &egui::Context) -> InitDialog {
    let kind = config::project_kind(&dir);
    let write_gitignore = !dir.join(".gitignore").exists();
    let (tx, rx) = mpsc::channel();
    let (scan_dir, ctx) = (dir.clone(), ctx.clone());
    std::thread::spawn(move || {
      let _ = tx.send(setup::preview(&scan_dir).map_err(|e| format!("{e:#}")));
      ctx.request_repaint();
    });
    InitDialog {
      gh_name: setup::default_name(&dir).replace(' ', "-"),
      dir,
      new_folder,
      preview: None,
      preview_rx: Some(rx),
      creating: None,
      kind,
      gitignore: if write_gitignore {
        setup::gitignore_for(kind)
      } else {
        String::new()
      },
      write_gitignore,
      save: true,
      message: "Initial commit".into(),
      github: false,
      private: true,
      ssh: false,
      token_ok: github::token().is_some(),
    }
  }
}

pub struct Welcome {
  path: String,
  name: String,
  error: Option<String>,
  info: Option<String>,
  recent: Vec<PathBuf>,
  init: Option<InitDialog>,
  settings: Option<SettingsDialog>,
}

impl Welcome {
  pub fn new(start: Option<PathBuf>, error: Option<String>) -> Welcome {
    let path = start
      .or_else(|| std::env::current_dir().ok())
      .map(|p| p.display().to_string())
      .unwrap_or_default();
    Welcome {
      info: None,
      path,
      name: String::new(),
      error,
      recent: load_recent(),
      init: None,
      settings: None,
    }
  }

  pub fn with_info(mut self, info: Option<String>) -> Welcome {
    self.info = info;
    self
  }

  fn dir(&self) -> PathBuf {
    PathBuf::from(self.path.trim())
  }

  pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<Opened> {
    let mut opened = None;
    let ctx = ui.ctx().clone();
    egui::CentralPanel::default_margins().show(ui, |ui| {
      egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        column(ui, 680.0, |ui| {
          if self.init.is_some() {
            opened = self.init_page(ui, &ctx);
          } else {
            opened = self.home(ui);
          }
        });
      });
    });
    opened
  }

  fn home(&mut self, ui: &mut egui::Ui) -> Option<Opened> {
    let mut opened = None;
    ui.add_space(36.0);
    ui.vertical_centered(|ui| {
      ui.label(RichText::new("mig").size(40.0).strong());
      ui.label(RichText::new("Open a repository, or create one").size(15.0).weak());
    });
    ui.add_space(24.0);
    card(ui, "Folder", |ui| {
      ui.horizontal(|ui| {
        let w = ui.available_width() - 90.0;
        ui.add(
          egui::TextEdit::singleline(&mut self.path)
            .desired_width(w)
            .margin(egui::vec2(8.0, 6.0)),
        );
        if ui
          .add(egui::Button::new("Browse…").min_size(egui::vec2(80.0, 30.0)))
          .clicked()
          && let Some(p) = rfd::FileDialog::new().set_directory(self.dir()).pick_folder()
        {
          self.path = p.display().to_string();
        }
      });
    });
    card(ui, "Open", |ui| {
      ui.label(RichText::new("Open the repository in the folder above.").weak());
      ui.add_space(6.0);
      if ui.add(primary("Open repository")).clicked() {
        match repo::open(&self.dir()) {
          Ok(r) => {
            opened = Some(Opened {
              dir: r.workdir().unwrap_or(&self.dir()).to_path_buf(),
              status: String::new(),
              blocked: None,
            })
          }
          Err(_) => {
            self.error = Some("This folder is not a repository yet. Use \"Make it a repository\" below.".into())
          }
        }
      }
    });
    card(ui, "Make it a repository", |ui| {
      ui.label(RichText::new("Turn the existing folder above into a repository, keeping its files.").weak());
      ui.add_space(6.0);
      if ui.add(secondary("Set up this folder…")).clicked() {
        let d = self.dir();
        if d.is_dir() {
          self.error = None;
          self.init = Some(InitDialog::new(d, false, ui.ctx()));
        } else {
          self.error = Some(format!("{} is not a folder", d.display()));
        }
      }
    });
    card(ui, "New repository", |ui| {
      ui.label(RichText::new("Create a new folder inside the folder above.").weak());
      ui.add_space(6.0);
      ui.horizontal(|ui| {
        ui.add(
          egui::TextEdit::singleline(&mut self.name)
            .desired_width(260.0)
            .hint_text("my-project")
            .margin(egui::vec2(8.0, 6.0)),
        );
        if ui
          .add_enabled(!self.name.trim().is_empty(), secondary("Create…"))
          .clicked()
        {
          let d = self.dir().join(self.name.trim());
          if d.exists() && std::fs::read_dir(&d).is_ok_and(|mut it| it.next().is_some()) {
            self.error = Some(format!("{} already exists and is not empty", d.display()));
          } else {
            self.error = None;
            self.init = Some(InitDialog::new(d, true, ui.ctx()));
          }
        }
      });
    });
    if let Some(e) = &self.error {
      notice(ui, ERROR, e);
    }
    if let Some(i) = &self.info {
      notice(ui, INFO, i);
    }
    if let Some(d) = &mut self.settings
      && !matches!(d.ui(ui.ctx()), settings::Outcome::Open)
    {
      self.settings = None;
    }
    if !self.recent.is_empty() {
      card(ui, "Recent", |ui| {
        for p in self.recent.clone() {
          let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
          ui.horizontal(|ui| {
            if ui.link(RichText::new(name).size(15.0)).clicked() {
              opened = Some(Opened {
                dir: p.clone(),
                status: String::new(),
                blocked: None,
              });
            }
            ui.label(RichText::new(p.display().to_string()).weak().small());
          });
        }
      });
    }
    ui.vertical_centered(|ui| {
      if ui.add(secondary("Settings…")).clicked() {
        self.settings = Some(SettingsDialog::new(None));
      }
    });
    opened
  }

  fn init_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) -> Option<Opened> {
    let mut d = self.init.take()?;
    if let Some(rx) = &d.preview_rx
      && let Ok(p) = rx.try_recv()
    {
      d.preview = Some(p);
      d.preview_rx = None;
    }
    let mut keep = true;
    let mut create = false;
    ui.add_space(20.0);
    ui.horizontal(|ui| {
      if ui
        .add_enabled(
          d.creating.is_none(),
          egui::Button::new("Back").min_size(egui::vec2(64.0, 28.0)),
        )
        .clicked()
      {
        keep = false;
      }
      ui.add_space(8.0);
      let title = if d.new_folder {
        "New repository"
      } else {
        "Make it a repository"
      };
      ui.label(RichText::new(title).size(24.0).strong());
    });
    ui.label(RichText::new(d.dir.display().to_string()).monospace().weak());
    ui.add_space(12.0);

    if let Some(e) = &self.error {
      notice(ui, ERROR, e);
    }
    card(ui, "Contents", |ui| match &d.preview {
      None => {
        ui.horizontal(|ui| {
          ui.spinner();
          ui.label("Scanning the folder…");
        });
      }
      Some(Err(e)) => notice(ui, ERROR, e),
      Some(Ok(p)) => {
        egui::Grid::new("stats")
          .num_columns(2)
          .spacing([24.0, 6.0])
          .show(ui, |ui| {
            ui.label(RichText::new("Project type").weak());
            ui.label(d.kind.map_or("Unknown", Kind::name));
            ui.end_row();
            ui.label(RichText::new("Files to add").weak());
            ui.label(format!("{} ({})", p.files, guard::fmt_size(p.bytes)));
            ui.end_row();
          });
        if p.already_repo {
          notice(
            ui,
            INFO,
            "Already a repository: only the options below will be applied.",
          );
        }
        if !p.secrets.is_empty() {
          let mut text = format!(
            "{} possible secret(s) found; the first commit will stop until they are removed.",
            p.secrets.len()
          );
          for f in p.secrets.iter().take(5) {
            text.push_str(&format!("\n{}:{}  {}  {}", f.path, f.line, f.rule, f.masked));
          }
          notice(ui, ERROR, &text);
        }
        if !p.large.is_empty() {
          let mut text = "Large files (the first commit will stop until they are removed or ignored):".to_string();
          for (path, size) in &p.large {
            text.push_str(&format!("\n{path}  {}", guard::fmt_size(*size)));
          }
          notice(ui, WARN, &text);
        }
      }
    });

    card(ui, ".gitignore", |ui| {
      if d.write_gitignore {
        ui.horizontal(|ui| {
          ui.label("Template");
          let before = d.kind;
          egui::ComboBox::from_id_salt("kind")
            .selected_text(d.kind.map_or("Generic", Kind::name))
            .show_ui(ui, |ui| {
              ui.selectable_value(&mut d.kind, None, "Generic");
              for k in [Kind::Rust, Kind::Node, Kind::Go, Kind::Python] {
                ui.selectable_value(&mut d.kind, Some(k), k.name());
              }
            });
          if d.kind != before {
            d.gitignore = setup::gitignore_for(d.kind);
          }
        });
        ui.add_space(4.0);
        ui.add(
          egui::TextEdit::multiline(&mut d.gitignore)
            .code_editor()
            .desired_rows(10)
            .desired_width(f32::INFINITY),
        );
      } else {
        ui.label(RichText::new("The folder already has a .gitignore; it will be kept.").weak());
      }
    });

    card(ui, "First commit", |ui| {
      ui.checkbox(&mut d.save, "Commit the files now");
      if d.save {
        ui.add(
          egui::TextEdit::singleline(&mut d.message)
            .desired_width(f32::INFINITY)
            .margin(egui::vec2(8.0, 6.0)),
        );
      }
    });

    card(ui, "GitHub", |ui| {
      ui.checkbox(&mut d.github, "Create the repository on GitHub and push");
      if d.github {
        ui.add_space(4.0);
        egui::Grid::new("gh")
          .num_columns(2)
          .spacing([16.0, 8.0])
          .show(ui, |ui| {
            ui.label("Name");
            ui.add(
              egui::TextEdit::singleline(&mut d.gh_name)
                .hint_text("name or org/name")
                .desired_width(280.0)
                .margin(egui::vec2(8.0, 6.0)),
            );
            ui.end_row();
            ui.label("Visibility");
            ui.horizontal(|ui| {
              ui.radio_value(&mut d.private, true, "Private");
              ui.radio_value(&mut d.private, false, "Public");
            });
            ui.end_row();
            ui.label("Remote URL");
            ui.horizontal(|ui| {
              ui.radio_value(&mut d.ssh, false, "HTTPS");
              ui.radio_value(&mut d.ssh, true, "SSH");
            });
            ui.end_row();
          });
        if !d.token_ok {
          notice(
            ui,
            WARN,
            "No GitHub token found. Set GITHUB_TOKEN or run `gh auth login`, then reopen this page.",
          );
        }
      }
    });

    ui.add_space(8.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      let ok = d.creating.is_none()
        && !matches!(d.preview, Some(Err(_)))
        && (!d.github || (d.token_ok && !d.gh_name.trim().is_empty()));
      let label = if d.new_folder {
        "Create repository"
      } else {
        "Make repository"
      };
      if ui.add_enabled(ok, primary(label)).clicked() {
        create = true;
      }
      if ui.add_enabled(d.creating.is_none(), secondary("Cancel")).clicked() {
        keep = false;
      }
      if d.creating.is_some() {
        ui.spinner();
        ui.label("working…");
      }
    });
    ui.add_space(24.0);

    if create {
      let opts = CreateOptions {
        dir: d.dir.clone(),
        gitignore: d.write_gitignore.then(|| d.gitignore.clone()),
        save: d.save,
        message: Some(d.message.clone()).filter(|m| !m.trim().is_empty()),
        force: false,
        remote: None,
        github: d.github.then(|| GithubOptions {
          name: d.gh_name.trim().to_string(),
          private: d.private,
          ssh: d.ssh,
        }),
      };
      // The first commit of a big folder and the GitHub round trip both take time.
      let (tx, rx) = mpsc::channel();
      let ctx = ctx.clone();
      std::thread::spawn(move || {
        let _ = tx.send(setup::create(opts));
        ctx.request_repaint();
      });
      d.creating = Some(rx);
    }
    if let Some(rx) = &d.creating
      && let Ok(r) = rx.try_recv()
    {
      d.creating = None;
      match r {
        Ok(out) => {
          let mut status = vec![if out.created {
            "created repository".to_string()
          } else {
            "opened repository".into()
          }];
          if let Some(g) = &out.github {
            status.push(format!("GitHub: {}", g.html_url));
          }
          if out.pushed > 0 {
            status.push(format!("pushed {} commit(s)", out.pushed));
          }
          if out.blocked.is_some() {
            status.push("first commit blocked".into());
          }
          status.extend(out.warnings);
          status.extend(out.notes);
          return Some(Opened {
            dir: out.dir,
            status: status.join(" · "),
            blocked: out.blocked,
          });
        }
        Err(e) => self.error = Some(format!("{e:#}")),
      }
    }
    if keep {
      self.init = Some(d);
    } else {
      self.error = None;
    }
    None
  }
}

const ERROR: Color32 = Color32::from_rgb(230, 100, 100);
const WARN: Color32 = Color32::from_rgb(230, 170, 80);
const INFO: Color32 = Color32::from_rgb(110, 170, 230);

// A centered column at most `width` wide.
fn column(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
  let w = ui.available_width().min(width);
  let pad = ((ui.available_width() - w) / 2.0).max(0.0);
  ui.horizontal(|ui| {
    ui.add_space(pad);
    ui.vertical(|ui| {
      ui.set_width(w);
      add(ui);
    });
  });
}

fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
  let v = ui.visuals();
  egui::Frame::new()
    .fill(v.faint_bg_color)
    .stroke(v.widgets.noninteractive.bg_stroke)
    .corner_radius(8.0)
    .inner_margin(14.0)
    .show(ui, |ui| {
      ui.set_width(ui.available_width());
      ui.label(RichText::new(title).size(16.0).strong());
      ui.add_space(6.0);
      add(ui);
    });
  ui.add_space(10.0);
}

fn notice(ui: &mut egui::Ui, color: Color32, text: &str) {
  egui::Frame::new()
    .fill(color.gamma_multiply(0.15))
    .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.6)))
    .corner_radius(6.0)
    .inner_margin(10.0)
    .show(ui, |ui| {
      ui.set_width(ui.available_width());
      ui.label(RichText::new(text).color(color));
    });
  ui.add_space(6.0);
}

fn primary(text: &str) -> egui::Button<'_> {
  egui::Button::new(RichText::new(text).strong().color(Color32::WHITE))
    .fill(Color32::from_rgb(45, 110, 200))
    .min_size(egui::vec2(140.0, 32.0))
}

fn secondary(text: &str) -> egui::Button<'_> {
  egui::Button::new(text).min_size(egui::vec2(100.0, 32.0))
}
