// The detail of one run, shared by the Console page and the Live view: what was run (the whole
// command or script), where and with what limit, what mig's check said, how it ended, which files it
// changed, and its output as it grows.
use std::path::Path;

use eframe::egui::{self, Color32, RichText};
use regex::Regex;

use crate::outlog::{self, Meta};

const SHOWN_LINES: usize = 1500;

pub struct State {
  pub filter: String,
  pub follow: bool,
  status: Option<String>,
}

impl Default for State {
  fn default() -> State {
    State {
      filter: String::new(),
      follow: true,
      status: None,
    }
  }
}

fn clock(ms: i64) -> String {
  chrono::DateTime::from_timestamp(ms / 1000, 0)
    .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
    .unwrap_or_default()
}

fn dur(secs: f64) -> String {
  if secs >= 3600.0 {
    format!("{}h{:02}m", (secs / 3600.0) as u64, ((secs % 3600.0) / 60.0) as u64)
  } else if secs >= 60.0 {
    format!("{}m{:02}s", (secs / 60.0) as u64, (secs % 60.0) as u64)
  } else {
    format!("{secs:.1}s")
  }
}

// "… (line 3)" in a note names a line of the script.
fn flagged_lines(notes: &[String]) -> Vec<usize> {
  notes
    .iter()
    .filter_map(|n| {
      let i = n.find("(line ")? + 6;
      n[i..].split(')').next()?.trim().parse().ok()
    })
    .collect()
}

pub fn show(ui: &mut egui::Ui, git: &Path, id: &str, st: &mut State, folder: Option<&str>) {
  let Some(m) = outlog::meta(git, id) else {
    ui.label(RichText::new("That run is gone.").weak());
    return;
  };
  let lines = outlog::lines(git, id).unwrap_or_default();
  header(ui, git, &m, folder, lines.len(), st);
  ui.add_space(2.0);

  let what = if m.kind == "script" {
    format!("Script ({})", m.lang.as_deref().unwrap_or("?"))
  } else {
    "Command".to_string()
  };
  if !m.code.is_empty() {
    egui::CollapsingHeader::new(RichText::new(what).strong())
      .id_salt(("code", id))
      .default_open(true)
      .show(ui, |ui| {
        let flagged = flagged_lines(&m.notes);
        egui::ScrollArea::both()
          .id_salt(("code-scroll", id))
          .max_height(190.0)
          .auto_shrink([false, true])
          .show(ui, |ui| {
            for (i, l) in m.code.lines().enumerate() {
              let mut t = RichText::new(format!("{:>3}  {l}", i + 1)).monospace();
              if flagged.contains(&(i + 1)) {
                t = t.background_color(Color32::from_rgb(100, 70, 20));
              }
              ui.label(t);
            }
          });
      });
  }
  if !m.notes.is_empty() {
    for n in &m.notes {
      ui.label(RichText::new(format!("• {n}")).color(Color32::from_rgb(225, 170, 80)));
    }
  }
  if !m.changed.is_empty() {
    let shown: Vec<&str> = m.changed.iter().take(12).map(String::as_str).collect();
    let more = if m.changed.len() > 12 {
      format!(" … (+{})", m.changed.len() - 12)
    } else {
      String::new()
    };
    let cp = m
      .checkpoint
      .as_deref()
      .map(|c| format!("  ·  checkpoint {c}"))
      .unwrap_or_default();
    ui.label(
      RichText::new(format!(
        "Changed {} file(s): {}{more}{cp}",
        m.changed.len(),
        shown.join(", ")
      ))
      .weak(),
    );
  }
  ui.separator();
  output(ui, &lines, st);
}

fn header(ui: &mut egui::Ui, git: &Path, m: &Meta, folder: Option<&str>, n_lines: usize, st: &mut State) {
  ui.horizontal_wrapped(|ui| {
    if let Some(f) = folder {
      ui.label(RichText::new(f).weak());
    }
    if m.running() {
      let secs = (crate::live::now_ms() - m.ts) as f64 / 1000.0;
      ui.label(RichText::new(format!("● running {}", dur(secs))).color(Color32::from_rgb(110, 200, 130)));
      ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    } else {
      let ok = m.exit == Some(0) && !m.timed_out && !m.cancelled;
      let color = if ok {
        Color32::from_rgb(110, 200, 130)
      } else {
        Color32::from_rgb(230, 110, 110)
      };
      ui.label(
        RichText::new(format!(
          "{} {} in {}",
          if ok { "✓" } else { "×" },
          m.headline(),
          dur(m.secs)
        ))
        .color(color),
      );
    }
    if let Some(a) = &m.agent {
      ui.label(RichText::new(a).strong().color(Color32::from_rgb(140, 180, 230)));
    }
    ui.label(RichText::new(format!("started {}", clock(m.ts))).weak());
    if m.running()
      && ui
        .button(RichText::new("Stop").color(Color32::from_rgb(230, 110, 110)))
        .on_hover_text("Stop the program and everything it started")
        .clicked()
    {
      st.status = Some(match outlog::request_stop(git, &m.id) {
        Ok(()) => "Stopping…".into(),
        Err(e) => format!("{e:#}"),
      });
    }
    if let Some(s) = &st.status {
      ui.label(RichText::new(s).weak());
    }
  });
  let mut facts = vec![format!("{n_lines} lines of output")];
  facts.push(format!("in {}", if m.cwd.is_empty() { "." } else { &m.cwd }));
  if m.timeout_secs > 0 {
    facts.push(format!("limit {}", dur(m.timeout_secs as f64)));
  } else if m.kind == "argv" || m.kind == "script" {
    facts.push("no time limit".to_string());
  }
  if let Some(c) = m.exit {
    facts.push(format!("exit code {c}"));
  }
  facts.push(format!("id {}", m.id));
  ui.label(RichText::new(facts.join("  ·  ")).weak());
}

fn output(ui: &mut egui::Ui, lines: &[String], st: &mut State) {
  ui.horizontal(|ui| {
    ui.label(RichText::new("Output").strong());
    ui.checkbox(&mut st.follow, "Follow")
      .on_hover_text("Keep the end in view");
    ui.add(
      egui::TextEdit::singleline(&mut st.filter)
        .hint_text("filter (regular expression)")
        .desired_width(220.0),
    );
  });
  let re = (!st.filter.is_empty()).then(|| Regex::new(&st.filter)).transpose();
  let shown: Vec<&String> = match &re {
    Ok(Some(re)) => lines.iter().filter(|l| re.is_match(l)).collect(),
    _ => lines.iter().collect(),
  };
  if re.is_err() {
    ui.label(RichText::new("not a valid regular expression").color(Color32::from_rgb(230, 110, 110)));
  }
  let skip = shown.len().saturating_sub(SHOWN_LINES);
  let mut text = String::new();
  if skip > 0 {
    text.push_str(&format!("… {skip} earlier lines (`mig run out ID` has them) …\n"));
  }
  for l in &shown[skip..] {
    text.push_str(l);
    text.push('\n');
  }
  if lines.is_empty() {
    text = "(no output yet)".into();
  }
  egui::ScrollArea::both()
    .id_salt("run-output")
    .auto_shrink([false, false])
    .stick_to_bottom(st.follow)
    .show(ui, |ui| {
      ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true));
    });
}
