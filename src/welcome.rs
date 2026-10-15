use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, RichText};

use crate::config::Kind;
use crate::guard::{self, SaveBlocked};
use crate::setup::{self, CreateOptions, GithubOptions, Preview};
use crate::{github, repo};

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
  preview: Result<Preview, String>,
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
  fn new(dir: PathBuf, new_folder: bool) -> InitDialog {
    let preview = setup::preview(&dir).map_err(|e| format!("{e:#}"));
    let kind = preview.as_ref().ok().and_then(|p| p.kind);
    let (gitignore, write_gitignore) = match &preview {
      Ok(Preview { gitignore: Some(g), .. }) => (g.clone(), true),
      Ok(_) => (String::new(), false),
      Err(_) => (setup::gitignore_for(kind), true),
    };
    InitDialog {
      gh_name: setup::default_name(&dir).replace(' ', "-"),
      dir,
      new_folder,
      preview,
      kind,
      gitignore,
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
  recent: Vec<PathBuf>,
  init: Option<InitDialog>,
}

impl Welcome {
  pub fn new(start: Option<PathBuf>, error: Option<String>) -> Welcome {
    let path = start
      .or_else(|| std::env::current_dir().ok())
      .map(|p| p.display().to_string())
      .unwrap_or_default();
    Welcome {
      path,
      name: String::new(),
      error,
      recent: load_recent(),
      init: None,
    }
  }

  fn dir(&self) -> PathBuf {
    PathBuf::from(self.path.trim())
  }

  pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<Opened> {
    let mut opened = None;
    egui::CentralPanel::default_margins().show(ui, |ui| {
      ui.vertical_centered(|ui| {
        ui.add_space(40.0);
        ui.heading(RichText::new("mig").size(32.0).strong());
        ui.label(RichText::new("Open a repository, or create one").weak());
        ui.add_space(24.0);
      });
      ui.horizontal(|ui| {
        ui.label("Folder");
        ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(520.0));
        if ui.button("Browse…").clicked()
          && let Some(p) = rfd::FileDialog::new().set_directory(self.dir()).pick_folder()
        {
          self.path = p.display().to_string();
        }
      });
      ui.add_space(8.0);
      ui.horizontal(|ui| {
        if ui.button("Open repository").clicked() {
          match repo::open(&self.dir()) {
            Ok(r) => {
              opened = Some(Opened {
                dir: r.workdir().unwrap_or(&self.dir()).to_path_buf(),
                status: String::new(),
                blocked: None,
              })
            }
            Err(_) => {
              self.error = Some("not a repository; use \"Make this folder a repository\"".into());
            }
          }
        }
        if ui.button("Make this folder a repository…").clicked() {
          let d = self.dir();
          if d.is_dir() {
            self.init = Some(InitDialog::new(d, false));
          } else {
            self.error = Some(format!("{} is not a folder", d.display()));
          }
        }
      });
      ui.add_space(8.0);
      ui.horizontal(|ui| {
        ui.label("New repository named");
        ui.add(
          egui::TextEdit::singleline(&mut self.name)
            .desired_width(200.0)
            .hint_text("my-project"),
        );
        ui.label("inside the folder above");
        if ui
          .add_enabled(!self.name.trim().is_empty(), egui::Button::new("Create…"))
          .clicked()
        {
          let d = self.dir().join(self.name.trim());
          if d.exists() && std::fs::read_dir(&d).is_ok_and(|mut it| it.next().is_some()) {
            self.error = Some(format!("{} already exists and is not empty", d.display()));
          } else {
            self.init = Some(InitDialog::new(d, true));
          }
        }
      });
      if let Some(e) = &self.error {
        ui.add_space(8.0);
        ui.label(RichText::new(e).color(Color32::from_rgb(230, 110, 110)));
      }
      if !self.recent.is_empty() {
        ui.add_space(24.0);
        ui.label(RichText::new("Recent").strong());
        for p in self.recent.clone() {
          if ui.link(p.display().to_string()).clicked() {
            opened = Some(Opened {
              dir: p,
              status: String::new(),
              blocked: None,
            });
          }
        }
      }
    });
    if let Some(o) = self.init_dialog(ui.ctx()) {
      opened = Some(o);
    }
    opened
  }

  fn init_dialog(&mut self, ctx: &egui::Context) -> Option<Opened> {
    let mut d = self.init.take()?;
    let mut keep = true;
    let mut create = false;
    let title = if d.new_folder {
      "New repository"
    } else {
      "Make a repository"
    };
    egui::Window::new(title)
      .collapsible(false)
      .resizable(true)
      .show(ctx, |ui| {
        ui.label(RichText::new(d.dir.display().to_string()).monospace());
        match &d.preview {
          Err(e) => {
            ui.label(RichText::new(e).color(Color32::from_rgb(230, 110, 110)));
          }
          Ok(p) => {
            if p.already_repo {
              ui.label("Already a repository; only the options below will be applied.");
            }
            if !d.new_folder {
              ui.label(format!(
                "{} · {} file(s), {}",
                d.kind.map_or("Unknown project type", Kind::name),
                p.files,
                guard::fmt_size(p.bytes)
              ));
            }
            for f in p.secrets.iter().take(5) {
              ui.label(
                RichText::new(format!("⚠ {}:{}  {}  {}", f.path, f.line, f.rule, f.masked))
                  .color(Color32::from_rgb(230, 110, 110)),
              );
            }
            for (path, size) in &p.large {
              ui.label(
                RichText::new(format!("⚠ {path} is {}", guard::fmt_size(*size))).color(Color32::from_rgb(230, 170, 90)),
              );
            }
          }
        }
        ui.separator();
        if d.write_gitignore {
          ui.horizontal(|ui| {
            ui.label(".gitignore template");
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
          egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
            ui.add(
              egui::TextEdit::multiline(&mut d.gitignore)
                .code_editor()
                .desired_width(f32::INFINITY),
            );
          });
        } else {
          ui.label(".gitignore already exists and will be kept.");
        }
        ui.separator();
        ui.checkbox(&mut d.save, "Make the first commit");
        if d.save {
          ui.add(egui::TextEdit::singleline(&mut d.message).desired_width(300.0));
        }
        ui.checkbox(&mut d.github, "Create it on GitHub and push");
        if d.github {
          ui.horizontal(|ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut d.gh_name).hint_text("name or org/name"));
            ui.checkbox(&mut d.private, "Private");
            ui.checkbox(&mut d.ssh, "Use SSH URL");
          });
          if !d.token_ok {
            ui.label(
              RichText::new("No GitHub token found: set GITHUB_TOKEN or run `gh auth login`")
                .color(Color32::from_rgb(230, 170, 90)),
            );
          }
        }
        ui.separator();
        ui.horizontal(|ui| {
          if ui.button("Cancel").clicked() {
            keep = false;
          }
          let ok = d.preview.is_ok() && (!d.github || (d.token_ok && !d.gh_name.trim().is_empty()));
          if ui.add_enabled(ok, egui::Button::new("Create")).clicked() {
            create = true;
          }
        });
      });
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
      match setup::create(opts) {
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
        Err(e) => {
          self.error = Some(format!("{e:#}"));
          return None;
        }
      }
    }
    if keep {
      self.init = Some(d);
    }
    None
  }
}
