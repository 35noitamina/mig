use std::path::Path;
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

const PAGE: usize = 500;
const MAX_CELL: usize = 200;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
  Markdown,
  Sqlite,
  Image,
}

impl Kind {
  pub fn detect(path: &str, head: &[u8]) -> Option<Kind> {
    let ext = Path::new(path)
      .extension()
      .map(|e| e.to_string_lossy().to_lowercase())
      .unwrap_or_default();
    if head.starts_with(b"SQLite format 3\0") || matches!(ext.as_str(), "db" | "sqlite" | "sqlite3") {
      return Some(Kind::Sqlite);
    }
    match ext.as_str() {
      "md" | "markdown" => Some(Kind::Markdown),
      "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => Some(Kind::Image),
      _ => None,
    }
  }

  pub fn label(self) -> &'static str {
    match self {
      Kind::Markdown => "Preview",
      Kind::Sqlite => "Tables",
      Kind::Image => "Image",
    }
  }
}

// Classic offset / hex / ASCII dump; only visible rows are laid out.
pub fn hex(ui: &mut egui::Ui, bytes: &[u8], truncated: Option<u64>) {
  if let Some(total) = truncated {
    ui.label(
      RichText::new(format!("Showing the first {} of {} bytes", bytes.len(), total))
        .color(Color32::from_rgb(230, 170, 80)),
    );
  }
  let rows = bytes.len().div_ceil(16);
  let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
  egui::ScrollArea::both()
    .auto_shrink([false, false])
    .show_rows(ui, row_h, rows, |ui, range| {
      for r in range {
        let chunk = &bytes[r * 16..(r * 16 + 16).min(bytes.len())];
        let mut hex = String::with_capacity(16 * 3 + 1);
        for (i, b) in chunk.iter().enumerate() {
          if i == 8 {
            hex.push(' ');
          }
          hex.push_str(&format!("{b:02x} "));
        }
        let ascii: String = chunk
          .iter()
          .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
              b as char
            } else {
              '.'
            }
          })
          .collect();
        ui.horizontal(|ui| {
          ui.label(
            RichText::new(format!("{:08x}", r * 16))
              .monospace()
              .color(Color32::GRAY),
          );
          ui.label(RichText::new(format!("{hex:<49}")).monospace());
          ui.label(RichText::new(ascii).monospace().color(Color32::from_rgb(140, 180, 220)));
        });
      }
    });
}

pub fn image(ui: &mut egui::Ui, path: &str, bytes: &Arc<[u8]>) {
  egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
    // Natural size, shrunk only when it doesn't fit.
    ui.add(
      egui::Image::from_bytes(format!("bytes://{path}"), bytes.clone())
        .fit_to_original_size(1.0)
        .max_size(ui.available_size()),
    );
  });
}

pub fn markdown(ui: &mut egui::Ui, cache: &mut egui_commonmark::CommonMarkCache, text: &str) {
  egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
    egui_commonmark::CommonMarkViewer::new().show(ui, cache, text);
  });
}

struct Grid {
  columns: Vec<String>,
  rows: Vec<Vec<String>>,
}

// Read-only browser for SQLite files.
pub struct DbView {
  conn: Result<Connection, String>,
  tables: Vec<(String, i64)>,
  table: Option<String>,
  page: usize,
  query: String,
  result: Option<Result<Grid, String>>,
}

impl DbView {
  pub fn open(path: &Path) -> DbView {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
      .map_err(|e| e.to_string());
    let mut v = DbView {
      conn,
      tables: vec![],
      table: None,
      page: 0,
      query: String::new(),
      result: None,
    };
    v.load_tables();
    if let Some((t, _)) = v.tables.first().cloned() {
      v.select(t);
    }
    v
  }

  fn load_tables(&mut self) {
    let Ok(conn) = &self.conn else { return };
    let names: Vec<String> = conn
      .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'view') ORDER BY name")
      .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
      .unwrap_or_default();
    self.tables = names
      .into_iter()
      .map(|n| {
        let count = conn
          .query_row(&format!("SELECT count(*) FROM {}", quote(&n)), [], |r| r.get(0))
          .unwrap_or(-1);
        (n, count)
      })
      .collect();
  }

  fn select(&mut self, table: String) {
    self.page = 0;
    self.query = format!("SELECT * FROM {}", quote(&table));
    self.table = Some(table);
    self.run_page();
  }

  fn run_page(&mut self) {
    let q = self.query.trim().trim_end_matches(';');
    let lower = q.to_lowercase();
    // Only plain SELECTs get paged; anything else runs as written (and is refused if it writes).
    let pageable = (lower.starts_with("select") || lower.starts_with("with")) && !lower.contains(" limit ");
    let sql = if pageable {
      format!("{q} LIMIT {PAGE} OFFSET {}", self.page * PAGE)
    } else {
      q.to_string()
    };
    self.result = Some(self.run(&sql));
  }

  fn run(&self, sql: &str) -> Result<Grid, String> {
    let conn = self.conn.as_ref().map_err(Clone::clone)?;
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    if !stmt.readonly() {
      return Err("only read-only queries are allowed here".into());
    }
    let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let n = columns.len();
    let mut rows = Vec::new();
    let mut q = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(r) = q.next().map_err(|e| e.to_string())? {
      rows.push((0..n).map(|i| cell(r.get_ref(i).ok())).collect());
      if rows.len() >= PAGE {
        break;
      }
    }
    Ok(Grid { columns, rows })
  }

  pub fn ui(&mut self, ui: &mut egui::Ui) {
    if let Err(e) = &self.conn {
      ui.label(RichText::new(format!("Cannot open as SQLite: {e}")).color(Color32::from_rgb(230, 100, 100)));
      return;
    }
    let mut pick = None;
    egui::Panel::left("db-tables")
      .resizable(true)
      .default_size(200.0)
      .show(ui, |ui| {
        ui.label(RichText::new("Tables").strong());
        egui::ScrollArea::vertical().id_salt("db-table-list").show(ui, |ui| {
          for (name, count) in &self.tables {
            let text = if *count >= 0 {
              format!("{name}  ({count})")
            } else {
              name.clone()
            };
            if ui.selectable_label(self.table.as_deref() == Some(name), text).clicked() {
              pick = Some(name.clone());
            }
          }
        });
      });
    if let Some(t) = pick {
      self.select(t);
    }
    egui::CentralPanel::default_margins().show(ui, |ui| {
      ui.horizontal(|ui| {
        let w = ui.available_width() - 190.0;
        let resp = ui.add(
          egui::TextEdit::singleline(&mut self.query)
            .code_editor()
            .hint_text("SELECT … (read-only)")
            .desired_width(w.max(120.0)),
        );
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("Run").on_hover_text("Run the query (read-only)").clicked() || enter {
          self.table = None;
          self.page = 0;
          self.run_page();
        }
        if ui
          .add_enabled(self.page > 0, egui::Button::new("Prev"))
          .on_hover_text(format!("Previous {PAGE} rows"))
          .clicked()
        {
          self.page -= 1;
          self.run_page();
        }
        ui.label(format!("page {}", self.page + 1));
        let full = matches!(&self.result, Some(Ok(g)) if g.rows.len() >= PAGE);
        if ui
          .add_enabled(full, egui::Button::new("Next"))
          .on_hover_text(format!("Next {PAGE} rows"))
          .clicked()
        {
          self.page += 1;
          self.run_page();
        }
      });
      ui.separator();
      match &self.result {
        None => {}
        Some(Err(e)) => {
          ui.label(RichText::new(e).color(Color32::from_rgb(230, 100, 100)));
        }
        Some(Ok(g)) if g.columns.is_empty() => {
          ui.label("No columns");
        }
        Some(Ok(g)) => grid(ui, g),
      }
    });
  }
}

fn grid(ui: &mut egui::Ui, g: &Grid) {
  let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
  egui::ScrollArea::horizontal()
    .auto_shrink([false, false])
    .show(ui, |ui| {
      TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .columns(Column::auto().at_least(60.0).resizable(true), g.columns.len())
        .header(row_h, |mut h| {
          for c in &g.columns {
            h.col(|ui| {
              ui.strong(c);
            });
          }
        })
        .body(|body| {
          body.rows(row_h, g.rows.len(), |mut row| {
            let r = row.index();
            for v in &g.rows[r] {
              row.col(|ui| {
                if v == "NULL" {
                  ui.label(RichText::new(v).weak().italics());
                } else {
                  ui.label(v);
                }
              });
            }
          });
        });
    });
}

fn quote(name: &str) -> String {
  format!("\"{}\"", name.replace('"', "\"\""))
}

fn cell(v: Option<ValueRef<'_>>) -> String {
  let s = match v {
    None | Some(ValueRef::Null) => return "NULL".into(),
    Some(ValueRef::Integer(i)) => i.to_string(),
    Some(ValueRef::Real(f)) => f.to_string(),
    Some(ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
    Some(ValueRef::Blob(b)) => return format!("<blob, {} bytes>", b.len()),
  };
  if s.chars().count() > MAX_CELL {
    format!("{}…", s.chars().take(MAX_CELL).collect::<String>())
  } else {
    s
  }
}
