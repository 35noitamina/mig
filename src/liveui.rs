// The "Live" tab: every write in the repository as it happens, with the changed lines shown as a
// red (removed) and green (added) diff. New lines glow before settling into the normal green.
use std::collections::VecDeque;
use std::time::Duration;

use eframe::egui::{self, Align2, Color32, FontId, RichText, Sense, Vec2};

use crate::livewatch::{self, CONTEXT, Event, Handle, Kind, Msg, Sign};

const KEEP_EVENTS: usize = 100;
const GLOW: Duration = Duration::from_millis(1500);
const HOT: Duration = Duration::from_secs(2);

enum Row {
  Line(usize),
  Gap(usize),
}

struct Shown {
  id: u64,
  whole: bool,
  rows: Vec<Row>,
  first_change: usize,
  widest: usize,
}

pub struct LiveView {
  handle: Handle,
  events: VecDeque<Event>,
  selected: Option<u64>,
  follow: bool,
  whole: bool,
  note: String,
  unseen: usize,
  scroll_pending: bool,
  shown: Option<Shown>,
}

fn rows_for(ev: &Event, whole: bool) -> Shown {
  let n = ev.lines.len();
  let changed: Vec<usize> = (0..n).filter(|&i| ev.lines[i].sign != Sign::Ctx).collect();
  let keep: Vec<bool> = (0..n)
    .map(|i| !whole || !ev.whole || changed.iter().any(|&c| c.abs_diff(i) <= CONTEXT))
    .collect();
  let mut rows = Vec::new();
  let mut skipped = 0;
  for (i, k) in keep.iter().enumerate() {
    if *k {
      if skipped > 0 {
        rows.push(Row::Gap(skipped));
        skipped = 0;
      }
      rows.push(Row::Line(i));
    } else {
      skipped += 1;
    }
  }
  if skipped > 0 {
    rows.push(Row::Gap(skipped));
  }
  let first_change = rows
    .iter()
    .position(|r| matches!(r, Row::Line(i) if ev.lines[*i].sign != Sign::Ctx))
    .unwrap_or(0);
  Shown {
    id: ev.id,
    whole,
    rows,
    first_change,
    widest: ev.lines.iter().map(|l| l.text.chars().count()).max().unwrap_or(0),
  }
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
  let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
  Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

fn kind_label(k: Kind) -> &'static str {
  match k {
    Kind::Created => "created",
    Kind::Edited => "edited",
    Kind::Deleted => "deleted",
  }
}

fn ranges_text(ev: &Event) -> String {
  match ev.kind {
    Kind::Deleted => format!("{} lines removed", ev.removed),
    _ if ev.binary => "binary or very large file".to_string(),
    _ => ev
      .ranges
      .iter()
      .take(6)
      .map(|&(a, b)| {
        if a == b {
          format!("L{a}")
        } else {
          format!("L{a}–L{b}")
        }
      })
      .collect::<Vec<_>>()
      .join(", "),
  }
}

impl LiveView {
  pub fn new(root: std::path::PathBuf, ctx: egui::Context) -> LiveView {
    LiveView {
      handle: livewatch::spawn(root, ctx),
      events: VecDeque::new(),
      selected: None,
      follow: true,
      whole: false,
      note: String::new(),
      unseen: 0,
      scroll_pending: false,
      shown: None,
    }
  }

  // Takes in new events; call every frame, whichever tab is open.
  pub fn poll(&mut self, visible: bool) {
    while let Ok(m) = self.handle.rx.try_recv() {
      match m {
        Msg::Note(n) => self.note = n,
        Msg::Event(ev) => {
          if self.follow {
            self.selected = Some(ev.id);
            self.scroll_pending = true;
          }
          if !visible {
            self.unseen += 1;
          }
          self.events.push_front(*ev);
          self.events.truncate(KEEP_EVENTS);
        }
      }
    }
    if visible {
      self.unseen = 0;
    }
  }

  pub fn unseen(&self) -> usize {
    self.unseen
  }

  fn hot(&self) -> bool {
    self.events.front().is_some_and(|e| e.arrived.elapsed() < HOT)
  }

  pub fn ui(&mut self, ui: &mut egui::Ui, session: Option<&str>) {
    egui::Panel::left("live-list")
      .resizable(true)
      .default_size(340.0)
      .show(ui, |ui| self.list(ui));
    egui::CentralPanel::default_margins().show(ui, |ui| {
      self.header(ui, session);
      ui.add_space(4.0);
      self.diff(ui);
    });
    if self.hot() || self.events.front().is_some_and(|e| e.arrived.elapsed() < GLOW) {
      ui.ctx().request_repaint_after(Duration::from_millis(40));
    }
  }

  fn header(&mut self, ui: &mut egui::Ui, session: Option<&str>) {
    ui.horizontal(|ui| {
      let t = ui.input(|i| i.time) as f32;
      let live = if self.hot() {
        Color32::from_rgb(235, 80, 80).gamma_multiply(0.65 + 0.35 * (t * 6.0).sin().abs())
      } else {
        Color32::from_gray(130)
      };
      ui.label(RichText::new("● LIVE").color(live).strong());
      match session {
        Some(n) => ui.label(format!("agent session: {n}")),
        None => ui.label(RichText::new("no agent session").weak()),
      };
      ui.separator();
      ui.checkbox(&mut self.follow, "Follow")
        .on_hover_text("Jump to each new write and scroll to what changed");
      let before = self.whole;
      ui.selectable_value(&mut self.whole, false, "Changes")
        .on_hover_text("Only the changed lines with a few lines around them");
      ui.selectable_value(&mut self.whole, true, "Whole file")
        .on_hover_text("The whole file, scrolled to the change");
      if before != self.whole {
        self.scroll_pending = true;
      }
      if ui
        .button("Clear")
        .on_hover_text("Forget the events listed here")
        .clicked()
      {
        self.events.clear();
        self.selected = None;
        self.shown = None;
      }
      if !self.note.is_empty() {
        ui.label(RichText::new(&self.note).weak().small());
      }
    });
  }

  fn list(&mut self, ui: &mut egui::Ui) {
    if self.events.is_empty() {
      ui.add_space(12.0);
      ui.label(RichText::new("Waiting for writes…").weak());
      ui.label(
        RichText::new("Changes made in this repository, by an AI agent or any other tool, appear here as they happen.")
          .weak()
          .small(),
      );
      return;
    }
    let mut clicked = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
      for ev in &self.events {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
        let p = ui.painter();
        let selected = self.selected == Some(ev.id);
        if selected {
          p.rect_filled(rect, 4.0, ui.visuals().selection.bg_fill.gamma_multiply(0.6));
        } else if resp.hovered() {
          p.rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        if ev.arrived.elapsed() < HOT {
          p.rect_filled(
            egui::Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())),
            0.0,
            Color32::from_rgb(70, 190, 110),
          );
        }
        let weak = ui.visuals().weak_text_color();
        let text = ui.visuals().text_color();
        let small = FontId::monospace(11.0);
        let clipped = p.with_clip_rect(rect.shrink2(Vec2::new(8.0, 0.0)));
        clipped.text(
          rect.min + Vec2::new(10.0, 4.0),
          Align2::LEFT_TOP,
          format!("{}  {}", ev.at.format("%H:%M:%S"), kind_label(ev.kind)),
          small.clone(),
          weak,
        );
        clipped.text(
          rect.min + Vec2::new(10.0, 19.0),
          Align2::LEFT_TOP,
          &ev.path,
          FontId::proportional(13.0),
          text,
        );
        let stat = if ev.binary {
          "binary".to_string()
        } else {
          format!("+{} −{}", ev.added, ev.removed)
        };
        clipped.text(
          egui::pos2(rect.right() - 10.0, rect.top() + 4.0),
          Align2::RIGHT_TOP,
          format!("{stat}  {}", ev.tool),
          small,
          if ev.tool == "other" {
            weak
          } else {
            Color32::from_rgb(110, 170, 235)
          },
        );
        if resp.clicked() {
          clicked = Some(ev.id);
        }
      }
    });
    if let Some(id) = clicked {
      self.selected = Some(id);
      self.follow = false;
      self.scroll_pending = true;
    }
  }

  fn diff(&mut self, ui: &mut egui::Ui) {
    let Some(ev) = self.selected.and_then(|id| self.events.iter().find(|e| e.id == id)) else {
      ui.label(RichText::new("No write selected").weak());
      return;
    };
    ui.horizontal(|ui| {
      ui.label(RichText::new(&ev.path).strong().monospace());
      ui.label(RichText::new(format!("{}  ·  {}", kind_label(ev.kind), ranges_text(ev))).weak());
    });
    ui.label(
      RichText::new(format!(
        "+{} −{}  ·  {}  ·  {}",
        ev.added,
        ev.removed,
        ev.tool,
        ev.at.format("%H:%M:%S")
      ))
      .weak()
      .small(),
    );
    ui.add_space(4.0);
    if ev.binary {
      ui.label(RichText::new("Written, but not shown: binary or larger than 1 MB.").weak());
      return;
    }
    if self
      .shown
      .as_ref()
      .is_none_or(|s| s.id != ev.id || s.whole != self.whole)
    {
      self.shown = Some(rows_for(ev, self.whole));
    }
    let Some(shown) = &self.shown else { return };

    let dark = ui.visuals().dark_mode;
    let (add, add_hot, del) = if dark {
      (
        Color32::from_rgb(28, 72, 44),
        Color32::from_rgb(52, 150, 86),
        Color32::from_rgb(92, 36, 44),
      )
    } else {
      (
        Color32::from_rgb(208, 240, 214),
        Color32::from_rgb(120, 220, 140),
        Color32::from_rgb(255, 216, 216),
      )
    };
    let glow = 1.0 - (ev.arrived.elapsed().as_secs_f32() / GLOW.as_secs_f32()).clamp(0.0, 1.0);
    let font = FontId::monospace(13.0);
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, 'm'));
    let text_color = ui.visuals().text_color();
    let weak = ui.visuals().weak_text_color();
    let width = ui.available_width().max((shown.widest as f32 + 14.0) * char_w);

    let mut area = egui::ScrollArea::both().auto_shrink([false, false]);
    if self.scroll_pending {
      area = area.vertical_scroll_offset(shown.first_change.saturating_sub(3) as f32 * row_h);
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    area.show_rows(ui, row_h, shown.rows.len(), |ui, range| {
      for row in &shown.rows[range] {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, row_h), Sense::hover());
        let p = ui.painter();
        match row {
          Row::Gap(n) => {
            p.text(
              rect.left_center() + Vec2::new(8.0, 0.0),
              Align2::LEFT_CENTER,
              format!("⋯ {n} unchanged lines"),
              FontId::proportional(11.0),
              weak,
            );
          }
          Row::Line(i) => {
            let l = &ev.lines[*i];
            match l.sign {
              Sign::Add => {
                p.rect_filled(rect, 0.0, lerp(add, add_hot, glow));
              }
              Sign::Del => {
                p.rect_filled(rect, 0.0, del);
              }
              Sign::Ctx => {}
            }
            let num = |n: Option<u32>| n.map_or(String::new(), |n| n.to_string());
            let y = rect.center().y;
            p.text(
              egui::pos2(rect.left() + 38.0, y),
              Align2::RIGHT_CENTER,
              num(l.old),
              font.clone(),
              weak,
            );
            p.text(
              egui::pos2(rect.left() + 82.0, y),
              Align2::RIGHT_CENTER,
              num(l.new),
              font.clone(),
              weak,
            );
            let (mark, color) = match l.sign {
              Sign::Add => ("+", text_color),
              Sign::Del => ("−", text_color),
              Sign::Ctx => (" ", weak),
            };
            p.text(
              egui::pos2(rect.left() + 92.0, y),
              Align2::LEFT_CENTER,
              mark,
              font.clone(),
              color,
            );
            p.text(
              egui::pos2(rect.left() + 92.0 + char_w * 2.0, y),
              Align2::LEFT_CENTER,
              &l.text,
              font.clone(),
              if l.sign == Sign::Ctx { weak } else { text_color },
            );
          }
        }
      }
    });
    self.scroll_pending = false;
  }
}
