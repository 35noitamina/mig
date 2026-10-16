// Size history of the current branch: per commit, or per day / week / month. The totals are built
// from tree diffs against the first parent, so only changed files are read, and cached in
// `.git/mig/growth.json` so later runs only look at new commits.
use std::path::PathBuf;

use anyhow::Result;
use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
use git2::{Diff, DiffOptions, FileMode, Oid, Repository};
use serde::{Deserialize, Serialize};

use crate::repo;

// Blobs above this are counted for bytes and files, but not for lines.
const LINE_COUNT_MAX: u64 = 4 << 20;
pub const DEFAULT_LAST: usize = 1000;

#[derive(Clone, Copy, PartialEq)]
pub enum Metric {
  Bytes,
  Files,
  Lines,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Unit {
  Commit,
  Day,
  Week,
  Month,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Point {
  pub oid: String,
  pub time: i64,
  pub summary: String,
  pub bytes: u64,
  pub files: u64,
  pub lines: u64,
}

impl Point {
  #[cfg_attr(not(feature = "gui"), allow(dead_code))]
  pub fn value(&self, m: Metric) -> u64 {
    match m {
      Metric::Bytes => self.bytes,
      Metric::Files => self.files,
      Metric::Lines => self.lines,
    }
  }
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
  points: Vec<Point>,
}

fn cache_path(repo: &Repository) -> PathBuf {
  repo.path().join("mig").join("growth.json")
}

fn load_cache(repo: &Repository) -> Cache {
  std::fs::read_to_string(cache_path(repo))
    .ok()
    .and_then(|s| serde_json::from_str(&s).ok())
    .unwrap_or_default()
}

fn save_cache(repo: &Repository, c: &Cache) {
  let p = cache_path(repo);
  if let Some(dir) = p.parent() {
    let _ = std::fs::create_dir_all(dir);
  }
  if let Ok(s) = serde_json::to_string(c) {
    let _ = std::fs::write(p, s);
  }
}

fn blob_lines(repo: &Repository, id: Oid, size: u64) -> u64 {
  if size == 0 || size > LINE_COUNT_MAX {
    return 0;
  }
  match repo.find_blob(id) {
    Ok(b) if !b.is_binary() => {
      let c = b.content();
      c.iter().filter(|&&x| x == b'\n').count() as u64 + u64::from(!c.ends_with(b"\n"))
    }
    _ => 0,
  }
}

// What one commit adds to (or removes from) the totals of its first parent.
fn delta(repo: &Repository, diff: &Diff<'_>) -> Result<(i64, i64, i64)> {
  let odb = repo.odb()?;
  let (mut bytes, mut files, mut lines) = (0i64, 0i64, 0i64);
  for d in diff.deltas() {
    for (file, sign) in [(d.old_file(), -1i64), (d.new_file(), 1i64)] {
      if !file.exists() || file.mode() == FileMode::Commit || file.mode() == FileMode::Tree {
        continue;
      }
      let size = odb.read_header(file.id()).map(|(s, _)| s as u64).unwrap_or(0);
      bytes += sign * size as i64;
      lines += sign * blob_lines(repo, file.id(), size) as i64;
      files += sign;
    }
  }
  Ok((bytes, files, lines))
}

fn add(base: u64, d: i64) -> u64 {
  (base as i64 + d).max(0) as u64
}

// Oldest to newest along the first-parent chain of HEAD.
pub fn history(repo: &Repository) -> Result<Vec<Point>> {
  let Some(head) = repo::head_commit(repo)? else {
    return Ok(vec![]);
  };
  let mut walk = repo.revwalk()?;
  walk.push(head.id())?;
  walk.simplify_first_parent()?;
  walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
  let ids: Vec<Oid> = walk.collect::<Result<_, _>>()?;

  let mut cache = load_cache(repo);
  let keep = cache
    .points
    .iter()
    .zip(&ids)
    .take_while(|(p, id)| p.oid == id.to_string())
    .count();
  cache.points.truncate(keep);
  if keep == ids.len() {
    return Ok(cache.points);
  }

  let mut opts = DiffOptions::new();
  opts.include_typechange(true);
  for id in &ids[keep..] {
    let commit = repo.find_commit(*id)?;
    let parent = commit.parents().next().map(|p| p.tree()).transpose()?;
    let diff = repo.diff_tree_to_tree(parent.as_ref(), Some(&commit.tree()?), Some(&mut opts))?;
    let (b, f, l) = delta(repo, &diff)?;
    let prev = cache.points.last();
    cache.points.push(Point {
      oid: id.to_string(),
      time: commit.time().seconds(),
      summary: repo::summary(&commit),
      bytes: add(prev.map_or(0, |p| p.bytes), b),
      files: add(prev.map_or(0, |p| p.files), f),
      lines: add(prev.map_or(0, |p| p.lines), l),
    });
  }
  save_cache(repo, &cache);
  Ok(cache.points)
}

// The newest `last` commits.
pub fn tail(points: &[Point], last: usize) -> &[Point] {
  &points[points.len().saturating_sub(last)..]
}

#[derive(Clone)]
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub struct Bucket {
  pub label: String,
  pub time: i64,
  pub commits: usize,
  // The newest commit of the period (of an earlier one when the period is empty).
  pub oid: String,
  pub summary: String,
  pub bytes: u64,
  pub files: u64,
  pub lines: u64,
}

impl Bucket {
  #[cfg_attr(not(feature = "gui"), allow(dead_code))]
  pub fn value(&self, m: Metric) -> u64 {
    match m {
      Metric::Bytes => self.bytes,
      Metric::Files => self.files,
      Metric::Lines => self.lines,
    }
  }
}

fn day_of(t: i64) -> NaiveDate {
  Local
    .timestamp_opt(t, 0)
    .single()
    .map_or(NaiveDate::MIN, |d| d.date_naive())
}

fn start_of(day: NaiveDate, unit: Unit) -> NaiveDate {
  match unit {
    Unit::Week => day - Duration::days(day.weekday().num_days_from_monday() as i64),
    Unit::Month => day.with_day(1).unwrap_or(day),
    _ => day,
  }
}

fn next_start(day: NaiveDate, unit: Unit) -> NaiveDate {
  match unit {
    Unit::Week => day + Duration::days(7),
    Unit::Month => {
      let (y, m) = if day.month() == 12 {
        (day.year() + 1, 1)
      } else {
        (day.year(), day.month() + 1)
      };
      NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(day + Duration::days(31))
    }
    _ => day + Duration::days(1),
  }
}

fn label(day: NaiveDate, unit: Unit) -> String {
  match unit {
    Unit::Month => day.format("%Y-%m").to_string(),
    _ => day.format("%Y-%m-%d").to_string(),
  }
}

// One entry per period from the first commit to the last, newest value of each period; periods
// without commits repeat the previous totals. Points must be oldest first.
pub fn buckets(points: &[Point], unit: Unit) -> Vec<Bucket> {
  let mut out: Vec<Bucket> = Vec::new();
  let mut i = 0;
  let Some(first) = points.first() else { return out };
  let last_day = day_of(points[points.len() - 1].time);
  let mut start = start_of(day_of(first.time), unit);
  while start <= last_day {
    let end = next_start(start, unit);
    let mut commits = 0;
    while i < points.len() && day_of(points[i].time) < end {
      commits += 1;
      i += 1;
    }
    let newest = i.checked_sub(1).map(|k| &points[k]);
    let time = Local
      .from_local_datetime(&start.and_hms_opt(0, 0, 0).unwrap_or_default())
      .earliest()
      .map_or(0, |d| d.timestamp());
    out.push(Bucket {
      label: label(start, unit),
      time,
      commits,
      oid: newest.map(|p| p.oid.clone()).unwrap_or_default(),
      summary: newest.map(|p| p.summary.clone()).unwrap_or_default(),
      bytes: newest.map_or(0, |p| p.bytes),
      files: newest.map_or(0, |p| p.files),
      lines: newest.map_or(0, |p| p.lines),
    });
    start = end;
  }
  out
}

// At most `rows` entries, evenly spread and always including the newest.
pub fn thin<T: Clone>(items: &[T], rows: usize) -> Vec<T> {
  if rows < 2 || items.len() <= rows {
    return items.to_vec();
  }
  (0..rows)
    .map(|k| items[k * (items.len() - 1) / (rows - 1)].clone())
    .collect()
}
