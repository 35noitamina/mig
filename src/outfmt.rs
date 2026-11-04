// Makes command output cheap to read: terminal noise is removed, repeats are folded, and the
// caller can keep only the lines it cares about (a regex, the first or last lines, errors only).
use anyhow::{Result, anyhow};
use regex::Regex;
use serde_json::Value;

const MAX_LINE: usize = 400;
// Without a filter, longer output is cut to the first and the last lines.
const PLAIN_HEAD: usize = 40;
const PLAIN_TAIL: usize = 110;
const ERROR_WORDS: &str = r"(?i)\b(error|errors|warning|warn|fail|failed|failure|failures|panic|panicked|traceback|exception|fatal|assert|assertion|denied|cannot|can't|unresolved)\b|^\s*-->|^E\s{2,}|^FAILED|✗|✘";

#[derive(Default, Clone)]
pub struct Filter {
  pub grep: Option<String>,
  pub invert: bool,
  pub context: Option<usize>,
  pub head: Option<usize>,
  pub tail: Option<usize>,
  // On success, say only how much there was.
  pub quiet: bool,
  pub preset: Option<String>,
}

fn num(v: &Value, k: &str) -> Option<usize> {
  v.get(k).and_then(Value::as_u64).map(|n| n as usize)
}

impl Filter {
  pub fn from_args(v: &Value) -> Result<Filter> {
    let f = Filter {
      grep: v.get("grep").and_then(Value::as_str).map(str::to_string),
      invert: v.get("invert").and_then(Value::as_bool).unwrap_or(false),
      context: num(v, "grep_context"),
      head: num(v, "head"),
      tail: num(v, "tail"),
      quiet: v.get("quiet").and_then(Value::as_bool).unwrap_or(false),
      preset: v.get("preset").and_then(Value::as_str).map(str::to_string),
    };
    if let Some(g) = &f.grep {
      Regex::new(g).map_err(|e| anyhow!("grep is not a valid regular expression: {e}"))?;
    }
    if let Some(p) = &f.preset
      && p != "errors"
    {
      return Err(anyhow!("unknown preset {p}; the only one is \"errors\""));
    }
    Ok(f)
  }

  fn regex(&self) -> Option<Regex> {
    match (&self.grep, &self.preset) {
      (Some(g), _) => Regex::new(g).ok(),
      (None, Some(_)) => Regex::new(ERROR_WORDS).ok(),
      _ => None,
    }
  }
}

// Removes colour codes and other terminal escape sequences.
pub fn strip_ansi(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  let mut it = s.chars().peekable();
  while let Some(c) = it.next() {
    if c != '\u{1b}' {
      if c != '\u{7}' && (!c.is_control() || c == '\t') {
        out.push(c);
      }
      continue;
    }
    match it.peek().copied() {
      // CSI: ESC [ params final-byte
      Some('[') => {
        it.next();
        for n in it.by_ref() {
          if ('@'..='~').contains(&n) {
            break;
          }
        }
      }
      // OSC: ESC ] … BEL or ESC \
      Some(']') => {
        it.next();
        while let Some(n) = it.next() {
          if n == '\u{7}' {
            break;
          }
          if n == '\u{1b}' {
            it.next();
            break;
          }
        }
      }
      Some(_) => {
        it.next();
      }
      None => {}
    }
  }
  out
}

pub fn clip_line(s: &str) -> String {
  if s.chars().count() <= MAX_LINE {
    return s.to_string();
  }
  let head: String = s.chars().take(MAX_LINE).collect();
  format!("{head}… (+{} characters)", s.chars().count() - MAX_LINE)
}

// Equal lines in a row become one line with a count.
pub fn fold_repeats(lines: &[String]) -> Vec<String> {
  let mut out: Vec<String> = Vec::new();
  let mut i = 0;
  while i < lines.len() {
    let mut j = i + 1;
    while j < lines.len() && lines[j] == lines[i] {
      j += 1;
    }
    if j - i >= 3 {
      out.push(format!("{}  (×{})", lines[i], j - i));
    } else {
      out.extend(lines[i..j].iter().cloned());
    }
    i = j;
  }
  out
}

pub struct Shown {
  pub text: String,
  // Lines of the cleaned output, and how many of them were shown.
  pub total: usize,
  pub shown: usize,
}

// The text to hand back. `ok` says whether the program succeeded (quiet only hides a success).
pub fn render(raw: &[String], f: &Filter, ok: bool, limit_bytes: usize) -> Shown {
  render_from(raw, f, ok, limit_bytes, 0)
}

// `base` is how many lines came before `raw` in the whole output, so line numbers stay true.
pub fn render_from(raw: &[String], f: &Filter, ok: bool, limit_bytes: usize, base: usize) -> Shown {
  let re = f.regex();
  // Folding changes line numbers, so it is only done when no numbers are shown.
  let lines = if re.is_some() { raw.to_vec() } else { fold_repeats(raw) };
  let total = lines.len();
  if f.quiet && ok {
    return Shown {
      text: String::new(),
      total,
      shown: 0,
    };
  }
  let mut picked: Vec<(usize, &String)>;
  let mut note = String::new();
  match &re {
    Some(re) => {
      let hit: Vec<bool> = lines.iter().map(|l| re.is_match(l) != f.invert).collect();
      let n = hit.iter().filter(|h| **h).count();
      let ctx = f
        .context
        .unwrap_or(if f.preset.is_some() && f.grep.is_none() { 2 } else { 0 });
      let mut keep = vec![false; total];
      for (i, h) in hit.iter().enumerate() {
        if *h {
          for k in i.saturating_sub(ctx)..=(i + ctx).min(total.saturating_sub(1)) {
            keep[k] = true;
          }
        }
      }
      picked = lines.iter().enumerate().filter(|(i, _)| keep[*i]).collect();
      note = format!("{n} of {total} lines match");
      if n == 0 && !ok {
        // A failure must never come back empty.
        let from = total.saturating_sub(15);
        picked = lines.iter().enumerate().skip(from).collect();
        note = format!("no line matched; the last {} of {total} lines", total - from);
      } else if n == 0 {
        note = format!("no line matched ({total} lines)");
      }
    }
    None => picked = lines.iter().enumerate().collect(),
  }
  if let Some(h) = f.head {
    picked.truncate(h);
  }
  if let Some(t) = f.tail {
    let skip = picked.len().saturating_sub(t);
    picked.drain(..skip);
  }
  let mut gap = 0;
  if re.is_none() && f.head.is_none() && f.tail.is_none() && picked.len() > PLAIN_HEAD + PLAIN_TAIL {
    gap = picked.len() - PLAIN_HEAD - PLAIN_TAIL;
    picked.drain(PLAIN_HEAD..PLAIN_HEAD + gap);
  }
  let numbered = re.is_some();
  let mut out = String::new();
  let mut last: Option<usize> = None;
  for (i, l) in &picked {
    if let Some(p) = last
      && *i > p + 1
    {
      if numbered {
        out.push_str("--\n");
      } else if gap > 0 {
        out.push_str(&format!("… {gap} lines left out …\n"));
      }
    }
    if numbered {
      out.push_str(&format!("{}: {l}\n", base + i + 1));
    } else {
      out.push_str(l);
      out.push('\n');
    }
    last = Some(*i);
  }
  let shown = picked.len();
  if out.len() > limit_bytes {
    out = middle(&out, limit_bytes);
  }
  if !note.is_empty() {
    out = format!("[{note}]\n{out}");
  } else if (f.head.is_some() || f.tail.is_some()) && shown < total {
    out = format!("[{shown} of {total} lines]\n{out}");
  }
  Shown {
    text: out,
    total,
    shown,
  }
}

// Keeps the start and the end of a long text.
pub fn middle(s: &str, limit: usize) -> String {
  if s.len() <= limit {
    return s.to_string();
  }
  let (a, b) = (limit / 3, limit - limit / 3);
  let mut x = a;
  while !s.is_char_boundary(x) {
    x -= 1;
  }
  let mut y = s.len() - b;
  while !s.is_char_boundary(y) {
    y += 1;
  }
  format!("{}\n… {} bytes left out …\n{}", &s[..x], y - x, &s[y..])
}

#[cfg(test)]
mod tests {
  use super::*;

  fn v(s: &str) -> Vec<String> {
    s.lines().map(str::to_string).collect()
  }

  #[test]
  fn ansi_is_removed() {
    assert_eq!(strip_ansi("\u{1b}[31merror\u{1b}[0m: x"), "error: x");
    assert_eq!(strip_ansi("a\u{1b}]0;title\u{7}b"), "ab");
  }

  #[test]
  fn repeats_fold() {
    let out = fold_repeats(&v("a\nb\nb\nb\nb\nc\nc"));
    assert_eq!(out, vec!["a", "b  (×4)", "c", "c"]);
  }

  #[test]
  fn grep_with_context_numbers_lines() {
    let f = Filter {
      grep: Some("boom".into()),
      context: Some(1),
      ..Default::default()
    };
    let s = render(&v("a\nb\nboom\nc\nd\ne\nboom"), &f, false, 10_000);
    assert!(s.text.contains("3: boom") && s.text.contains("2: b") && s.text.contains("--"));
    assert!(s.text.starts_with("[2 of 7 lines match]"));
  }

  #[test]
  fn errors_preset_keeps_failures_and_never_returns_nothing_on_failure() {
    let f = Filter {
      preset: Some("errors".into()),
      ..Default::default()
    };
    let text = "Compiling a\nCompiling b\nerror[E0425]: cannot find\n --> src/x.rs:3:1\nCompiling c";
    let s = render(&v(text), &f, false, 10_000);
    assert!(s.text.contains("E0425") && s.text.contains("src/x.rs"));
    let none = render(&v("one\ntwo"), &f, false, 10_000);
    assert!(none.text.contains("two"));
  }

  #[test]
  fn quiet_hides_only_success() {
    let f = Filter {
      quiet: true,
      ..Default::default()
    };
    assert!(render(&v("x\ny"), &f, true, 100).text.is_empty());
    assert!(!render(&v("x\ny"), &f, false, 100).text.is_empty());
  }

  #[test]
  fn head_and_tail() {
    let f = Filter {
      tail: Some(2),
      ..Default::default()
    };
    let s = render(&v("1\n2\n3\n4"), &f, true, 100);
    assert_eq!(s.text, "[2 of 4 lines]\n3\n4\n");
  }
}
