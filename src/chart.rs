// Draws a line or bar chart from numbers, as SVG and as a PNG (the PNG so that an AI that can look
// at images sees the result; the SVG for people). The drawing is described once as a list of simple
// shapes, and two small renderers turn that list into the two formats.
use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::Value;

use crate::{files, outlog};

const PALETTE: [&str; 8] = [
  "#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f", "#edc948", "#b07aa1", "#ff9da7",
];
const MAX_POINTS: usize = 1500;
const INK: &str = "#222222";
const GRID: &str = "#dddddd";
const SCALE: f32 = 2.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
  Line,
  Bar,
}

#[derive(Clone, Debug)]
pub struct Series {
  pub name: String,
  pub ys: Vec<f64>,
}

#[derive(Clone, Debug)]
pub struct Spec {
  pub kind: Kind,
  pub title: String,
  pub series: Vec<Series>,
  // Names for the x positions (optional); otherwise 1, 2, 3...
  pub x_labels: Vec<String>,
  pub width: u32,
  pub height: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum Anchor {
  Start,
  Middle,
  End,
}

enum Shape {
  Line {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    w: f32,
    color: &'static str,
  },
  Poly {
    pts: Vec<(f32, f32)>,
    w: f32,
    color: &'static str,
  },
  Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: &'static str,
  },
  Text {
    s: String,
    x: f32,
    y: f32,
    size: f32,
    anchor: Anchor,
    color: &'static str,
  },
}

pub struct Stats {
  pub name: String,
  pub n: usize,
  pub min: f64,
  pub max: f64,
  pub last: f64,
  pub mean: f64,
}

pub fn stats(spec: &Spec) -> Vec<Stats> {
  spec
    .series
    .iter()
    .map(|s| {
      let n = s.ys.len();
      let min = s.ys.iter().copied().fold(f64::INFINITY, f64::min);
      let max = s.ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
      Stats {
        name: s.name.clone(),
        n,
        min,
        max,
        last: s.ys.last().copied().unwrap_or(f64::NAN),
        mean: if n == 0 {
          f64::NAN
        } else {
          s.ys.iter().sum::<f64>() / n as f64
        },
      }
    })
    .collect()
}

pub fn fmt_num(v: f64) -> String {
  if !v.is_finite() {
    return "-".into();
  }
  if v == v.trunc() && v.abs() < 1e9 {
    return format!("{}", v as i64);
  }
  let a = v.abs();
  let s = if a >= 100.0 {
    format!("{v:.1}")
  } else if a >= 1.0 {
    format!("{v:.2}")
  } else {
    format!("{v:.3}")
  };
  s.trim_end_matches('0').trim_end_matches('.').to_string()
}

// Round-number tick positions covering lo..=hi.
fn nice_ticks(lo: f64, hi: f64, want: usize) -> Vec<f64> {
  if !(lo.is_finite() && hi.is_finite()) {
    return vec![0.0, 1.0];
  }
  let (lo, hi) = if (hi - lo).abs() < f64::EPSILON {
    (lo - 1.0, hi + 1.0)
  } else {
    (lo, hi)
  };
  let raw = (hi - lo) / want.max(1) as f64;
  let mag = 10f64.powf(raw.log10().floor());
  let step = [1.0, 2.0, 2.5, 5.0, 10.0]
    .iter()
    .map(|m| m * mag)
    .find(|s| *s >= raw)
    .unwrap_or(10.0 * mag);
  let first = (lo / step).floor() * step;
  let mut out = Vec::new();
  let mut t = first;
  while out.len() < 50 {
    out.push((t / step).round() * step);
    if t >= hi - step * 1e-9 {
      break;
    }
    t += step;
  }
  out
}

// Long series are thinned to about MAX_POINTS by keeping the highest and lowest value of each bucket.
fn thin(ys: &[f64]) -> Vec<f64> {
  if ys.len() <= MAX_POINTS {
    return ys.to_vec();
  }
  let bucket = ys.len().div_ceil(MAX_POINTS / 2);
  let mut out = Vec::new();
  for c in ys.chunks(bucket) {
    let (mut lo, mut hi) = (0, 0);
    for (i, v) in c.iter().enumerate() {
      if *v < c[lo] {
        lo = i;
      }
      if *v > c[hi] {
        hi = i;
      }
    }
    let (a, b) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    out.push(c[a]);
    if a != b {
      out.push(c[b]);
    }
  }
  out
}

fn ascii(s: &str) -> String {
  s.chars()
    .map(|c| if c.is_ascii() && !c.is_control() { c } else { '?' })
    .collect()
}

fn layout(spec: &Spec) -> Result<Vec<Shape>> {
  let (w, h) = (spec.width as f32, spec.height as f32);
  if spec.series.is_empty() || spec.series.iter().all(|s| s.ys.is_empty()) {
    bail!("there are no numbers to draw");
  }
  let legend = spec.series.len() > 1;
  let (left, right, top, bottom) = (62.0, if legend { 130.0 } else { 18.0 }, 38.0, 42.0);
  let (px0, px1, py0, py1) = (left, w - right, top, h - bottom);
  let series: Vec<Series> = spec
    .series
    .iter()
    .map(|s| Series {
      name: s.name.clone(),
      ys: thin(&s.ys.iter().copied().filter(|v| v.is_finite()).collect::<Vec<_>>()),
    })
    .collect();
  let n = series.iter().map(|s| s.ys.len()).max().unwrap_or(0).max(1);
  let mut lo = series
    .iter()
    .flat_map(|s| s.ys.iter())
    .copied()
    .fold(f64::INFINITY, f64::min);
  let mut hi = series
    .iter()
    .flat_map(|s| s.ys.iter())
    .copied()
    .fold(f64::NEG_INFINITY, f64::max);
  if spec.kind == Kind::Bar {
    lo = lo.min(0.0);
    hi = hi.max(0.0);
  }
  let ticks = nice_ticks(lo, hi, 5);
  let (ymin, ymax) = (ticks[0].min(lo), ticks[ticks.len() - 1].max(hi));
  let span = if ymax > ymin { ymax - ymin } else { 1.0 };
  let ypos = |v: f64| py1 - ((v - ymin) / span) as f32 * (py1 - py0);
  let mut out = vec![Shape::Rect {
    x: 0.0,
    y: 0.0,
    w,
    h,
    color: "#ffffff",
  }];
  out.push(Shape::Text {
    s: ascii(&spec.title),
    x: w / 2.0,
    y: 24.0,
    size: 16.0,
    anchor: Anchor::Middle,
    color: INK,
  });
  for t in &ticks {
    let y = ypos(*t);
    out.push(Shape::Line {
      x0: px0,
      y0: y,
      x1: px1,
      y1: y,
      w: 1.0,
      color: GRID,
    });
    out.push(Shape::Text {
      s: fmt_num(*t),
      x: px0 - 6.0,
      y: y + 4.0,
      size: 11.0,
      anchor: Anchor::End,
      color: INK,
    });
  }
  out.push(Shape::Line {
    x0: px0,
    y0: py0,
    x1: px0,
    y1: py1,
    w: 1.0,
    color: INK,
  });
  out.push(Shape::Line {
    x0: px0,
    y0: ypos(0.0f64.clamp(ymin, ymax)),
    x1: px1,
    y1: ypos(0.0f64.clamp(ymin, ymax)),
    w: 1.0,
    color: INK,
  });
  // x positions
  let xpos = |i: usize| -> f32 {
    match spec.kind {
      Kind::Line => {
        if n == 1 {
          (px0 + px1) / 2.0
        } else {
          px0 + (px1 - px0) * i as f32 / (n - 1) as f32
        }
      }
      Kind::Bar => px0 + (px1 - px0) * (i as f32 + 0.5) / n as f32,
    }
  };
  let labels = spec.x_labels.len() == n && n <= MAX_POINTS;
  let step = n.div_ceil(8).max(1);
  for i in (0..n).step_by(step) {
    let s = if labels {
      ascii(&spec.x_labels[i])
    } else {
      format!("{}", i + 1)
    };
    out.push(Shape::Text {
      s,
      x: xpos(i),
      y: py1 + 16.0,
      size: 11.0,
      anchor: Anchor::Middle,
      color: INK,
    });
  }
  match spec.kind {
    Kind::Line => {
      for (k, s) in series.iter().enumerate() {
        let color = PALETTE[k % PALETTE.len()];
        let pts: Vec<(f32, f32)> = s.ys.iter().enumerate().map(|(i, v)| (xpos(i), ypos(*v))).collect();
        if pts.len() == 1 {
          out.push(Shape::Rect {
            x: pts[0].0 - 3.0,
            y: pts[0].1 - 3.0,
            w: 6.0,
            h: 6.0,
            color,
          });
        } else {
          out.push(Shape::Poly { pts, w: 2.0, color });
        }
      }
    }
    Kind::Bar => {
      let group = (px1 - px0) / n as f32 * 0.8;
      let each = group / series.len() as f32;
      for (k, s) in series.iter().enumerate() {
        let color = PALETTE[k % PALETTE.len()];
        for (i, v) in s.ys.iter().enumerate() {
          let x = xpos(i) - group / 2.0 + each * k as f32;
          let (a, b) = (ypos(*v), ypos(0.0f64.clamp(ymin, ymax)));
          out.push(Shape::Rect {
            x,
            y: a.min(b),
            w: (each - 1.0).max(1.0),
            h: (a - b).abs().max(1.0),
            color,
          });
        }
      }
    }
  }
  if legend {
    for (k, s) in series.iter().enumerate() {
      let y = py0 + 6.0 + k as f32 * 18.0;
      out.push(Shape::Rect {
        x: px1 + 12.0,
        y: y - 8.0,
        w: 10.0,
        h: 10.0,
        color: PALETTE[k % PALETTE.len()],
      });
      let mut name = ascii(&s.name);
      if name.len() > 14 {
        name.truncate(13);
        name.push('~');
      }
      out.push(Shape::Text {
        s: name,
        x: px1 + 28.0,
        y,
        size: 11.0,
        anchor: Anchor::Start,
        color: INK,
      });
    }
  }
  Ok(out)
}

fn xml(s: &str) -> String {
  s.replace('&', "&amp;")
    .replace('<', "&lt;")
    .replace('>', "&gt;")
    .replace('"', "&quot;")
}

pub fn svg(spec: &Spec) -> Result<String> {
  let shapes = layout(spec)?;
  let (w, h) = (spec.width, spec.height);
  let mut out = format!(
    "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" font-family=\"monospace\">\n"
  );
  for s in &shapes {
    match s {
      Shape::Line {
        x0,
        y0,
        x1,
        y1,
        w,
        color,
      } => out.push_str(&format!(
        "<line x1=\"{x0:.1}\" y1=\"{y0:.1}\" x2=\"{x1:.1}\" y2=\"{y1:.1}\" stroke=\"{color}\" stroke-width=\"{w}\"/>\n"
      )),
      Shape::Poly { pts, w, color } => {
        let p: Vec<String> = pts.iter().map(|(x, y)| format!("{x:.1},{y:.1}")).collect();
        out.push_str(&format!(
          "<polyline points=\"{}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"{w}\" stroke-linejoin=\"round\"/>\n",
          p.join(" ")
        ));
      }
      Shape::Rect { x, y, w, h, color } => out.push_str(&format!(
        "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" fill=\"{color}\"/>\n"
      )),
      Shape::Text {
        s,
        x,
        y,
        size,
        anchor,
        color,
      } => {
        let a = match anchor {
          Anchor::Start => "start",
          Anchor::Middle => "middle",
          Anchor::End => "end",
        };
        out.push_str(&format!(
          "<text x=\"{x:.1}\" y=\"{y:.1}\" font-size=\"{size}\" text-anchor=\"{a}\" fill=\"{color}\">{}</text>\n",
          xml(s)
        ));
      }
    }
  }
  out.push_str("</svg>\n");
  Ok(out)
}

fn rgb(hex: &str) -> [f32; 3] {
  let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
  [((v >> 16) & 255) as f32, ((v >> 8) & 255) as f32, (v & 255) as f32]
}

struct Canvas {
  w: usize,
  h: usize,
  px: Vec<f32>,
}

impl Canvas {
  fn blend(&mut self, x: i32, y: i32, c: [f32; 3], a: f32) {
    if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h || a <= 0.0 {
      return;
    }
    let i = (y as usize * self.w + x as usize) * 3;
    for k in 0..3 {
      self.px[i + k] = self.px[i + k] * (1.0 - a) + c[k] * a;
    }
  }

  fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: [f32; 3]) {
    let (x0, y0, x1, y1) = (
      x.round() as i32,
      y.round() as i32,
      (x + w).round() as i32,
      (y + h).round() as i32,
    );
    for yy in y0..y1.max(y0 + 1) {
      for xx in x0..x1.max(x0 + 1) {
        self.blend(xx, yy, c, 1.0);
      }
    }
  }

  // A thick segment, anti-aliased by the distance of each pixel to it.
  fn segment(&mut self, a: (f32, f32), b: (f32, f32), width: f32, c: [f32; 3]) {
    let r = width / 2.0;
    let (x0, x1) = (
      (a.0.min(b.0) - r - 1.0).floor() as i32,
      (a.0.max(b.0) + r + 1.0).ceil() as i32,
    );
    let (y0, y1) = (
      (a.1.min(b.1) - r - 1.0).floor() as i32,
      (a.1.max(b.1) + r + 1.0).ceil() as i32,
    );
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    for y in y0..=y1 {
      for x in x0..=x1 {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        let t = if len2 == 0.0 {
          0.0
        } else {
          (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0)
        };
        let d = ((px - (a.0 + t * dx)).powi(2) + (py - (a.1 + t * dy)).powi(2)).sqrt();
        self.blend(x, y, c, (r + 0.5 - d).clamp(0.0, 1.0));
      }
    }
  }

  fn text(&mut self, font: &FontRef, s: &str, x: f32, y: f32, size: f32, anchor: Anchor, c: [f32; 3]) {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let width: f32 = s.chars().map(|ch| scaled.h_advance(scaled.glyph_id(ch))).sum();
    let mut cx = match anchor {
      Anchor::Start => x,
      Anchor::Middle => x - width / 2.0,
      Anchor::End => x - width,
    };
    for ch in s.chars() {
      let id = scaled.glyph_id(ch);
      let glyph = id.with_scale_and_position(scale, point(cx, y));
      cx += scaled.h_advance(id);
      if let Some(o) = font.outline_glyph(glyph) {
        let b = o.px_bounds();
        o.draw(|gx, gy, cov| self.blend(b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, c, cov));
      }
    }
  }
}

pub fn png(spec: &Spec) -> Result<Vec<u8>> {
  let shapes = layout(spec)?;
  let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).map_err(|e| anyhow!("font: {e}"))?;
  let (w, h) = (
    (spec.width as f32 * SCALE) as usize,
    (spec.height as f32 * SCALE) as usize,
  );
  let mut cv = Canvas {
    w,
    h,
    px: vec![255.0; w * h * 3],
  };
  for s in &shapes {
    match s {
      Shape::Rect { x, y, w, h, color } => cv.rect(x * SCALE, y * SCALE, w * SCALE, h * SCALE, rgb(color)),
      Shape::Line {
        x0,
        y0,
        x1,
        y1,
        w,
        color,
      } => cv.segment(
        (x0 * SCALE, y0 * SCALE),
        (x1 * SCALE, y1 * SCALE),
        w * SCALE,
        rgb(color),
      ),
      Shape::Poly { pts, w, color } => {
        for p in pts.windows(2) {
          cv.segment(
            (p[0].0 * SCALE, p[0].1 * SCALE),
            (p[1].0 * SCALE, p[1].1 * SCALE),
            w * SCALE,
            rgb(color),
          );
        }
      }
      Shape::Text {
        s,
        x,
        y,
        size,
        anchor,
        color,
      } => cv.text(&font, s, x * SCALE, y * SCALE, size * SCALE, *anchor, rgb(color)),
    }
  }
  // Average each 2x2 block: the drawing was made twice as large to smooth its edges.
  let (ow, oh) = (spec.width as usize, spec.height as usize);
  let mut data = Vec::with_capacity(ow * oh * 3);
  for y in 0..oh {
    for x in 0..ow {
      for k in 0..3 {
        let mut sum = 0.0;
        for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
          sum += cv.px[((y * 2 + dy).min(h - 1) * w + (x * 2 + dx).min(w - 1)) * 3 + k];
        }
        data.push((sum / 4.0).round().clamp(0.0, 255.0) as u8);
      }
    }
  }
  let mut out = Vec::new();
  {
    let mut enc = png::Encoder::new(&mut out, spec.width, spec.height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| anyhow!("png: {e}"))?;
    writer.write_image_data(&data).map_err(|e| anyhow!("png: {e}"))?;
  }
  Ok(out)
}

fn numbers(v: &Value) -> Vec<f64> {
  v.as_array()
    .map(|a| a.iter().filter_map(Value::as_f64).collect())
    .unwrap_or_default()
}

// The first number each matching line holds: group 1 of the pattern, or the whole match.
fn from_lines(lines: &[String], pattern: &str) -> Result<Vec<f64>> {
  let re = Regex::new(pattern).map_err(|e| anyhow!("pattern is not a valid regular expression: {e}"))?;
  Ok(
    lines
      .iter()
      .filter_map(|l| {
        let c = re.captures(l)?;
        c.get(1)
          .or_else(|| c.get(0))?
          .as_str()
          .replace(',', "")
          .trim()
          .parse::<f64>()
          .ok()
      })
      .collect(),
  )
}

fn split_row(line: &str, d: char) -> Vec<String> {
  line.split(d).map(|c| c.trim().trim_matches('"').to_string()).collect()
}

// Columns of a CSV file: (x labels, series), chosen by header name or position.
fn from_csv(text: &str, x: Option<&str>, ys: &[String]) -> Result<(Vec<String>, Vec<Series>)> {
  let first = text
    .lines()
    .find(|l| !l.trim().is_empty())
    .ok_or_else(|| anyhow!("the file is empty"))?;
  let d = [',', ';', '\t']
    .into_iter()
    .max_by_key(|d| first.matches(*d).count())
    .unwrap_or(',');
  let rows: Vec<Vec<String>> = text
    .lines()
    .filter(|l| !l.trim().is_empty())
    .take(100_000)
    .map(|l| split_row(l, d))
    .collect();
  let header = rows[0].iter().any(|c| c.parse::<f64>().is_err());
  let names: Vec<String> = if header {
    rows[0].clone()
  } else {
    (0..rows[0].len()).map(|i| format!("col{i}")).collect()
  };
  let body = &rows[usize::from(header)..];
  let col = |q: &str| -> Result<usize> {
    names
      .iter()
      .position(|n| n == q)
      .or_else(|| q.parse::<usize>().ok().filter(|i| *i < names.len()))
      .ok_or_else(|| anyhow!("no column {q}; the columns are: {}", names.join(", ")))
  };
  let xi = x.map(col).transpose()?;
  let chosen: Vec<usize> = if ys.is_empty() {
    (0..names.len()).filter(|i| Some(*i) != xi).collect()
  } else {
    ys.iter().map(|y| col(y)).collect::<Result<_>>()?
  };
  let labels = xi
    .map(|i| body.iter().map(|r| r.get(i).cloned().unwrap_or_default()).collect())
    .unwrap_or_default();
  let series: Vec<Series> = chosen
    .into_iter()
    .map(|i| Series {
      name: names[i].clone(),
      ys: body
        .iter()
        .filter_map(|r| r.get(i)?.replace(',', "").parse::<f64>().ok())
        .collect(),
    })
    .filter(|s| !s.ys.is_empty())
    .collect();
  Ok((labels, series))
}

pub struct Made {
  pub text: String,
  pub png: Vec<u8>,
}

// The `chart` tool: numbers from the arguments, from the output of a run, or from a CSV file.
pub fn make(root: &Path, git_dir: &Path, args: &Value) -> Result<Made> {
  let kind = match args.get("type").and_then(Value::as_str).unwrap_or("line") {
    "line" => Kind::Line,
    "bar" => Kind::Bar,
    o => bail!("type is line or bar, not {o}"),
  };
  let mut series: Vec<Series> = Vec::new();
  let mut x_labels: Vec<String> = args
    .get("x_labels")
    .and_then(Value::as_array)
    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
    .unwrap_or_default();
  if let Some(a) = args.get("series").and_then(Value::as_array) {
    for (i, s) in a.iter().enumerate() {
      series.push(Series {
        name: s
          .get("name")
          .and_then(Value::as_str)
          .map_or(format!("series {}", i + 1), str::to_string),
        ys: numbers(s.get("values").unwrap_or(&Value::Null)),
      });
    }
  }
  if let Some(v) = args.get("values") {
    series.push(Series {
      name: "values".into(),
      ys: numbers(v),
    });
  }
  let runs: Vec<&Value> = match args.get("from_run") {
    Some(Value::Array(a)) => a.iter().collect(),
    Some(v @ Value::Object(_)) => vec![v],
    _ => vec![],
  };
  for r in runs {
    let job = r
      .get("job")
      .and_then(Value::as_str)
      .ok_or_else(|| anyhow!("from_run needs job"))?;
    let pat = r
      .get("pattern")
      .and_then(Value::as_str)
      .ok_or_else(|| anyhow!("from_run needs pattern (a regex whose group 1 is the number)"))?;
    let ys = from_lines(&outlog::lines(git_dir, job)?, pat)?;
    series.push(Series {
      name: r.get("name").and_then(Value::as_str).unwrap_or(job).to_string(),
      ys,
    });
  }
  if let Some(c) = args.get("csv") {
    let path = c
      .get("path")
      .and_then(Value::as_str)
      .ok_or_else(|| anyhow!("csv needs path"))?;
    let text = std::fs::read_to_string(files::resolve(root, path)?).map_err(|e| anyhow!("{path}: {e}"))?;
    let ys: Vec<String> = c
      .get("y")
      .map(|v| match v {
        Value::Array(a) => a
          .iter()
          .map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))
          .collect(),
        o => vec![o.as_str().map(str::to_string).unwrap_or_else(|| o.to_string())],
      })
      .unwrap_or_default();
    let x = c
      .get("x")
      .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
    let (labels, s) = from_csv(&text, x.as_deref(), &ys)?;
    if x_labels.is_empty() {
      x_labels = labels;
    }
    series.extend(s);
  }
  if series.iter().all(|s| s.ys.is_empty()) {
    bail!("no numbers found: give series, values, from_run (job + pattern) or csv");
  }
  let dim = |k: &str, d: u32| {
    args
      .get(k)
      .and_then(Value::as_u64)
      .map_or(d, |n| (n as u32).clamp(200, 1600))
  };
  let spec = Spec {
    kind,
    title: args.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
    series,
    x_labels,
    width: dim("width", 720),
    height: dim("height", 360),
  };
  let svg_text = svg(&spec)?;
  let png_bytes = png(&spec)?;
  let dir = git_dir.join("mig").join("charts");
  std::fs::create_dir_all(&dir)?;
  prune(&dir);
  let name: String = args
    .get("name")
    .and_then(Value::as_str)
    .unwrap_or("")
    .chars()
    .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
    .take(40)
    .collect();
  let name = if name.is_empty() {
    format!("chart-{}", crate::live::now_ms())
  } else {
    name
  };
  std::fs::write(dir.join(format!("{name}.svg")), svg_text)?;
  std::fs::write(dir.join(format!("{name}.png")), &png_bytes)?;
  let mut text = format!("chart saved: .git/mig/charts/{name}.png (and .svg)\n");
  for s in stats(&spec) {
    text.push_str(&format!(
      "{}: n={} min={} max={} last={} mean={}\n",
      s.name,
      s.n,
      fmt_num(s.min),
      fmt_num(s.max),
      fmt_num(s.last),
      fmt_num(s.mean)
    ));
  }
  Ok(Made {
    text: text.trim_end().to_string(),
    png: png_bytes,
  })
}

fn prune(dir: &Path) {
  let mut files: Vec<_> = std::fs::read_dir(dir)
    .map(|rd| {
      rd.filter_map(Result::ok)
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect()
    })
    .unwrap_or_default();
  files.sort();
  let excess = files.len().saturating_sub(60);
  for (_, p) in files.into_iter().take(excess) {
    let _ = std::fs::remove_file(p);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn spec(kind: Kind) -> Spec {
    Spec {
      kind,
      title: "Build time".into(),
      series: vec![
        Series {
          name: "a".into(),
          ys: vec![1.0, 3.0, 2.0, 5.0],
        },
        Series {
          name: "b".into(),
          ys: vec![2.0, 2.5, 4.0, 3.0],
        },
      ],
      x_labels: vec![],
      width: 480,
      height: 260,
    }
  }

  #[test]
  fn ticks_are_round() {
    assert_eq!(nice_ticks(0.0, 10.0, 5), vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0]);
    assert_eq!(fmt_num(2.50), "2.5");
    assert_eq!(fmt_num(3.0), "3");
  }

  #[test]
  fn svg_has_title_and_series() {
    let s = svg(&spec(Kind::Line)).unwrap();
    assert!(s.contains("Build time") && s.matches("<polyline").count() == 2);
    let b = svg(&spec(Kind::Bar)).unwrap();
    assert!(b.matches("<rect").count() > 8);
  }

  #[test]
  fn png_is_a_real_image_with_ink_in_it() {
    let p = png(&spec(Kind::Line)).unwrap();
    assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
    let dec = png::Decoder::new(std::io::Cursor::new(&p));
    let mut r = dec.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    assert!(buf.iter().any(|b| *b < 100), "something dark was drawn");
    assert!(buf.iter().any(|b| *b == 255));
  }

  #[test]
  fn numbers_come_from_run_output_and_csv() {
    let lines: Vec<String> = ["step 1 took 1.5s", "noise", "step 2 took 2,500.25s"]
      .iter()
      .map(|s| s.to_string())
      .collect();
    assert_eq!(from_lines(&lines, r"took ([\d.,]+)s").unwrap(), vec![1.5, 2500.25]);
    let (labels, s) = from_csv("day,a,b\nmon,1,2\ntue,3,x\n", Some("day"), &[]).unwrap();
    assert_eq!(labels, vec!["mon", "tue"]);
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].ys, vec![1.0, 3.0]);
    assert!(from_csv("a,b\n1,2\n", Some("zzz"), &[]).is_err());
  }

  #[test]
  fn nothing_to_draw_is_an_error_and_long_series_are_thinned() {
    let mut s = spec(Kind::Line);
    s.series.clear();
    assert!(svg(&s).is_err());
    let long: Vec<f64> = (0..100_000).map(|i| (i % 97) as f64).collect();
    assert!(thin(&long).len() <= MAX_POINTS + 2);
  }
}
