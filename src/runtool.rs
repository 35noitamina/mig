// The run tools: check what an agent wants to run, run it, and keep a record. What a command changes
// in the working folder is noted as the agent's own work, so finishing or aborting the agent's
// session covers it too.
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use git2::Repository;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::{self, Answer, Pending};
use crate::config::{self, RunSection};
use crate::outfmt::{self, Filter};
use crate::runcheck::{self, Lang, Level, Report, Spec, Verdict};
use crate::runner::{self, Params, TempScript};
use crate::{agent, live, outlog, repo};

const MAX_TRACKED: u64 = 2 << 20;
const LOG_KEEP: usize = 500;
const LOG_ROTATE_AT: u64 = 256 * 1024;
const TAIL_KEEP: usize = 2000;

pub enum Kind {
  Argv(Vec<String>),
  Script {
    lang: Lang,
    name: String,
    code: String,
    args: Vec<String>,
  },
}

pub struct Request {
  pub kind: Kind,
  pub cwd: Option<String>,
  pub timeout: Option<u64>,
  pub filter: Filter,
  // Wait this long for the end; if it has not come, hand back what there is and keep running.
  pub wait_secs: Option<u64>,
}

// Called with the lines a run has just printed (clean and masked).
pub type LineSink = Arc<dyn Fn(Vec<String>) + Send + Sync>;

#[derive(Serialize, Deserialize, Clone)]
pub struct LogEntry {
  pub ts: i64,
  pub agent: Option<String>,
  pub kind: String,
  pub summary: String,
  pub verdict: String,
  pub findings: Vec<String>,
  pub exit: Option<i32>,
  pub timed_out: bool,
  pub secs: f64,
  pub tail: String,
  // The id of the stored output (see outlog).
  #[serde(default)]
  pub out: String,
  // The mig process that ran it, so a server can tell its own runs from other people's.
  #[serde(default)]
  pub pid: u32,
}

fn log_path(git_dir: &Path) -> PathBuf {
  git_dir.join("mig").join("run.jsonl")
}

fn read_log(git_dir: &Path) -> Vec<LogEntry> {
  std::fs::read_to_string(log_path(git_dir))
    .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
    .unwrap_or_default()
}

// Newest last.
pub fn recent(git_dir: &Path, n: usize) -> Vec<LogEntry> {
  let all = read_log(git_dir);
  all[all.len().saturating_sub(n)..].to_vec()
}

fn append_log(git_dir: &Path, e: &LogEntry) {
  let file = log_path(git_dir);
  if let Some(dir) = file.parent() {
    let _ = std::fs::create_dir_all(dir);
  }
  if std::fs::metadata(&file).is_ok_and(|m| m.len() > LOG_ROTATE_AT) {
    let all = read_log(git_dir);
    let tail: Vec<String> = all[all.len().saturating_sub(LOG_KEEP)..]
      .iter()
      .filter_map(|e| serde_json::to_string(e).ok())
      .collect();
    let _ = std::fs::write(&file, tail.join("\n") + "\n");
  }
  if let (Ok(line), Ok(mut f)) = (
    serde_json::to_string(e),
    std::fs::OpenOptions::new().create(true).append(true).open(&file),
  ) {
    let _ = writeln!(f, "{line}");
  }
}

fn tail_of(s: &str) -> String {
  let skip = s.chars().count().saturating_sub(TAIL_KEEP);
  s.chars().skip(skip).collect()
}

fn strings(v: &Value, key: &str) -> Vec<String> {
  v.get(key)
    .and_then(Value::as_array)
    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
    .unwrap_or_default()
}

pub fn parse(args: &Value, script: bool) -> Result<Request> {
  let kind = if script {
    let name = args
      .get("lang")
      .and_then(Value::as_str)
      .ok_or_else(|| anyhow!("lang is required, for example \"python\", \"bash\", \"node\" or \"powershell\""))?
      .to_string();
    let lang = Lang::parse(&name)
      .ok_or_else(|| anyhow!("unknown lang {name}; use sh, bash, python, node, powershell, cmd, ruby or perl"))?;
    let code = args
      .get("code")
      .and_then(Value::as_str)
      .ok_or_else(|| anyhow!("code is required"))?
      .to_string();
    if code.trim().is_empty() {
      bail!("code is empty");
    }
    Kind::Script {
      lang,
      name,
      code,
      args: strings(args, "args"),
    }
  } else {
    let argv = strings(args, "argv");
    if argv.is_empty() {
      bail!(
        "argv is required: the program and its arguments as a list, e.g. [\"cargo\", \"test\"]. For pipes or several commands, use run_script."
      );
    }
    Kind::Argv(argv)
  };
  Ok(Request {
    kind,
    cwd: args.get("cwd").and_then(Value::as_str).map(str::to_string),
    timeout: args.get("timeout").and_then(Value::as_u64),
    filter: Filter::from_args(args)?,
    wait_secs: args.get("wait_secs").and_then(Value::as_u64),
  })
}

fn spec_of(kind: &Kind) -> Spec<'_> {
  match kind {
    Kind::Argv(a) => Spec::Argv(a),
    Kind::Script { lang, code, .. } => Spec::Script { lang: *lang, code },
  }
}

fn explain(report: &Report) -> String {
  report
    .findings
    .iter()
    .map(|f| format!("- {}", f.show()))
    .collect::<Vec<_>>()
    .join("\n")
}

pub fn check_only(repo: &Repository, req: &Request) -> Result<String> {
  let cfg = config::load(repo)?.run;
  let report = runcheck::check(&spec_of(&req.kind), &cfg, repo.workdir());
  let v = runcheck::verdict(&report, cfg.mode);
  let head = match v {
    Verdict::Run => "would run",
    Verdict::Ask => "would wait for a person's approval",
    Verdict::Block => "would be blocked",
  };
  Ok(if report.findings.is_empty() {
    format!("{head}; nothing suspicious found")
  } else {
    format!("{head}\n{}", explain(&report))
  })
}

struct Snap {
  content: Option<Vec<u8>>,
  skipped: bool,
}

fn read_tracked(root: &Path, path: &str) -> (Option<Vec<u8>>, bool) {
  let full = root.join(path);
  match std::fs::metadata(&full) {
    Ok(m) if m.is_file() && m.len() <= MAX_TRACKED => (std::fs::read(&full).ok(), false),
    Ok(m) if m.is_file() => (None, true),
    _ => (None, false),
  }
}

// What the changed files look like now, so a command's effect can be told apart later.
fn snapshot_dirty(repo: &Repository, root: &Path) -> HashMap<String, Snap> {
  let mut out = HashMap::new();
  for c in repo::changes(repo).unwrap_or_default() {
    let (content, skipped) = read_tracked(root, &c.path);
    out.insert(c.path, Snap { content, skipped });
  }
  out
}

fn head_content(repo: &Repository, path: &str) -> Option<Vec<u8>> {
  let tree = repo::head_commit(repo).ok().flatten()?.tree().ok()?;
  let entry = tree.get_path(Path::new(path)).ok()?;
  Some(repo.find_blob(entry.id()).ok()?.content().to_vec())
}

// Notes the files the command changed as the agent's own. Returns their names.
fn record_changes(repo: &Repository, root: &Path, agent_id: &str, before: &HashMap<String, Snap>) -> Vec<String> {
  let mut paths: BTreeSet<String> = before.keys().cloned().collect();
  paths.extend(repo::changes(repo).unwrap_or_default().into_iter().map(|c| c.path));
  let mut changed = Vec::new();
  for path in paths {
    let (now, skipped) = read_tracked(root, &path);
    let was = before.get(&path);
    if skipped || was.is_some_and(|w| w.skipped) {
      continue;
    }
    let original = match was {
      Some(w) => {
        if w.content == now {
          continue;
        }
        w.content.clone()
      }
      None => head_content(repo, &path),
    };
    if original == now {
      continue;
    }
    if agent::record_write(repo, agent_id, &path, original.as_deref(), now.as_deref()).is_ok() {
      changed.push(path);
    }
  }
  changed
}

// Some notes are worth saying once, not after every run.
static NOTE_ENV: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static NOTE_NO_AGENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn first_time(flag: &std::sync::atomic::AtomicBool) -> bool {
  !flag.swap(true, std::sync::atomic::Ordering::Relaxed)
}

pub struct Done {
  pub text: String,
}

fn summary_of(kind: &Kind) -> (String, &'static str) {
  match kind {
    Kind::Argv(a) => (a.join(" "), "argv"),
    Kind::Script { name, code, .. } => {
      let first = code.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
      (format!("{name} script: {first}"), "script")
    }
  }
}

fn blocked_text(report: &Report) -> String {
  format!(
    "blocked by mig; nothing was run.\n{}\nIf the person really wants this, they can run it themselves.",
    explain(report)
  )
}

// Everything decided before the program starts: checked, approved if need be, and resolved.
pub struct Prepared {
  entry: LogEntry,
  report: Report,
  root: PathBuf,
  cwd: PathBuf,
  program: PathBuf,
  args: Vec<String>,
  env: HashMap<String, String>,
  dropped: Vec<String>,
  timeout: Duration,
  limit: usize,
  filter: Filter,
  id: String,
  agent: Option<agent::Session>,
  _script: Option<TempScript>,
}

impl Prepared {
  pub fn summary(&self) -> &str {
    &self.entry.summary
  }

  // The id the output is stored under; known before the program starts.
  pub fn id(&self) -> &str {
    &self.id
  }
}

pub fn run(repo: &Repository, agent: Option<&agent::Session>, req: Request) -> Result<Done> {
  prepare(repo, agent, req)?.go(repo, None, None)
}

pub fn prepare(repo: &Repository, agent: Option<&agent::Session>, req: Request) -> Result<Prepared> {
  let root = repo
    .workdir()
    .ok_or_else(|| anyhow!("this repository has no working folder"))?
    .to_path_buf();
  let cfg: RunSection = config::load(repo)?.run;
  let cwd = runner::ensure_inside(&root, &root.join(req.cwd.as_deref().unwrap_or(".")))?;
  let report = runcheck::check(&spec_of(&req.kind), &cfg, Some(&root));
  let mut verdict = runcheck::verdict(&report, cfg.mode);
  let (summary, kind_name) = summary_of(&req.kind);
  let (code_text, lang_text) = match &req.kind {
    Kind::Argv(a) => (a.join(" "), None),
    Kind::Script { name, code, .. } => (code.clone(), Some(name.clone())),
  };
  let always = approval::key(kind_name, lang_text.as_deref(), &code_text);
  if verdict == Verdict::Ask && approval::is_always(repo.path(), &always) {
    verdict = Verdict::Run;
  }
  let mut entry = LogEntry {
    ts: live::now_ms(),
    agent: agent.map(|a| a.name.clone()),
    kind: kind_name.into(),
    summary: tail_of(&summary),
    verdict: String::new(),
    findings: report.findings.iter().map(|f| f.rule.to_string()).collect(),
    exit: None,
    timed_out: false,
    secs: 0.0,
    tail: String::new(),
    out: String::new(),
    pid: std::process::id(),
  };
  match verdict {
    Verdict::Block => {
      entry.verdict = "blocked".into();
      entry.tail = tail_of(&explain(&report));
      append_log(repo.path(), &entry);
      bail!("{}", blocked_text(&report));
    }
    Verdict::Ask => {
      let id = approval::new_id();
      let wait = cfg.approval_wait_secs.max(1);
      approval::create(
        repo.path(),
        &Pending {
          id: id.clone(),
          ts: chrono::Utc::now().timestamp(),
          agent: agent.map(|a| a.name.clone()),
          kind: kind_name.into(),
          lang: lang_text.clone(),
          code: code_text.clone(),
          cwd: cwd
            .strip_prefix(&root)
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
          notes: report.findings.iter().map(Into::into).collect(),
          wait_secs: wait,
          status: "pending".into(),
        },
      )?;
      let answer = approval::wait(repo.path(), &id, Duration::from_secs(wait));
      match answer {
        Answer::Approved => entry.findings.push("approved".into()),
        Answer::Denied => {
          entry.verdict = "denied".into();
          append_log(repo.path(), &entry);
          bail!("a person denied this. Nothing was run.\n{}", explain(&report));
        }
        Answer::TimedOut => {
          entry.verdict = "no answer".into();
          append_log(repo.path(), &entry);
          bail!(
            "nobody approved this within {wait} seconds, so nothing was run. It needs a person's approval (mig GUI, Approvals, or `mig run approve`).\n{}",
            explain(&report)
          );
        }
      }
    }
    Verdict::Run => {}
  }

  // Resolve what to start.
  let mut _script = None;
  let (program, args) = match &req.kind {
    Kind::Argv(a) => (runner::find_program(&a[0], &cwd)?, a[1..].to_vec()),
    Kind::Script { lang, name, code, args } => {
      let it = runner::interpreter(*lang, name)?;
      // A script written with CRLF breaks sh, bash, python and the like.
      let code = if matches!(lang, Lang::Cmd) {
        code.clone()
      } else {
        code.replace("\r\n", "\n")
      };
      let tmp = TempScript::write(repo.path(), it.ext, &code)?;
      let mut full = it.before.clone();
      full.push(tmp.path.to_string_lossy().into_owned());
      full.extend(args.iter().cloned());
      _script = Some(tmp);
      (it.program, full)
    }
  };
  let (env, dropped) = runner::clean_env(&cfg, std::env::vars());
  let timeout = Duration::from_secs(
    req
      .timeout
      .unwrap_or(cfg.timeout_secs)
      .clamp(1, cfg.max_timeout_secs.max(1)),
  );
  Ok(Prepared {
    entry,
    report,
    root,
    cwd,
    program,
    args,
    env,
    dropped,
    timeout,
    limit: cfg.output_limit_kb.max(1) * 1024,
    filter: req.filter.clone(),
    id: approval::new_id(),
    agent: agent.cloned(),
    _script,
  })
}

impl Prepared {
  pub fn go(self, repo: &Repository, lines: Option<LineSink>, cancel: Option<Arc<AtomicBool>>) -> Result<Done> {
    let Prepared {
      mut entry,
      report,
      root,
      cwd,
      program,
      args,
      env,
      dropped,
      timeout,
      limit,
      filter,
      id,
      agent,
      _script,
    } = self;
    let agent = agent.as_ref();
    let before = agent.map(|_| snapshot_dirty(repo, &root));
    let who = agent.map(|a| a.name.clone());
    live::record(repo, "mig run", ".", who.as_deref());
    entry.out = id.clone();
    let writer = Arc::new(outlog::create(repo.path(), &id, who.as_deref(), &entry.summary)?);
    let (w2, ls2) = (writer.clone(), lines.clone());
    let sink: runner::Sink = Arc::new(move |bytes: &[u8]| {
      let got = w2.feed(bytes);
      if !got.is_empty()
        && let Some(f) = &ls2
      {
        f(got);
      }
    });
    // A person can ask for a stop from the GUI; that arrives as a file next to the output.
    let cancel = cancel.unwrap_or_default();
    let over = Arc::new(AtomicBool::new(false));
    {
      let (git, id, cancel, over) = (repo.path().to_path_buf(), id.clone(), cancel.clone(), over.clone());
      std::thread::spawn(move || {
        while !over.load(std::sync::atomic::Ordering::Relaxed) {
          if outlog::stop_requested(&git, &id) {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            return;
          }
          std::thread::sleep(Duration::from_millis(300));
        }
      });
    }
    let result = runner::execute(
      Params {
        program,
        args,
        cwd: cwd.clone(),
        timeout,
        env,
        limit,
        sink: Some(sink),
        cancel: Some(cancel.clone()),
      },
      || live::record(repo, "mig run", ".", who.as_deref()),
    );
    over.store(true, std::sync::atomic::Ordering::Relaxed);
    let outcome = match result {
      Ok(o) => o,
      Err(e) => {
        writer.finish(None, false, false, 0.0);
        return Err(e);
      }
    };
    let rest = writer.finish(outcome.exit, outcome.timed_out, outcome.cancelled, outcome.secs);
    if !rest.is_empty()
      && let Some(f) = &lines
    {
      f(rest);
    }
    let stored = outlog::lines(repo.path(), &id).unwrap_or_default();
    let ok = outcome.exit == Some(0) && !outcome.timed_out && !outcome.cancelled;

    let mut changed = Vec::new();
    let mut note = String::new();
    if let (Some(a), Some(before)) = (agent, &before) {
      changed = record_changes(repo, &root, &a.id, before);
      if !changed.is_empty() {
        if let Ok(Some(o)) = agent::checkpoint(repo, &a.id, &format!("run: {}", entry.summary)) {
          note = format!("agent {}: checkpoint {}", a.name, repo::short(o));
        }
      }
    } else {
      note = "no agent session, so mig cannot undo what this changed (agent_start first)".into();
    }

    entry.verdict = "ran".into();
    entry.exit = outcome.exit;
    entry.timed_out = outcome.timed_out;
    entry.secs = outcome.secs;
    entry.tail = tail_of(&stored[stored.len().saturating_sub(40)..].join("\n"));
    append_log(repo.path(), &entry);

    let head = if outcome.cancelled {
      format!("stopped on request after {:.1}s", outcome.secs)
    } else if outcome.timed_out {
      format!("timed out after {}s and was stopped", timeout.as_secs())
    } else {
      match outcome.exit {
        Some(c) => format!("exit {c} in {:.1}s", outcome.secs),
        None => format!("ended by a signal after {:.1}s", outcome.secs),
      }
    };
    let mut text = format!("{head}\n");
    let shown = outfmt::render(&stored, &filter, ok, limit);
    if stored.is_empty() {
      text.push_str("(no output)\n");
    } else if shown.text.is_empty() {
      text.push_str(&format!("({} lines not shown; run_output job={id})\n", shown.total));
    } else {
      text.push_str(&shown.text);
      if shown.shown < shown.total {
        text.push_str(&format!("(all {} lines: run_output job={id})\n", shown.total));
      }
    }
    let notes: Vec<String> = report
      .findings
      .iter()
      .filter(|f| f.level != Level::Block)
      .map(|f| format!("- {}", f.show()))
      .collect();
    if !notes.is_empty() {
      text.push_str(&format!("notes:\n{}\n", notes.join("\n")));
    }
    if !dropped.is_empty() && first_time(&NOTE_ENV) {
      text.push_str(&format!(
        "({} environment variable(s) that look like credentials were not passed on)\n",
        dropped.len()
      ));
    }
    if !changed.is_empty() {
      let shown: Vec<&str> = changed.iter().take(10).map(String::as_str).collect();
      text.push_str(&format!(
        "changed: {}{}\n",
        shown.join(", "),
        if changed.len() > 10 {
          format!(" … (+{})", changed.len() - 10)
        } else {
          String::new()
        }
      ));
    }
    if !note.is_empty() && (agent.is_some() || first_time(&NOTE_NO_AGENT)) {
      text.push_str(&note);
      text.push('\n');
    }
    Ok(Done {
      text: text.trim_end().to_string(),
    })
  }
}
