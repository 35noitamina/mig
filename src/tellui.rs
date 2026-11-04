// "Send to Claude": a small dialog that leaves a note for an AI agent (see inbox.rs). The editor opens
// it with the file, the lines and the selected text filled in.
use std::path::PathBuf;

use eframe::egui::{self, RichText};

use crate::{agent, inbox, repo};

pub struct Dialog {
  root: PathBuf,
  folder: String,
  to: String,
  agents: Vec<String>,
  text: String,
  file: Option<String>,
  lines: Option<(usize, usize)>,
  selection: Option<String>,
  focus: bool,
}

impl Dialog {
  pub fn new(
    root: PathBuf,
    folder: String,
    to: Option<String>,
    file: Option<String>,
    lines: Option<(usize, usize)>,
    selection: Option<String>,
  ) -> Dialog {
    let agents = repo::open(&root)
      .map(|r| agent::list(&r).into_iter().map(|s| s.name).collect())
      .unwrap_or_default();
    Dialog {
      root,
      folder,
      to: to.unwrap_or_else(|| "*".into()),
      agents,
      text: String::new(),
      file,
      lines,
      selection,
      focus: true,
    }
  }
}

pub enum Outcome {
  Open,
  Closed,
  Sent(String),
  Failed(String),
}

fn place(file: &Option<String>, lines: Option<(usize, usize)>) -> Option<String> {
  let f = file.as_ref()?;
  Some(match lines {
    Some((a, b)) if a == b => format!("{f}:{a}"),
    Some((a, b)) => format!("{f}:{a}-{b}"),
    None => f.clone(),
  })
}

pub fn show(d: &mut Dialog, ctx: &egui::Context) -> Outcome {
  let (mut send, mut close) = (false, false);
  egui::Window::new("Send to Claude")
    .collapsible(false)
    .pivot(egui::Align2::CENTER_CENTER)
    .default_pos(ctx.content_rect().center())
    .show(ctx, |ui| {
      ui.horizontal(|ui| {
        ui.label(RichText::new(&d.folder).weak());
        ui.label("to");
        let shown = if d.to == "*" {
          "whoever is working".to_string()
        } else {
          d.to.clone()
        };
        egui::ComboBox::from_id_salt("tell-to")
          .selected_text(shown)
          .show_ui(ui, |ui| {
            ui.selectable_value(&mut d.to, "*".to_string(), "whoever is working");
            for a in &d.agents {
              ui.selectable_value(&mut d.to, a.clone(), a);
            }
          });
      });
      if let Some(p) = place(&d.file, d.lines) {
        ui.label(RichText::new(p).monospace().small());
      }
      if let Some(s) = &d.selection {
        let shown: String = s.chars().take(300).collect();
        ui.label(RichText::new(shown).monospace().small().weak());
        if ui.small_button("Leave out the selected text").clicked() {
          d.selection = None;
        }
      }
      let r = ui.add(
        egui::TextEdit::multiline(&mut d.text)
          .hint_text("What should it do or know?  (Ctrl+Enter sends)")
          .desired_rows(4)
          .desired_width(460.0),
      );
      if std::mem::take(&mut d.focus) {
        r.request_focus();
      }
      if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)) {
        send = true;
      }
      ui.horizontal(|ui| {
        close = ui.button("Cancel").clicked();
        send |= ui
          .add_enabled(
            !d.text.trim().is_empty(),
            egui::Button::new(RichText::new("Send").strong()),
          )
          .on_hover_text("The agent gets it with its next tool call, or at once if it is waiting or listening")
          .clicked();
      });
    });
  if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
    close = true;
  }
  if send && !d.text.trim().is_empty() {
    let r = inbox::send(
      &d.root.join(".git"),
      &d.to,
      d.text.trim(),
      d.file.clone(),
      d.lines,
      d.selection.clone(),
    );
    return match r {
      Ok(()) => Outcome::Sent(format!(
        "Sent to {}.",
        if d.to == "*" { "the agent".into() } else { d.to.clone() }
      )),
      Err(e) => Outcome::Failed(format!("{e:#}")),
    };
  }
  if close { Outcome::Closed } else { Outcome::Open }
}
