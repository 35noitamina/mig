// Runs a program or a temporary script for an agent: a cleaned-up environment, a time limit with
// the whole process tree stopped when it runs out, and output that is cut to a readable size with
// secrets masked.
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use globset::{Glob, GlobSetBuilder};

use crate::config::RunSection;
use crate::runcheck::Lang;
use crate::scan;

const POLL: Duration = Duration::from_millis(40);
const TICK: Duration = Duration::from_secs(2);

// Name patterns of variables that usually hold a credential; matched without regard to case.
const SECRET_ENV: &[&str] = &[
  "*_KEY",
  "*_KEYS",
  "*_TOKEN",
  "*_TOKENS",
  "*_SECRET",
  "*_SECRETS",
  "*_PASSWORD",
  "*PASSWD*",
  "*CREDENTIAL*",
  "*_PAT",
  "*ACCESS_KEY*",
  "*PRIVATE_KEY*",
  "*_DSN",
  "*API_KEY*",
  "*TOKEN*",
  "*SECRET*",
  "AWS_*",
  "AZURE_*",
  "GOOGLE_APPLICATION_CREDENTIALS",
  "ANTHROPIC_*",
  "OPENAI_*",
  "OPENROUTER_*",
  "GEMINI_*",
  "MISTRAL_*",
  "DEEPSEEK_*",
  "XAI_*",
  "GROQ_*",
  "GITHUB_TOKEN",
  "GH_TOKEN",
  "GITLAB_*",
  "NPM_TOKEN",
  "NODE_AUTH_TOKEN",
  "PYPI_*",
  "TWINE_*",
  "SSH_AUTH_SOCK",
  "SSH_AGENT_PID",
  "GPG_*",
  "GNUPG*",
  "DATABASE_URL",
  "SENTRY_*",
  "SLACK_*",
  "STRIPE_*",
  "TWILIO_*",
  "SENDGRID_*",
  "CLAUDE_CODE_OAUTH_TOKEN",
  "DOCKER_PASSWORD",
  "HF_TOKEN",
  "HUGGING*TOKEN*",
  "KUBECONFIG",
];

fn globset(patterns: &[&str]) -> globset::GlobSet {
  let mut b = GlobSetBuilder::new();
  for p in patterns {
    if let Ok(g) = Glob::new(&p.to_uppercase()) {
      b.add(g);
    }
  }
  b.build().unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap())
}

// The environment a command gets: the current one without credentials, plus a few settings that
// keep programs from waiting for a person.
pub fn clean_env(
  cfg: &RunSection,
  vars: impl Iterator<Item = (String, String)>,
) -> (HashMap<String, String>, Vec<String>) {
  let secret = globset(SECRET_ENV);
  let extra: Vec<&str> = cfg.env_drop.iter().map(String::as_str).collect();
  let extra = globset(&extra);
  let keep: Vec<&str> = cfg.env_keep.iter().map(String::as_str).collect();
  let keep = globset(&keep);
  let mut out = HashMap::new();
  let mut dropped = Vec::new();
  for (k, v) in vars {
    let up = k.to_uppercase();
    if (secret.is_match(&up) || extra.is_match(&up)) && !keep.is_match(&up) {
      dropped.push(k);
    } else {
      out.insert(k, v);
    }
  }
  dropped.sort();
  for (k, v) in [
    ("NO_COLOR", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("MIG_RUN", "1"),
  ] {
    out.insert(k.to_string(), v.to_string());
  }
  (out, dropped)
}

// Looks `name` up on PATH. On Windows `scripts` also accepts .cmd and .bat (npm, for example).
fn on_path_with(name: &str, scripts: bool) -> Option<PathBuf> {
  let exts: Vec<String> = if cfg!(windows) {
    std::env::var("PATHEXT")
      .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
      .split(';')
      .map(str::to_lowercase)
      .filter(|e| e == ".exe" || e == ".com" || (scripts && (e == ".cmd" || e == ".bat")))
      .collect()
  } else {
    vec![String::new()]
  };
  let path = std::env::var_os("PATH")?;
  for dir in std::env::split_paths(&path) {
    for e in &exts {
      let candidate = dir.join(format!("{name}{e}"));
      if candidate.is_file() {
        return Some(candidate);
      }
    }
  }
  None
}

fn on_path(name: &str) -> Option<PathBuf> {
  on_path_with(name, false)
}

// The program an argv starts: a path (relative to `cwd`) or a name on PATH.
pub fn find_program(name: &str, cwd: &Path) -> Result<PathBuf> {
  if name.contains('/') || name.contains('\\') {
    let p = Path::new(name);
    let full = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    if full.is_file() {
      return Ok(full);
    }
    if cfg!(windows) && full.extension().is_none() {
      for e in ["exe", "cmd", "bat", "com"] {
        let with = full.with_extension(e);
        if with.is_file() {
          return Ok(with);
        }
      }
    }
    bail!("{name}: no such program");
  }
  on_path_with(name, true).ok_or_else(|| anyhow!("{name}: command not found on PATH"))
}

pub struct Interpreter {
  pub program: PathBuf,
  // Arguments before the script file.
  pub before: Vec<String>,
  pub ext: &'static str,
}

pub fn interpreter(lang: Lang, requested: &str) -> Result<Interpreter> {
  let want = requested.to_ascii_lowercase();
  let find = |names: &[&str], before: &[&str], ext: &'static str| -> Option<Interpreter> {
    names.iter().find_map(|n| {
      on_path(n).map(|program| Interpreter {
        program,
        before: before.iter().map(|s| s.to_string()).collect(),
        ext,
      })
    })
  };
  let found = match lang {
    Lang::Shell => match want.as_str() {
      "bash" => find(&["bash"], &[], "sh"),
      "zsh" => find(&["zsh"], &[], "sh"),
      _ => find(&["sh", "bash"], &[], "sh"),
    },
    Lang::Python => {
      if cfg!(windows) {
        find(&["python", "python3"], &["-u"], "py").or_else(|| find(&["py"], &["-3", "-u"], "py"))
      } else {
        find(&["python3", "python"], &["-u"], "py")
      }
    }
    Lang::Node => find(&["node"], &[], "js"),
    Lang::Ruby => find(&["ruby"], &[], "rb"),
    Lang::Perl => find(&["perl"], &[], "pl"),
    Lang::PowerShell => find(
      &["pwsh", "powershell"],
      &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"],
      "ps1",
    ),
    Lang::Cmd => {
      if cfg!(windows) {
        find(&["cmd"], &["/d", "/s", "/c"], "cmd")
      } else {
        None
      }
    }
  };
  found.ok_or_else(|| anyhow!("no interpreter for {requested} was found on PATH"))
}

// A script written under .git/mig/run, removed again when this goes out of scope.
pub struct TempScript {
  pub path: PathBuf,
}

impl TempScript {
  pub fn write(git_dir: &Path, ext: &str, code: &str) -> Result<TempScript> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = git_dir.join("mig").join("run");
    std::fs::create_dir_all(&dir)?;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{}-{}-{n}.{ext}", crate::live::now_ms(), std::process::id()));
    std::fs::write(&path, code)?;
    Ok(TempScript { path })
  }
}

impl Drop for TempScript {
  fn drop(&mut self) {
    let _ = std::fs::remove_file(&self.path);
  }
}

// The first bytes and the last bytes of the output; the middle is dropped when it is too long.
struct Capture {
  head: Vec<u8>,
  tail: VecDeque<u8>,
  head_max: usize,
  tail_max: usize,
  total: usize,
}

impl Capture {
  fn new(limit: usize) -> Capture {
    Capture {
      head: Vec::new(),
      tail: VecDeque::new(),
      head_max: limit / 3,
      tail_max: limit - limit / 3,
      total: 0,
    }
  }

  fn push(&mut self, chunk: &[u8]) {
    self.total += chunk.len();
    let room = self.head_max.saturating_sub(self.head.len());
    let (a, b) = chunk.split_at(room.min(chunk.len()));
    self.head.extend_from_slice(a);
    self.tail.extend(b.iter().copied());
    while self.tail.len() > self.tail_max {
      self.tail.pop_front();
    }
  }

  fn text(&self) -> String {
    let tail: Vec<u8> = self.tail.iter().copied().collect();
    let kept = self.head.len() + tail.len();
    let mut out = String::from_utf8_lossy(&self.head).into_owned();
    if self.total > kept {
      out.push_str(&format!("\n… {} bytes left out …\n", self.total - kept));
    }
    out.push_str(&String::from_utf8_lossy(&tail));
    scan::redact(&out).0
  }
}

pub struct Params {
  pub program: PathBuf,
  pub args: Vec<String>,
  pub cwd: PathBuf,
  pub timeout: Duration,
  pub env: HashMap<String, String>,
  pub limit: usize,
}

pub struct Outcome {
  pub exit: Option<i32>,
  pub timed_out: bool,
  pub secs: f64,
  pub output: String,
}

#[cfg(unix)]
fn kill_tree(child: &mut std::process::Child) {
  // The child leads its own process group, so one signal reaches everything it started.
  unsafe {
    libc::kill(-(child.id() as i32), libc::SIGKILL);
  }
  let _ = child.kill();
}

#[cfg(windows)]
fn kill_tree(child: &mut std::process::Child) {
  let _ = Command::new("taskkill")
    .args(["/PID", &child.id().to_string(), "/T", "/F"])
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status();
  let _ = child.kill();
}

fn pump(mut src: impl Read + Send + 'static, cap: Arc<Mutex<Capture>>) -> std::thread::JoinHandle<()> {
  std::thread::spawn(move || {
    let mut buf = [0u8; 8192];
    while let Ok(n) = src.read(&mut buf) {
      if n == 0 {
        break;
      }
      if let Ok(mut c) = cap.lock() {
        c.push(&buf[..n]);
      }
    }
  })
}

// `tick` is called about every two seconds while the program runs.
pub fn execute(p: Params, mut tick: impl FnMut()) -> Result<Outcome> {
  let mut cmd = Command::new(&p.program);
  cmd
    .args(&p.args)
    .current_dir(&p.cwd)
    .env_clear()
    .envs(&p.env)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  #[cfg(unix)]
  {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
  }
  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
  }
  let started = Instant::now();
  let mut child = cmd
    .spawn()
    .map_err(|e| anyhow!("could not start {}: {e}", p.program.display()))?;
  let cap = Arc::new(Mutex::new(Capture::new(p.limit)));
  let readers = [
    child.stdout.take().map(|s| pump(s, cap.clone())),
    child.stderr.take().map(|s| pump(s, cap.clone())),
  ];
  let mut last_tick = Instant::now();
  let (status, timed_out) = loop {
    match child.try_wait()? {
      Some(s) => break (Some(s), false),
      None if started.elapsed() >= p.timeout => {
        kill_tree(&mut child);
        let _ = child.wait();
        break (None, true);
      }
      None => {
        if last_tick.elapsed() >= TICK {
          tick();
          last_tick = Instant::now();
        }
        std::thread::sleep(POLL);
      }
    }
  };
  for r in readers.into_iter().flatten() {
    let _ = r.join();
  }
  let output = cap.lock().map(|c| c.text()).unwrap_or_default();
  Ok(Outcome {
    exit: status.and_then(|s| s.code()),
    timed_out,
    secs: started.elapsed().as_secs_f64(),
    output,
  })
}

pub fn ensure_inside(root: &Path, dir: &Path) -> Result<PathBuf> {
  let (r, d) = (
    root.canonicalize()?,
    dir
      .canonicalize()
      .map_err(|_| anyhow!("{} does not exist", dir.display()))?,
  );
  if !d.starts_with(&r) {
    bail!("{} is outside the repository", dir.display());
  }
  Ok(d)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn params(program: &str, args: &[&str], timeout: Duration) -> Params {
    Params {
      program: on_path(program).unwrap_or_else(|| PathBuf::from(program)),
      args: args.iter().map(|s| s.to_string()).collect(),
      cwd: std::env::temp_dir(),
      timeout,
      env: std::env::vars().collect(),
      limit: 60_000,
    }
  }

  #[test]
  fn secret_variables_are_dropped() {
    let cfg = RunSection {
      env_drop: vec!["MY_*".into()],
      env_keep: vec!["GITHUB_TOKEN".into()],
      ..Default::default()
    };
    let vars = [
      ("PATH", "/bin"),
      ("HOME", "/h"),
      ("OPENAI_API_KEY", "x"),
      ("aws_secret_access_key", "x"),
      ("GITHUB_TOKEN", "x"),
      ("MY_THING", "x"),
      ("DATABASE_URL", "x"),
      ("EDITOR", "vi"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()));
    let (env, dropped) = clean_env(&cfg, vars.into_iter());
    assert!(env.contains_key("PATH") && env.contains_key("HOME") && env.contains_key("EDITOR"));
    assert!(env.contains_key("GITHUB_TOKEN"));
    assert_eq!(
      dropped,
      vec!["DATABASE_URL", "MY_THING", "OPENAI_API_KEY", "aws_secret_access_key"]
    );
    assert_eq!(env.get("MIG_RUN").map(String::as_str), Some("1"));
  }

  #[cfg(unix)]
  #[test]
  fn runs_and_captures_both_streams() {
    let o = execute(
      params(
        "sh",
        &["-c", "echo out; echo err 1>&2; exit 3"],
        Duration::from_secs(10),
      ),
      || {},
    )
    .unwrap();
    assert_eq!(o.exit, Some(3));
    assert!(!o.timed_out);
    assert!(o.output.contains("out") && o.output.contains("err"));
  }

  #[cfg(unix)]
  #[test]
  fn timeout_stops_the_whole_tree() {
    let t = Instant::now();
    let o = execute(
      params(
        "sh",
        &["-c", "sleep 30 & sleep 30; echo never"],
        Duration::from_millis(400),
      ),
      || {},
    )
    .unwrap();
    assert!(o.timed_out);
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(!o.output.contains("never"));
  }

  #[cfg(unix)]
  #[test]
  fn long_output_keeps_the_start_and_the_end() {
    let o = execute(
      params(
        "sh",
        &["-c", "i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done"],
        Duration::from_secs(20),
      ),
      || {},
    )
    .unwrap();
    assert!(o.output.contains("line-0\n"));
    assert!(o.output.contains("line-19999"));
    assert!(o.output.contains("bytes left out"));
    assert!(o.output.len() < 62_000);
  }

  #[cfg(unix)]
  #[test]
  fn secrets_in_output_are_masked() {
    let o = execute(
      params("sh", &["-c", "echo AKIAABCDEFGHIJKLMNOP"], Duration::from_secs(10)),
      || {},
    )
    .unwrap();
    assert!(o.output.contains("[REDACTED]") && !o.output.contains("AKIAABCDEFGHIJKLMNOP"));
  }

  #[test]
  fn temp_scripts_are_removed() {
    let dir = std::env::temp_dir().join(format!("mig-ts-{}", crate::live::now_ms()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = {
      let s = TempScript::write(&dir, "sh", "echo hi").unwrap();
      assert!(s.path.exists());
      s.path.clone()
    };
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(dir);
  }
}
