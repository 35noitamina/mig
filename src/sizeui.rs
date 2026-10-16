// The "Size" tab: how the repository grew, per commit or over time.
use std::sync::mpsc::{self, Receiver};

use anyhow::Result;
use chrono::{Local, TimeZone};
use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Shape, Stroke, Vec2};
use git2::{Oid, Repository};

use crate::growth::{self, Metric, Point, Unit};
use crate::{guard, repo};

const MAX_DRAWN: usize = 800;
const MARGIN_LEFT: f32 = 70.0;
const MARGIN_BOTTOM: f32 = 26.0;

#[derive(Clone)]
struct Item {
  x: f64,
  y: u64,
  label: String,
  note: String,
  oid: String,
}

pub struct SizeView {
  root: std::path::PathBuf,
  ctx: egui::Context,
  points: Vec<Point>,
  head: Option<Oid>,
  job: Option<Receiver<Result<Vec<Point>>>>,
  error: Option<String>,
  metric: Metric,
  by_time: bool,
  unit: Unit,
  last: usize,
}

impl SizeView {
  pub fn new(root: std::path::PathBuf, ctx: egui::Context) -> SizeView {
    SizeView {
      root,
      ctx,
      points: vec![],
      head: None,
      job: None,
      error: None,
      metric: Metric::Bytes,
      by_time: false,
      unit: Unit::Week,
      last: growth::DEFAULT_LAST,
    }
  }

  fn poll(&mut self) {
    let Some(rx) = &self.job else { return };
    match rx.try_recv() {
      Ok(r) => {
        self.job = None;
        match r {
          Ok(p) => {
            self.points = p;
            self.error = None;
          }
          Err(e) => self.error = Some(format!("{e:#}")),
        }
      }
      Err(mpsc::TryRecvError::Empty) => {}
      Err(mpsc::TryRecvError::Disconnected) => {
        self.job = None;
        self.error = Some("the size calculation stopped unexpectedly".into());
      }
    }
  }

  // Recalculates in the background when HEAD moved since the last run.
  fn refresh(&mut self, repo: &Repository) {
    let head = repo::head_commit(repo).ok().flatten().map(|c| c.id());
    if self.job.is_some() || head == self.head {
      return;
    }
    self.head = head;
    let (tx, rx) = mpsc::channel();
    let root = self.root.clone();
    let ctx = self.ctx.clone();
    std::thread::spawn(move || {
      let _ = tx.send(repo::open(&root).and_then(|r| growth::history(&r)));
      ctx.request_repaint();
    });
    self.job = Some(rx);
  }

  fn items(&self) -> Vec<Item> {
    let points = growth::tail(&self.points, self.last);
    let m = self.metric;
    let items: Vec<Item> = if self.by_time {
      growth::buckets(points, self.unit)
        .into_iter()
        .map(|b| Item {
          x: b.time as f64,
          y: b.value(m),
          label: b.label,
          note: if b.commits == 0 {
            "no commits in this period".to_string()
          } else {
            format!("{} commit(s), newest: {}", b.commits, b.summary)
          },
          oid: b.oid,
        })
        .collect()
    } else {
      let offset = self.points.len() - points.len();
      points
        .iter()
        .enumerate()
        .map(|(i, p)| Item {
          x: (offset + i + 1) as f64,
          y: p.value(m),
          label: format!("#{} {}", offset + i + 1, &p.oid[..7]),
          note: p.summary.clone(),
          oid: p.oid.clone(),
        })
        .collect()
    };
    growth::thin(&items, MAX_DRAWN)
  }

  // Returns the commit to show when a point is clicked.
  pub fn ui(&mut self, ui: &mut egui::Ui, repo: &Repository) -> Option<String> {
    self.poll();
    self.refresh(repo);

    ui.horizontal(|ui| {
      ui.label("Axis");
      ui.selectable_value(&mut self.by_time, false, "Commits")
        .on_hover_text("One point per commit, oldest on the left");
      ui.selectable_value(&mut self.by_time, true, "Time")
        .on_hover_text("Spread along the calendar by commit date");
      if self.by_time {
        egui::ComboBox::from_id_salt("size-unit")
          .selected_text(match self.unit {
            Unit::Day => "per day",
            Unit::Week => "per week",
            _ => "per month",
          })
          .show_ui(ui, |ui| {
            ui.selectable_value(&mut self.unit, Unit::Day, "per day");
            ui.selectable_value(&mut self.unit, Unit::Week, "per week");
            ui.selectable_value(&mut self.unit, Unit::Month, "per month");
          });
      }
      ui.separator();
      ui.label("Measure");
      ui.selectable_value(&mut self.metric, Metric::Bytes, "Size")
        .on_hover_text("Total bytes of the tracked files");
      ui.selectable_value(&mut self.metric, Metric::Files, "Files")
        .on_hover_text("Number of tracked files");
      ui.selectable_value(&mut self.metric, Metric::Lines, "Lines")
        .on_hover_text("Lines of text (binary files are not counted)");
      ui.separator();
      ui.label("Latest");
      ui.add(egui::DragValue::new(&mut self.last).range(2..=1_000_000).speed(10))
        .on_hover_text("How many of the newest commits to include");
      ui.label("commits");
      if self.job.is_some() {
        ui.spinner();
      }
    });
    ui.add_space(6.0);

    if let Some(e) = &self.error {
      ui.label(RichText::new(e).color(Color32::from_rgb(230, 110, 110)));
      return None;
    }
    if self.points.is_empty() {
      ui.label(if self.job.is_some() {
        "calculating the size history…"
      } else {
        "no commits yet"
      });
      return None;
    }

    let items = self.items();
    let (first, lastv) = (items[0].y, items[items.len() - 1].y);
    let m = self.metric;
    ui.label(format!(
      "now {}  ({} over {} points)",
      fmt(m, lastv),
      fmt_delta(m, lastv as i64 - first as i64),
      items.len()
    ));
    ui.add_space(4.0);

    let size = ui.available_size();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(size.x, size.y.max(160.0) - 8.0), Sense::click());
    draw(ui, rect, &resp, &items, m, self.by_time, self.unit)
  }
}

fn fmt(m: Metric, v: u64) -> String {
  match m {
    Metric::Bytes => guard::fmt_size(v),
    _ => v.to_string(),
  }
}

fn fmt_delta(m: Metric, d: i64) -> String {
  let s = fmt(m, d.unsigned_abs());
  if d >= 0 { format!("+{s}") } else { format!("-{s}") }
}

// A step of 1, 2 or 5 times a power of ten near `raw`.
fn nice_step(raw: f64) -> f64 {
  let p = 10f64.powf(raw.max(1e-9).log10().floor());
  let n = raw / p;
  p * if n <= 1.0 {
    1.0
  } else if n <= 2.0 {
    2.0
  } else if n <= 5.0 {
    5.0
  } else {
    10.0
  }
}

fn time_label(x: f64, unit: Unit) -> String {
  let Some(d) = Local.timestamp_opt(x as i64, 0).single() else {
    return String::new();
  };
  d.format(if unit == Unit::Month { "%Y-%m" } else { "%Y-%m-%d" })
    .to_string()
}

fn draw(
  ui: &mut egui::Ui,
  rect: Rect,
  resp: &egui::Response,
  items: &[Item],
  m: Metric,
  by_time: bool,
  unit: Unit,
) -> Option<String> {
  let painter = ui.painter_at(rect);
  let visuals = ui.visuals().clone();
  let text = visuals.text_color();
  let weak = visuals.weak_text_color();
  let grid = visuals.widgets.noninteractive.bg_stroke.color;
  let accent = visuals.hyperlink_color;
  let font = egui::TextStyle::Small.resolve(ui.style());

  let plot = Rect::from_min_max(
    Pos2::new(rect.left() + MARGIN_LEFT, rect.top() + 8.0),
    Pos2::new(rect.right() - 14.0, rect.bottom() - MARGIN_BOTTOM),
  );
  let (x0, x1) = (items[0].x, items[items.len() - 1].x);
  let span = if x1 > x0 { x1 - x0 } else { 1.0 };
  let top = items.iter().map(|i| i.y).max().unwrap_or(0).max(1) as f64;
  // Bytes are rounded in powers of 1024 so the labels read 100 KB, 200 KB, ...
  let unit_size = if m == Metric::Bytes {
    1024f64.powi(((top.max(1.0).log2() / 10.0).floor() as i32).max(0))
  } else {
    1.0
  };
  let step = nice_step(top * 1.05 / 4.0 / unit_size) * unit_size;
  let ymax = (top * 1.05 / step).ceil().max(1.0) * step;
  let px = |x: f64| plot.left() + ((x - x0) / span) as f32 * plot.width();
  let py = |y: f64| plot.bottom() - (y / ymax) as f32 * plot.height();

  let mut t = 0.0;
  while t <= ymax + step * 0.01 {
    let y = py(t);
    painter.line_segment(
      [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
      Stroke::new(1.0, grid),
    );
    painter.text(
      Pos2::new(plot.left() - 6.0, y),
      egui::Align2::RIGHT_CENTER,
      fmt(m, t.round() as u64),
      font.clone(),
      weak,
    );
    t += step;
  }
  let ticks = ((plot.width() / 110.0) as usize).clamp(2, 8);
  for k in 0..ticks {
    let it = &items[k * (items.len() - 1) / (ticks - 1).max(1)];
    let label = if by_time {
      time_label(it.x, unit)
    } else {
      format!("#{}", it.x as u64)
    };
    let align = match k {
      0 => egui::Align2::LEFT_TOP,
      k if k + 1 == ticks => egui::Align2::RIGHT_TOP,
      _ => egui::Align2::CENTER_TOP,
    };
    painter.text(
      Pos2::new(px(it.x), plot.bottom() + 6.0),
      align,
      label,
      font.clone(),
      weak,
    );
  }

  // Time is drawn as steps (the size holds until the next commit); commits as straight lines.
  let mut path: Vec<Pos2> = Vec::with_capacity(items.len() * 2);
  for (i, it) in items.iter().enumerate() {
    let p = Pos2::new(px(it.x), py(it.y as f64));
    if by_time && i > 0 {
      path.push(Pos2::new(p.x, path[path.len() - 1].y));
    }
    path.push(p);
  }
  let fill = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 28);
  for w in path.windows(2) {
    painter.add(Shape::convex_polygon(
      vec![
        w[0],
        w[1],
        Pos2::new(w[1].x, plot.bottom()),
        Pos2::new(w[0].x, plot.bottom()),
      ],
      fill,
      Stroke::NONE,
    ));
  }
  painter.add(Shape::line(path, Stroke::new(2.0, accent)));
  if items.len() <= 80 {
    for it in items {
      painter.circle_filled(Pos2::new(px(it.x), py(it.y as f64)), 3.0, accent);
    }
  }

  let mut clicked = None;
  if let Some(pos) = resp
    .hover_pos()
    .filter(|p| plot.expand2(Vec2::new(8.0, 0.0)).contains(*p))
  {
    let i = items
      .iter()
      .enumerate()
      .min_by(|a, b| {
        (px(a.1.x) - pos.x)
          .abs()
          .partial_cmp(&(px(b.1.x) - pos.x).abs())
          .unwrap_or(std::cmp::Ordering::Equal)
      })
      .map_or(0, |(i, _)| i);
    let it = &items[i];
    let at = Pos2::new(px(it.x), py(it.y as f64));
    painter.line_segment(
      [Pos2::new(at.x, plot.top()), Pos2::new(at.x, plot.bottom())],
      Stroke::new(1.0, text),
    );
    painter.circle(at, 5.0, accent, Stroke::new(2.0, text));
    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    let prev = if i > 0 { Some(items[i - 1].y) } else { None };
    resp.clone().on_hover_ui_at_pointer(|ui| {
      ui.label(RichText::new(&it.label).strong().monospace());
      ui.label(&it.note);
      ui.label(format!(
        "{}{}",
        fmt(m, it.y),
        prev.map_or(String::new(), |p| format!(
          "  ({} from the previous point)",
          fmt_delta(m, it.y as i64 - p as i64)
        ))
      ));
      ui.label(RichText::new("click to open this commit").weak());
    });
    if resp.clicked() {
      clicked = Some(it.oid.clone());
    }
  }
  clicked
}
