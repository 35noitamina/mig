// File tools for MCP clients. Compared with a plain filesystem server they honour .gitignore, read
// by line range, search file contents, edit by exact match (all or nothing), keep secrets out of
// both directions, and checkpoint every change when an agent session is running.
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use git2::{DiffOptions, Patch, Repository};
use globset::Glob;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::Value;

use crate::{agent, guard, live, repo, scan};

const DEFAULT_LINES: usize = 400;
const MAX_LINES: usize = 2000;
const MAX_OUT: usize = 60_000;
const MAX_DIFF: usize = 8_000;
const MAX_READ: u64 = 8 << 20;
const MAX_SEARCH_FILE: u64 = 2 << 20;
const MAX_WRITE: usize = 2 << 20;
const LS_MAX: usize = 500;
const LINE_CLIP: usize = 300;
const READ_CLIP: usize = 2000;
const MASK: &str = "[REDACTED]";

fn text(v: &Value, key: &str) -> Option<String> {
  v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn number(v: &Value, key: &str) -> Option<usize> {
  v.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

fn flag(v: &Value, key: &str) -> bool {
  v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn required(v: &Value, key: &str) -> Result<String> {
  text(v, key).ok_or_else(|| anyhow!("{key} is required"))
}

fn root(repo: &Repository) -> Result<PathBuf> {
  repo
    .workdir()
    .map(Path::to_path_buf)
    .ok_or_else(|| anyhow!("this repository has no working folder"))
}

// A path inside the repository: no `..`, nothing under .git, and no symlink leading outside.
fn resolve(root: &Path, input: &str) -> Result<PathBuf> {
  let given = Path::new(input);
  let relative = if given.is_absolute() {
    given
      .strip_prefix(root)
      .map_err(|_| anyhow!("{input} is outside the repository"))?
  } else {
    given
  };
  let mut out = root.to_path_buf();
  for c in relative.components() {
    match c {
      Component::Normal(n) if n == ".git" => bail!(".git is not accessible"),
      Component::Normal(n) => out.push(n),
      Component::CurDir => {}
      _ => bail!("{input}: only paths inside the repository are allowed"),
    }
  }
  let mut ancestor = out.as_path();
  while !ancestor.exists() {
    match ancestor.parent() {
      Some(p) => ancestor = p,
      None => break,
    }
  }
  if let (Ok(a), Ok(r)) = (ancestor.canonicalize(), root.canonicalize())
    && !a.starts_with(r)
  {
    bail!("{input} leads outside the repository");
  }
  Ok(out)
}

fn rel(root: &Path, p: &Path) -> String {
  p.strip_prefix(root)
    .unwrap_or(p)
    .components()
    .map(|c| c.as_os_str().to_string_lossy())
    .collect::<Vec<_>>()
    .join("/")
}

fn is_binary(bytes: &[u8]) -> bool {
  bytes[..bytes.len().min(8000)].contains(&0)
}

fn clip(line: &str, max: usize) -> String {
  if line.chars().count() <= max {
    return line.to_string();
  }
  let head: String = line.chars().take(max).collect();
  format!("{head}… (+{} chars)", line.chars().count() - max)
}

fn after_write(repo: &Repository, message: &str) -> String {
  if agent::session(repo).is_none() {
    return "no agent session, so mig cannot undo this (agent_start first)".into();
  }
  match agent::checkpoint(repo, message) {
    Ok(Some(o)) => format!("checkpoint {}", repo::short(o)),
    Ok(None) => String::new(),
    Err(e) => format!("checkpoint failed: {e:#}"),
  }
}

fn with_note(summary: String, note: String) -> String {
  if note.is_empty() {
    summary
  } else {
    format!("{summary}\n{note}")
  }
}

pub fn ls(repo: &Repository, args: &Value) -> Result<String> {
  let root = root(repo)?;
  let path = text(args, "path").unwrap_or_else(|| ".".into());
  let base = resolve(&root, &path)?;
  if !base.is_dir() {
    bail!("{path} is not a folder");
  }
  let depth = number(args, "depth").unwrap_or(1).clamp(1, 6);
  let status: HashMap<String, char> = repo::changes(repo)?.into_iter().map(|c| (c.path, c.kind)).collect();
  let walk = WalkBuilder::new(&base)
    .max_depth(Some(depth))
    .hidden(false)
    .filter_entry(|e| e.file_name() != ".git")
    .sort_by_file_path(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.cmp(b)))
    .build();
  let mut lines = Vec::new();
  let mut total = 0;
  for entry in walk.flatten().skip(1) {
    total += 1;
    if lines.len() >= LS_MAX {
      continue;
    }
    let indent = "  ".repeat(entry.depth().saturating_sub(1));
    let name = entry.file_name().to_string_lossy();
    let r = rel(&root, entry.path());
    if entry.file_type().is_some_and(|t| t.is_dir()) {
      lines.push(format!("{indent}{name}/"));
    } else {
      let size = entry.metadata().map(|m| guard::fmt_size(m.len())).unwrap_or_default();
      let mark = status.get(&r).map(|k| format!("  {k}")).unwrap_or_default();
      lines.push(format!("{indent}{name}  {size}{mark}"));
    }
  }
  let mut out = format!(
    "{}  {total} entries\n{}",
    if path == "." { "./" } else { &path },
    lines.join("\n")
  );
  if total > LS_MAX {
    out.push_str(&format!("\n… {} more; list a subfolder", total - LS_MAX));
  }
  Ok(out)
}

fn read_one(root: &Path, input: &str, from: Option<usize>, to: Option<usize>, budget: usize) -> Result<String> {
  let path = resolve(root, input)?;
  let meta = std::fs::metadata(&path).map_err(|_| anyhow!("{input}: no such file"))?;
  if meta.is_dir() {
    bail!("{input}: it is a folder; use ls");
  }
  let name = rel(root, &path);
  if meta.len() > MAX_READ {
    return Ok(format!(
      "{name}  {}  too large to read; use search",
      guard::fmt_size(meta.len())
    ));
  }
  let bytes = std::fs::read(&path)?;
  if is_binary(&bytes) {
    return Ok(format!(
      "{name}  {}  binary file, not shown",
      guard::fmt_size(meta.len())
    ));
  }
  let content = String::from_utf8_lossy(&bytes);
  let lines: Vec<&str> = content.lines().collect();
  let n = lines.len();
  let from = from.unwrap_or(1).max(1);
  if n > 0 && from > n {
    bail!("{input}: the file has only {n} lines");
  }
  let to = to
    .unwrap_or(from + DEFAULT_LINES - 1)
    .min(from + MAX_LINES - 1)
    .min(n)
    .max(from);
  let mut body = String::new();
  let mut masked = 0;
  let mut last = from - 1;
  for (i, line) in lines.iter().enumerate().take(to).skip(from - 1) {
    let (safe, hits) = scan::redact(line);
    masked += hits;
    let row = format!("{:>5}  {}\n", i + 1, clip(&safe, READ_CLIP));
    if body.len() + row.len() > budget {
      break;
    }
    body.push_str(&row);
    last = i + 1;
  }
  let mut head = format!("{name}  {n} lines  {}", guard::fmt_size(meta.len()));
  if from > 1 || last < n {
    head.push_str(&format!("  (lines {from}-{last})"));
  }
  if masked > 0 {
    head.push_str(&format!("  [{masked} secret(s) masked as {MASK}]"));
  }
  let mut out = format!("{head}\n{body}");
  if last < n {
    out.push_str(&format!(
      "… {} more lines; read again with from={}\n",
      n - last,
      last + 1
    ));
  }
  Ok(out)
}

pub fn read(repo: &Repository, args: &Value) -> Result<String> {
  let root = root(repo)?;
  read_one(
    &root,
    &required(args, "path")?,
    number(args, "from"),
    number(args, "to"),
    MAX_OUT,
  )
}

pub fn read_many(repo: &Repository, args: &Value) -> Result<String> {
  let root = root(repo)?;
  let files = args
    .get("files")
    .and_then(Value::as_array)
    .ok_or_else(|| anyhow!("files is required"))?;
  let mut out = String::new();
  for f in files {
    let (path, from, to) = match f {
      Value::String(p) => (p.clone(), None, None),
      v => (required(v, "path")?, number(v, "from"), number(v, "to")),
    };
    if out.len() >= MAX_OUT {
      out.push_str(&format!("{path}  skipped (output limit reached)\n\n"));
      continue;
    }
    match read_one(&root, &path, from, to, MAX_OUT - out.len()) {
      Ok(s) => out.push_str(&s),
      Err(e) => out.push_str(&format!("{e:#}\n")),
    }
    out.push('\n');
  }
  Ok(out.trim_end().to_string())
}

pub fn search(repo: &Repository, args: &Value) -> Result<String> {
  let root = root(repo)?;
  let base = resolve(&root, &text(args, "path").unwrap_or_else(|| ".".into()))?;
  let re = RegexBuilder::new(&required(args, "pattern")?)
    .case_insensitive(flag(args, "ignore_case"))
    .build()?;
  let glob = text(args, "glob")
    .map(|g| Glob::new(&g))
    .transpose()?
    .map(|g| g.compile_matcher());
  let context = number(args, "context").unwrap_or(0).min(5);
  let max = number(args, "max").unwrap_or(50).clamp(1, 500);
  let walk = WalkBuilder::new(&base)
    .hidden(false)
    .filter_entry(|e| e.file_name() != ".git")
    .sort_by_file_path(|a, b| a.cmp(b))
    .build();
  let (mut out, mut found, mut files, mut capped) = (String::new(), 0, 0, false);
  'files: for entry in walk.flatten() {
    if !entry.file_type().is_some_and(|t| t.is_file()) {
      continue;
    }
    let name = rel(&root, entry.path());
    if glob.as_ref().is_some_and(|g| !g.is_match(&name)) {
      continue;
    }
    if entry.metadata().map_or(true, |m| m.len() > MAX_SEARCH_FILE) {
      continue;
    }
    let Ok(bytes) = std::fs::read(entry.path()) else {
      continue;
    };
    if is_binary(&bytes) {
      continue;
    }
    let content = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = content.lines().collect();
    let hits: Vec<usize> = (0..lines.len()).filter(|&i| re.is_match(lines[i])).collect();
    if hits.is_empty() {
      continue;
    }
    files += 1;
    let mut printed = 0;
    for &i in &hits {
      if found >= max {
        capped = true;
        break 'files;
      }
      found += 1;
      let (start, end) = (
        i.saturating_sub(context).max(printed),
        (i + context).min(lines.len() - 1),
      );
      if printed > 0 && start > printed {
        out.push_str("--\n");
      }
      for (k, line) in lines.iter().enumerate().take(end + 1).skip(start) {
        let sep = if hits.contains(&k) { ':' } else { '-' };
        let (safe, _) = scan::redact(line);
        out.push_str(&format!("{name}{sep}{}{sep}{}\n", k + 1, clip(&safe, LINE_CLIP)));
      }
      printed = end + 1;
    }
  }
  if found == 0 {
    return Ok("no matches".into());
  }
  out.push_str(&format!(
    "{found} match(es) in {files} file(s){}",
    if capped {
      "; limit reached, narrow with path or glob"
    } else {
      ""
    }
  ));
  Ok(out)
}

fn to_crlf(s: &str) -> String {
  s.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn line_of(text: &str, at: usize) -> usize {
  text[..at].matches('\n').count() + 1
}

struct Edit {
  old: String,
  new: String,
  all: bool,
}

fn parse_edits(args: &Value) -> Result<Vec<Edit>> {
  let list = args
    .get("edits")
    .and_then(Value::as_array)
    .ok_or_else(|| anyhow!("edits is required"))?;
  if list.is_empty() {
    bail!("edits is empty");
  }
  list
    .iter()
    .map(|e| {
      Ok(Edit {
        old: required(e, "old")?,
        new: text(e, "new").unwrap_or_default(),
        all: flag(e, "all"),
      })
    })
    .collect()
}

// Every edit must match exactly (once, unless `all`); otherwise nothing is changed.
fn apply_edits(original: &str, edits: &[Edit]) -> Result<String> {
  let crlf = original.contains("\r\n");
  let mut cur = original.to_string();
  for (k, e) in edits.iter().enumerate() {
    let (old, new) = if crlf {
      (to_crlf(&e.old), to_crlf(&e.new))
    } else {
      (e.old.clone(), e.new.clone())
    };
    if old.is_empty() {
      bail!("edit {}: old is empty", k + 1);
    }
    let at: Vec<usize> = cur.match_indices(&old).map(|(i, _)| i).collect();
    if at.is_empty() {
      let mut msg = format!("edit {}: the text was not found; nothing was changed.", k + 1);
      if e.old.contains(MASK) {
        msg.push_str(&format!(
          " It contains {MASK}, which stands for a masked secret; edit around that line."
        ));
      } else if let Some(first) = old.lines().map(str::trim).find(|l| !l.is_empty())
        && let Some(i) = cur.find(first)
      {
        msg.push_str(&format!(
          " Its first line does appear at line {}; check the rest and the whitespace.",
          line_of(&cur, i)
        ));
      }
      bail!(msg);
    }
    if at.len() > 1 && !e.all {
      let lines: Vec<String> = at.iter().take(6).map(|&i| line_of(&cur, i).to_string()).collect();
      bail!(
        "edit {}: the text occurs {} times (lines {}); add more surrounding text or set all=true. Nothing was changed.",
        k + 1,
        at.len(),
        lines.join(", ")
      );
    }
    cur = if e.all {
      cur.replace(&old, &new)
    } else {
      cur.replacen(&old, &new, 1)
    };
  }
  Ok(cur)
}

fn refuse_secrets(repo: &Repository, name: &str, added: &str) -> Result<()> {
  let cfg = scan::Config::load(repo)?;
  let mut found = Vec::new();
  scan::scan_text(&cfg, name, added, &mut found);
  if found.is_empty() {
    return Ok(());
  }
  bail!(
    "refused: the new text looks like it contains a secret ({}). Use an environment variable or placeholder, or add `mig:allow` on that line if it is intended. Nothing was written.",
    found
      .iter()
      .map(|f| format!("{} at {}", f.rule, f.masked))
      .collect::<Vec<_>>()
      .join(", ")
  )
}

fn diff(old: &str, new: &str, name: &str) -> Result<(String, usize, usize)> {
  let mut opts = DiffOptions::new();
  opts.context_lines(2);
  let mut patch = Patch::from_buffers(
    old.as_bytes(),
    Some(Path::new(name)),
    new.as_bytes(),
    Some(Path::new(name)),
    Some(&mut opts),
  )?;
  let (_, added, removed) = patch.line_stats()?;
  let buf = patch.to_buf()?;
  let mut text = buf.as_str().unwrap_or("").to_string();
  if text.len() > MAX_DIFF {
    let mut cut = MAX_DIFF;
    while !text.is_char_boundary(cut) {
      cut -= 1;
    }
    text.truncate(cut);
    text.push_str("\n… diff cut");
  }
  Ok((text, added, removed))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
  if let Some(dir) = path.parent() {
    std::fs::create_dir_all(dir)?;
  }
  let name = path
    .file_name()
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_default();
  let tmp = path.with_file_name(format!(".{name}.mig-tmp"));
  std::fs::write(&tmp, bytes)?;
  if let Ok(m) = std::fs::metadata(path) {
    let _ = std::fs::set_permissions(&tmp, m.permissions());
  }
  std::fs::rename(&tmp, path).inspect_err(|_| {
    let _ = std::fs::remove_file(&tmp);
  })?;
  Ok(())
}

pub fn edit(repo: &Repository, args: &Value) -> Result<String> {
  let root = root(repo)?;
  let input = required(args, "path")?;
  let path = resolve(&root, &input)?;
  let name = rel(&root, &path);
  let bytes = std::fs::read(&path).map_err(|_| anyhow!("{input}: no such file"))?;
  let original = String::from_utf8(bytes).map_err(|_| anyhow!("{input} is not UTF-8 text"))?;
  let edits = parse_edits(args)?;
  let updated = apply_edits(&original, &edits)?;
  if updated == original {
    return Ok(format!("{name}: no change"));
  }
  refuse_secrets(
    repo,
    &name,
    &edits.iter().map(|e| e.new.as_str()).collect::<Vec<_>>().join("\n"),
  )?;
  let (patch, added, removed) = diff(&original, &updated, &name)?;
  if flag(args, "dry_run") {
    return Ok(format!("would edit {name}  +{added} -{removed}\n{patch}"));
  }
  live::record(repo, "mig edit", &name);
  write_atomic(&path, updated.as_bytes())?;
  let note = after_write(repo, &format!("edit {name}"));
  Ok(with_note(format!("edited {name}  +{added} -{removed}\n{patch}"), note))
}

pub fn write(repo: &Repository, args: &Value) -> Result<String> {
  let root = root(repo)?;
  let input = required(args, "path")?;
  let content = text(args, "content").ok_or_else(|| anyhow!("content is required"))?;
  if content.len() > MAX_WRITE {
    bail!(
      "content is {}; the limit is {}",
      guard::fmt_size(content.len() as u64),
      guard::fmt_size(MAX_WRITE as u64)
    );
  }
  let path = resolve(&root, &input)?;
  if path.is_dir() {
    bail!("{input} is a folder");
  }
  let name = rel(&root, &path);
  refuse_secrets(repo, &name, &content)?;
  let previous = std::fs::read(&path).ok();
  live::record(repo, "mig write", &name);
  write_atomic(&path, content.as_bytes())?;
  let summary = match previous.map(String::from_utf8) {
    None => format!(
      "created {name}  {} lines  {}",
      content.lines().count(),
      guard::fmt_size(content.len() as u64)
    ),
    Some(Ok(old)) => {
      let (_, added, removed) = diff(&old, &content, &name)?;
      format!("overwrote {name}  +{added} -{removed}")
    }
    Some(Err(_)) => format!("overwrote {name} (was binary)"),
  };
  Ok(with_note(summary, after_write(repo, &format!("write {name}"))))
}

pub fn move_path(repo: &Repository, args: &Value) -> Result<String> {
  let root = root(repo)?;
  let (from, to) = (required(args, "from")?, required(args, "to")?);
  let (src, dst) = (resolve(&root, &from)?, resolve(&root, &to)?);
  if !src.exists() {
    bail!("{from}: no such file or folder");
  }
  if dst.exists() {
    bail!("{to} already exists");
  }
  if let Some(dir) = dst.parent() {
    std::fs::create_dir_all(dir)?;
  }
  live::record(repo, "mig move", &rel(&root, &src));
  live::record(repo, "mig move", &rel(&root, &dst));
  std::fs::rename(&src, &dst)?;
  let summary = format!("moved {} -> {}", rel(&root, &src), rel(&root, &dst));
  Ok(with_note(summary, after_write(repo, &format!("move {from}"))))
}

pub fn delete(repo: &Repository, args: &Value) -> Result<String> {
  let root = root(repo)?;
  let input = required(args, "path")?;
  let path = resolve(&root, &input)?;
  if path == root {
    bail!("refusing to delete the repository root");
  }
  let name = rel(&root, &path);
  live::record(repo, "mig delete", &name);
  let summary = if path.is_dir() {
    let entries = std::fs::read_dir(&path)?.count();
    if entries > 0 && !flag(args, "recursive") {
      bail!("{input} is a folder with {entries} entries; set recursive=true to delete it");
    }
    std::fs::remove_dir_all(&path)?;
    format!("deleted folder {name}")
  } else if path.exists() {
    std::fs::remove_file(&path)?;
    format!("deleted {name}")
  } else {
    bail!("{input}: no such file or folder");
  };
  Ok(with_note(summary, after_write(repo, &format!("delete {name}"))))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn edits(list: &[(&str, &str, bool)]) -> Vec<Edit> {
    list
      .iter()
      .map(|(o, n, a)| Edit {
        old: o.to_string(),
        new: n.to_string(),
        all: *a,
      })
      .collect()
  }

  #[test]
  fn paths_stay_inside() {
    let root = std::env::temp_dir();
    assert!(resolve(&root, "a/b.txt").is_ok());
    assert!(resolve(&root, "../x").is_err());
    assert!(resolve(&root, "a/../../x").is_err());
    assert!(resolve(&root, ".git/config").is_err());
    assert!(resolve(&root, "/etc/passwd").is_err());
  }

  #[test]
  fn edit_needs_one_match() {
    let t = "one\ntwo\none\n";
    assert!(
      apply_edits(t, &edits(&[("one", "1", false)]))
        .unwrap_err()
        .to_string()
        .contains("lines 1, 3")
    );
    assert_eq!(apply_edits(t, &edits(&[("one", "1", true)])).unwrap(), "1\ntwo\n1\n");
    assert_eq!(apply_edits(t, &edits(&[("two", "2", false)])).unwrap(), "one\n2\none\n");
    assert!(apply_edits(t, &edits(&[("three", "3", false)])).is_err());
  }

  #[test]
  fn edits_are_all_or_nothing_in_order() {
    let t = "a\nb\n";
    assert_eq!(
      apply_edits(t, &edits(&[("a", "b", false), ("b\nb", "c", false)])).unwrap(),
      "c\n"
    );
    assert!(apply_edits(t, &edits(&[("a", "x", false), ("zzz", "y", false)])).is_err());
  }

  #[test]
  fn crlf_files_keep_their_line_endings() {
    let t = "a\r\nb\r\nc\r\n";
    assert_eq!(
      apply_edits(t, &edits(&[("a\nb", "x\ny", false)])).unwrap(),
      "x\r\ny\r\nc\r\n"
    );
  }

  #[test]
  fn masked_text_is_explained() {
    let e = apply_edits("key = abc\n", &edits(&[("key = [REDACTED]", "key = x", false)])).unwrap_err();
    assert!(e.to_string().contains("masked secret"));
  }
}
