use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eframe::egui::{self, Color32, FontId, RichText, TextStyle, ThemePreference};
use serde::{Deserialize, Serialize};

use crate::config::{self, CONFIG_FILE};
use std::sync::mpsc::{self, Receiver};

use crate::ai::{self, AiConfig};
use crate::github;

// App-wide preferences, kept in the user's config folder.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Settings {
  pub theme: Theme,
  pub ui_scale: f32,
  pub code_font_size: f32,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
  Dark,
  Light,
  System,
}

impl Default for Settings {
  fn default() -> Self {
    Settings {
      theme: Theme::Dark,
      ui_scale: 1.0,
      code_font_size: 13.0,
    }
  }
}

fn settings_file() -> Option<PathBuf> {
  dirs::config_dir().map(|d| d.join("mig").join("settings.toml"))
}

impl Settings {
  pub fn load() -> Settings {
    settings_file()
      .and_then(|p| std::fs::read_to_string(p).ok())
      .and_then(|s| toml::from_str(&s).ok())
      .unwrap_or_default()
  }

  fn save(&self) -> Result<()> {
    let p = settings_file().context("no config folder on this system")?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(p, toml::to_string_pretty(self)?)?;
    Ok(())
  }

  pub fn apply(&self, ctx: &egui::Context) {
    ctx.set_theme(match self.theme {
      Theme::Dark => ThemePreference::Dark,
      Theme::Light => ThemePreference::Light,
      Theme::System => ThemePreference::System,
    });
    ctx.set_zoom_factor(self.ui_scale.clamp(0.5, 3.0));
    let size = self.code_font_size.clamp(8.0, 32.0);
    ctx.all_styles_mut(|s| {
      s.text_styles.insert(TextStyle::Monospace, FontId::monospace(size));
    });
  }
}

fn global_gitconfig() -> Result<git2::Config> {
  let path = match git2::Config::find_global() {
    Ok(p) => p,
    Err(_) => {
      let p = dirs::home_dir().context("no home folder")?.join(".gitconfig");
      if !p.exists() {
        std::fs::write(&p, "")?;
      }
      p
    }
  };
  Ok(git2::Config::open(&path)?)
}

// Fields of the repository's .mig.toml, edited as text.
#[derive(Clone, PartialEq)]
struct RepoFields {
  auto_commit_lines: String,
  check: String,
  run: String,
  max_file_mb: String,
  ignore_paths: String,
  allow: String,
}

impl RepoFields {
  fn load(root: &Path) -> RepoFields {
    let c = config::load_dir(root).unwrap_or_default();
    RepoFields {
      auto_commit_lines: c.editor.auto_commit_lines.to_string(),
      check: c.editor.check.unwrap_or_default(),
      run: c.editor.run.unwrap_or_default(),
      max_file_mb: c.save.max_file_mb.to_string(),
      ignore_paths: c.scan.ignore_paths.join("\n"),
      allow: c.scan.allow.join("\n"),
    }
  }

  // Writes only the fields that changed, so the rest of the file (and its comments) stays as is.
  fn save(&self, root: &Path, orig: &RepoFields) -> Result<()> {
    if self == orig {
      return Ok(());
    }
    let lines: i64 = self
      .auto_commit_lines
      .trim()
      .parse()
      .context("auto-commit lines must be a whole number")?;
    let mb: f64 = self
      .max_file_mb
      .trim()
      .parse()
      .context("file size limit must be a number")?;
    let path = root.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
      .parse()
      .with_context(|| format!("{CONFIG_FILE} is not valid TOML"))?;
    let list = |v: &str| -> toml_edit::Array { v.lines().map(str::trim).filter(|l| !l.is_empty()).collect() };
    let text_or_none = |v: &str| (!v.trim().is_empty()).then(|| toml_edit::value(v.trim()));
    let changes: Vec<(&str, &str, Option<toml_edit::Item>)> = [
      (
        "editor",
        "auto_commit_lines",
        self.auto_commit_lines != orig.auto_commit_lines,
        Some(toml_edit::value(lines.max(0))),
      ),
      ("editor", "check", self.check != orig.check, text_or_none(&self.check)),
      ("editor", "run", self.run != orig.run, text_or_none(&self.run)),
      (
        "save",
        "max_file_mb",
        self.max_file_mb != orig.max_file_mb,
        Some(toml_edit::value(mb.max(0.0))),
      ),
      (
        "scan",
        "ignore_paths",
        self.ignore_paths != orig.ignore_paths,
        Some(toml_edit::value(list(&self.ignore_paths))),
      ),
      (
        "scan",
        "allow",
        self.allow != orig.allow,
        Some(toml_edit::value(list(&self.allow))),
      ),
    ]
    .into_iter()
    .filter(|(_, _, changed, _)| *changed)
    .map(|(t, k, _, v)| (t, k, v))
    .collect();
    for (table, key, value) in changes {
      if !doc.contains_key(table) {
        doc[table] = toml_edit::table();
      }
      let t = doc[table]
        .as_table_like_mut()
        .with_context(|| format!("[{table}] in {CONFIG_FILE} is not a table"))?;
      match value {
        Some(v) => {
          t.insert(key, v);
        }
        None => {
          t.remove(key);
        }
      }
    }
    std::fs::write(&path, doc.to_string())?;
    Ok(())
  }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
  Appearance,
  Identity,
  Repository,
  Ai,
  GitHub,
}

struct AiFields {
  config: AiConfig,
  key: String,
  test: Option<Receiver<Result<String, String>>>,
  test_result: Option<Result<String, String>>,
}

pub struct SettingsDialog {
  tab: Tab,
  settings: Settings,
  name: String,
  email: String,
  local_identity: Option<String>,
  // (root, edited fields, fields as loaded)
  repo: Option<(PathBuf, RepoFields, RepoFields)>,
  token_ok: bool,
  ai: AiFields,
  error: Option<String>,
}

pub enum Outcome {
  Open,
  Closed,
  // Saved; the repository settings may have changed.
  Saved,
}

impl SettingsDialog {
  pub fn new(root: Option<&Path>) -> SettingsDialog {
    let cfg = git2::Config::open_default().ok();
    let get = |k: &str| cfg.as_ref().and_then(|c| c.get_string(k).ok()).unwrap_or_default();
    let local_identity = root.and_then(|r| git2::Repository::open(r).ok()).and_then(|r| {
      let local = r.config().ok()?.open_level(git2::ConfigLevel::Local).ok()?;
      let n = local.get_string("user.name").ok();
      let e = local.get_string("user.email").ok();
      (n.is_some() || e.is_some()).then(|| format!("{} <{}>", n.unwrap_or_default(), e.unwrap_or_default()))
    });
    SettingsDialog {
      tab: Tab::Appearance,
      settings: Settings::load(),
      name: get("user.name"),
      email: get("user.email"),
      local_identity,
      repo: root.map(|r| (r.to_path_buf(), RepoFields::load(r), RepoFields::load(r))),
      token_ok: github::token().is_some(),
      ai: AiFields {
        config: AiConfig::load(),
        key: String::new(),
        test: None,
        test_result: None,
      },
      error: None,
    }
  }

  fn save(&mut self, ctx: &egui::Context) -> Result<()> {
    self.settings.save()?;
    self.settings.apply(ctx);
    let mut git = global_gitconfig()?;
    for (key, v) in [("user.name", &self.name), ("user.email", &self.email)] {
      if !v.trim().is_empty() && git.get_string(key).ok().as_deref() != Some(v.trim()) {
        git.set_str(key, v.trim())?;
      }
    }
    if let Some((root, fields, orig)) = &self.repo {
      fields.save(root, orig)?;
    }
    self.ai.config.save()?;
    if !self.ai.key.trim().is_empty() {
      ai::store_key(&self.ai.config.provider, Some(&self.ai.key))?;
      self.ai.key.clear();
    }
    Ok(())
  }

  pub fn ui(&mut self, ctx: &egui::Context) -> Outcome {
    let mut outcome = Outcome::Open;
    let mut open = true;
    egui::Window::new("Settings")
      .collapsible(false)
      .resizable(false)
      .default_width(560.0)
      .open(&mut open)
      .show(ctx, |ui| {
        ui.horizontal(|ui| {
          for (tab, label) in [
            (Tab::Appearance, "Appearance"),
            (Tab::Identity, "Identity"),
            (Tab::Repository, "This repository"),
            (Tab::Ai, "AI"),
            (Tab::GitHub, "GitHub"),
          ] {
            if tab == Tab::Repository && self.repo.is_none() {
              continue;
            }
            ui.selectable_value(&mut self.tab, tab, RichText::new(label).size(15.0));
          }
        });
        ui.separator();
        ui.add_space(4.0);
        ui.vertical(|ui| {
          ui.set_min_height(240.0);
          match self.tab {
            Tab::Appearance => self.appearance(ui),
            Tab::Identity => self.identity(ui),
            Tab::Repository => self.repository(ui),
            Tab::Ai => self.ai_tab(ui),
            Tab::GitHub => self.github(ui),
          }
        });
        ui.add_space(8.0);
        if let Some(e) = &self.error {
          ui.label(RichText::new(e).color(Color32::from_rgb(230, 100, 100)));
        }
        ui.separator();
        ui.horizontal(|ui| {
          ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let save = egui::Button::new(RichText::new("Save").strong().color(Color32::WHITE))
              .fill(Color32::from_rgb(45, 110, 200))
              .min_size(egui::vec2(100.0, 30.0));
            if ui.add(save).on_hover_text("Save and apply").clicked() {
              match self.save(ctx) {
                Ok(()) => outcome = Outcome::Saved,
                Err(e) => self.error = Some(format!("{e:#}")),
              }
            }
            if ui
              .add(egui::Button::new("Cancel").min_size(egui::vec2(80.0, 30.0)))
              .on_hover_text("Close without saving")
              .clicked()
            {
              outcome = Outcome::Closed;
            }
          });
        });
      });
    if !open {
      outcome = Outcome::Closed;
    }
    if matches!(outcome, Outcome::Closed) {
      // Undo any live preview of appearance changes.
      Settings::load().apply(ctx);
    }
    outcome
  }

  fn appearance(&mut self, ui: &mut egui::Ui) {
    let before = self.settings.clone();
    egui::Grid::new("appearance")
      .num_columns(2)
      .spacing([24.0, 12.0])
      .show(ui, |ui| {
        ui.label("Theme");
        ui.horizontal(|ui| {
          ui.radio_value(&mut self.settings.theme, Theme::Dark, "Dark");
          ui.radio_value(&mut self.settings.theme, Theme::Light, "Light");
          ui.radio_value(&mut self.settings.theme, Theme::System, "Follow system");
        });
        ui.end_row();
        ui.label("Interface scale");
        ui.add(
          egui::Slider::new(&mut self.settings.ui_scale, 0.75..=2.0)
            .step_by(0.05)
            .suffix("×"),
        );
        ui.end_row();
        ui.label("Code font size");
        ui.add(
          egui::Slider::new(&mut self.settings.code_font_size, 9.0..=24.0)
            .step_by(1.0)
            .suffix(" pt"),
        );
        ui.end_row();
      });
    if self.settings != before {
      self.settings.apply(ui.ctx());
    }
  }

  fn identity(&mut self, ui: &mut egui::Ui) {
    ui.label(RichText::new("Used as the author of your commits (git's global user.name / user.email).").weak());
    ui.add_space(6.0);
    egui::Grid::new("identity")
      .num_columns(2)
      .spacing([24.0, 10.0])
      .show(ui, |ui| {
        ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(320.0));
        ui.end_row();
        ui.label("Email");
        ui.add(egui::TextEdit::singleline(&mut self.email).desired_width(320.0));
        ui.end_row();
      });
    let email = self.email.trim();
    if !email.is_empty() && !(email.contains('@') && email.contains('.')) {
      ui.add_space(4.0);
      ui.label(
        RichText::new("This doesn't look like an email address; GitHub won't link these commits to your account.")
          .color(Color32::from_rgb(230, 170, 80)),
      );
    }
    if let Some(local) = &self.local_identity {
      ui.add_space(4.0);
      ui.label(
        RichText::new(format!("This repository overrides it with {local} (in .git/config)."))
          .color(Color32::from_rgb(110, 170, 230)),
      );
    }
  }

  fn repository(&mut self, ui: &mut egui::Ui) {
    let Some((_, f, _)) = &mut self.repo else { return };
    ui.label(RichText::new(format!("Stored in {CONFIG_FILE} at the top of the repository.")).weak());
    ui.add_space(6.0);
    egui::Grid::new("repo")
      .num_columns(2)
      .spacing([24.0, 10.0])
      .show(ui, |ui| {
        ui.label("Auto-commit after");
        ui.horizontal(|ui| {
          ui.add(egui::TextEdit::singleline(&mut f.auto_commit_lines).desired_width(70.0));
          ui.label(RichText::new("changed lines (0 = off)").weak());
        });
        ui.end_row();
        ui.label("Check command");
        ui.add(
          egui::TextEdit::singleline(&mut f.check)
            .hint_text("guess from the project")
            .desired_width(340.0),
        );
        ui.end_row();
        ui.label("Run command");
        ui.add(
          egui::TextEdit::singleline(&mut f.run)
            .hint_text("guess from the project")
            .desired_width(340.0),
        );
        ui.end_row();
        ui.label("Largest file");
        ui.horizontal(|ui| {
          ui.add(egui::TextEdit::singleline(&mut f.max_file_mb).desired_width(70.0));
          ui.label(RichText::new("MB per file before a commit is stopped").weak());
        });
        ui.end_row();
        ui.label("Skip secret scan in");
        ui.add(
          egui::TextEdit::multiline(&mut f.ignore_paths)
            .hint_text("one glob per line, e.g. tests/fixtures/**")
            .desired_rows(3)
            .desired_width(340.0),
        );
        ui.end_row();
        ui.label("Allowed values");
        ui.add(
          egui::TextEdit::multiline(&mut f.allow)
            .hint_text("one per line; never reported as secrets")
            .desired_rows(3)
            .desired_width(340.0),
        );
        ui.end_row();
      });
  }

  fn ai_tab(&mut self, ui: &mut egui::Ui) {
    let f = &mut self.ai;
    if let Some(rx) = &f.test
      && let Ok(r) = rx.try_recv()
    {
      f.test_result = Some(r);
      f.test = None;
    }
    let before = f.config.provider.clone();
    egui::Grid::new("ai")
      .num_columns(2)
      .spacing([24.0, 10.0])
      .show(ui, |ui| {
        ui.label("Provider");
        let current = ai::provider(&f.config.provider).map(|p| p.name).unwrap_or("?");
        egui::ComboBox::from_id_salt("provider")
          .selected_text(current)
          .show_ui(ui, |ui| {
            for p in ai::PROVIDERS {
              ui.selectable_value(&mut f.config.provider, p.id.to_string(), p.name);
            }
          });
        ui.end_row();
        let p = ai::provider(&f.config.provider).ok();
        ui.label("Model");
        let hint = p
          .and_then(|p| p.default_model)
          .unwrap_or("required, e.g. as listed by the provider");
        ui.add(
          egui::TextEdit::singleline(&mut f.config.model)
            .hint_text(hint)
            .desired_width(340.0),
        );
        ui.end_row();
        ui.label("Base URL");
        let hint = p
          .map(|p| p.base_url)
          .filter(|u| !u.is_empty())
          .unwrap_or("required for this provider");
        ui.add(
          egui::TextEdit::singleline(&mut f.config.base_url)
            .hint_text(hint)
            .desired_width(340.0),
        );
        ui.end_row();
        ui.label("Answer language");
        ui.add(
          egui::TextEdit::singleline(&mut f.config.language)
            .hint_text("English")
            .desired_width(160.0),
        );
        ui.end_row();
        ui.label("API key");
        ui.vertical(|ui| {
          ui.add(
            egui::TextEdit::singleline(&mut f.key)
              .password(true)
              .hint_text("paste a new key to store it")
              .desired_width(340.0),
          );
          if let Some(p) = p {
            let (_, src) = ai::find_key(p);
            let (text, color) = match src {
              ai::KeySource::Env(v) => (format!("using ${v}"), Color32::from_rgb(110, 200, 110)),
              ai::KeySource::Stored => (
                "a key is stored for this provider".into(),
                Color32::from_rgb(110, 200, 110),
              ),
              ai::KeySource::NotNeeded => ("no key needed".into(), Color32::GRAY),
              ai::KeySource::Missing => (
                format!("no key yet (paste one, or set ${})", p.key_env),
                Color32::from_rgb(230, 170, 80),
              ),
            };
            ui.horizontal(|ui| {
              ui.label(RichText::new(text).color(color).small());
              if src == ai::KeySource::Stored
                && ui
                  .small_button("Remove")
                  .on_hover_text("Delete the stored key")
                  .clicked()
                && let Err(e) = ai::store_key(p.id, None)
              {
                self.error = Some(format!("{e:#}"));
              }
            });
          }
        });
        ui.end_row();
      });
    if f.config.provider != before {
      f.config.model.clear();
      f.config.base_url.clear();
    }
    ui.add_space(6.0);
    ui.label(
      RichText::new(
        "Keys are kept in your user config folder (credentials.toml), never in the repository. \
         Before anything is sent, values that look like secrets are replaced with [REDACTED].",
      )
      .weak()
      .small(),
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
      if ui
        .add_enabled(f.test.is_none(), egui::Button::new("Test"))
        .on_hover_text("Save these settings and send a tiny request to check they work")
        .clicked()
      {
        let saved = f.config.save().and_then(|_| {
          if f.key.trim().is_empty() {
            Ok(())
          } else {
            ai::store_key(&f.config.provider, Some(&f.key))
          }
        });
        match saved {
          Err(e) => f.test_result = Some(Err(format!("{e:#}"))),
          Ok(()) => {
            f.key.clear();
            let (tx, rx) = mpsc::channel();
            let cfg = f.config.clone();
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
              let r = ai::ask(&cfg, "Reply with the single word OK.", "Say OK.")
                .map(|r| format!("works: {}", r.text.chars().take(60).collect::<String>()))
                .map_err(|e| format!("{e:#}"));
              let _ = tx.send(r);
              ctx.request_repaint();
            });
            f.test = Some(rx);
            f.test_result = None;
          }
        }
      }
      if f.test.is_some() {
        ui.spinner();
      }
      match &f.test_result {
        Some(Ok(t)) => {
          ui.label(RichText::new(t).color(Color32::from_rgb(110, 200, 110)));
        }
        Some(Err(e)) => {
          ui.label(RichText::new(e).color(Color32::from_rgb(230, 100, 100)));
        }
        None => {}
      }
    });
  }

  fn github(&mut self, ui: &mut egui::Ui) {
    if self.token_ok {
      ui.label(RichText::new("A GitHub token was found.").color(Color32::from_rgb(110, 200, 110)));
    } else {
      ui.label(RichText::new("No GitHub token found.").color(Color32::from_rgb(230, 170, 80)));
    }
    ui.add_space(6.0);
    ui.label(
      "mig reads the token from the GITHUB_TOKEN (or GH_TOKEN) environment variable, or from the gh CLI \
       after `gh auth login`. It is never stored by mig.",
    );
    ui.add_space(6.0);
    if ui
      .button("Check again")
      .on_hover_text("Look for a token again")
      .clicked()
    {
      self.token_ok = github::token().is_some();
    }
  }
}
