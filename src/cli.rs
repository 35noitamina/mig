use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use chrono::{Local, TimeZone};
use clap::{Parser, Subcommand};
use git2::{DiffFormat, DiffOptions, Oid, Repository};

use crate::guard::{self, SaveBlocked};
use crate::scan::{Finding, SecretsFound};
use crate::{absorb, gc, oplog, ops, repo, rev, scan, setup, split};

#[derive(Parser)]
#[command(name = "mig", version, about = "A friendlier, git-compatible version control tool")]
struct Cli {
  /// Run as if started in <DIR>
  #[arg(short = 'C', global = true, value_name = "DIR")]
  dir: Option<PathBuf>,
  #[command(subcommand)]
  cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
  /// Create a repository in a new or existing folder
  Init {
    path: Option<PathBuf>,
    /// Make the first commit too
    #[arg(long)]
    save: bool,
    /// Message for the first commit
    #[arg(short, long)]
    message: Option<String>,
    /// Commit even if secrets or large files are found
    #[arg(long)]
    force: bool,
    /// Add this URL as origin
    #[arg(long, value_name = "URL")]
    remote: Option<String>,
    /// Also create the repository on GitHub ([OWNER/]NAME, defaults to the folder name) and push
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    github: Option<String>,
    /// Make the GitHub repository private
    #[arg(long)]
    private: bool,
    /// Use the SSH URL for the GitHub remote
    #[arg(long)]
    ssh: bool,
    /// Don't create a .gitignore
    #[arg(long)]
    no_ignore: bool,
  },
  /// List, add or remove remotes
  Remote {
    #[command(subcommand)]
    cmd: Option<RemoteCmd>,
  },
  /// Show changed files
  #[command(alias = "status")]
  St,
  /// Commit every change (no staging needed)
  Save {
    #[arg(short, long)]
    message: Option<String>,
    /// Commit even if secrets or large files are detected
    #[arg(long)]
    force: bool,
    /// Limit to these paths
    paths: Vec<String>,
  },
  /// Show history; numbers can be used as revisions
  Log {
    #[arg(short = 'n', default_value_t = 20)]
    count: usize,
  },
  /// Show uncommitted changes, or the changes of a commit
  Diff { rev: Option<String> },
  /// Fold commits into one: `fold 2..5`, `fold 3` (into its parent), `fold` (1 into 2)
  Fold {
    range: Option<String>,
    #[arg(short, long)]
    message: Option<String>,
  },
  /// Change a commit message
  Reword {
    rev: String,
    #[arg(short, long)]
    message: String,
  },
  /// Remove a commit from history
  Drop { rev: String },
  /// Move uncommitted edits into the unpushed commits that last touched those lines
  Absorb {
    /// Only show where each change would go
    #[arg(short = 'n', long)]
    dry_run: bool,
  },
  /// Split a commit in two: the given paths (or hunks picked with -i) go first
  Split {
    rev: String,
    paths: Vec<String>,
    /// Pick hunks interactively
    #[arg(short)]
    i: bool,
    /// Message for the first commit
    #[arg(short, long)]
    message: Option<String>,
  },
  /// Undo the last operation
  Undo {
    /// Discard unsaved changes
    #[arg(long)]
    force: bool,
  },
  /// Redo an undone operation
  Redo {
    #[arg(long)]
    force: bool,
  },
  /// Show the operation log
  Oplog {
    #[arg(short = 'n', default_value_t = 20)]
    count: usize,
  },
  /// List, create (`br NAME [AT]`) or delete (`br -d NAME`) branches
  #[command(alias = "branch")]
  Br {
    name: Option<String>,
    at: Option<String>,
    #[arg(short)]
    d: bool,
  },
  /// Switch branch
  #[command(alias = "switch")]
  Sw {
    name: String,
    /// Create the branch first
    #[arg(short)]
    c: bool,
  },
  /// Fetch, rebase onto upstream and push
  Sync {
    /// Push even if secrets are detected
    #[arg(long)]
    force: bool,
  },
  /// Scan the working tree for API keys and other secrets
  Scan,
  /// Pack objects with delta compression and remove redundant files
  Gc,
  /// Show repository size and the largest files in history
  Size {
    #[arg(short = 'n', default_value_t = 10)]
    count: usize,
  },
  /// Open the GUI on the welcome screen; `mig gui here` (or a path) opens that folder's repository
  Gui { target: Option<PathBuf> },
}

#[derive(Subcommand)]
enum RemoteCmd {
  /// Add a remote
  Add { name: String, url: String },
  /// Remove a remote
  #[command(alias = "remove")]
  Rm { name: String },
}

fn color() -> bool {
  std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn paint(code: &str, s: &str) -> String {
  if color() {
    format!("\x1b[{code}m{s}\x1b[0m")
  } else {
    s.to_string()
  }
}

pub fn fmt_time(t: i64) -> String {
  Local
    .timestamp_opt(t, 0)
    .single()
    .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
    .unwrap_or_default()
}

pub fn print_findings(found: &[Finding]) {
  eprintln!(
    "{}",
    paint("1;31", &format!("⚠ {} possible secret(s) found:", found.len()))
  );
  for f in found {
    eprintln!("  {}:{}  {}  {}", f.path, f.line, paint("33", f.rule), f.masked);
  }
  eprintln!(
    "add `mig:allow` to the line or list it in {} if this is a false positive",
    scan::CONFIG_FILE
  );
}

pub fn run() -> Result<()> {
  let cli = Cli::parse();
  let dir = match cli.dir {
    Some(d) => d,
    None => std::env::current_dir()?,
  };
  match cli.cmd {
    Cmd::Init {
      path,
      save,
      message,
      force,
      remote,
      github,
      private,
      ssh,
      no_ignore,
    } => {
      return init(
        dir.join(path.unwrap_or_default()),
        save,
        message,
        force,
        remote,
        github,
        private,
        ssh,
        no_ignore,
      );
    }
    #[cfg(feature = "gui")]
    Cmd::Gui { target } => {
      let open = target.map(|t| {
        if t.as_os_str() == "here" {
          dir.clone()
        } else {
          dir.join(t)
        }
      });
      return crate::gui::run(dir, open);
    }
    #[cfg(not(feature = "gui"))]
    Cmd::Gui { .. } => return Err(anyhow!("this build has no GUI (rebuild with --features gui)")),
    _ => {}
  }

  let repo = repo::open(&dir)?;
  let result = dispatch(&repo, cli.cmd);
  if let Err(e) = &result {
    if let Some(SecretsFound(found)) = e.downcast_ref::<SecretsFound>() {
      print_findings(found);
      return Err(anyhow!("aborted; re-run with --force to ignore"));
    }
    if let Some(b) = e.downcast_ref::<SaveBlocked>() {
      if !b.secrets.is_empty() {
        print_findings(&b.secrets);
      }
      for (path, size) in &b.large {
        eprintln!(
          "{}",
          paint(
            "1;31",
            &format!(
              "⚠ {path} is {} (limit set by [save] max_file_mb)",
              guard::fmt_size(*size)
            )
          )
        );
      }
      return Err(anyhow!("aborted; re-run with --force to ignore"));
    }
  }
  result
}

fn dispatch(repo: &Repository, cmd: Cmd) -> Result<()> {
  match cmd {
    Cmd::St => status(repo),
    Cmd::Save { message, force, paths } => {
      let saved = oplog::record(repo, "save", || ops::save(repo, message.as_deref(), &paths, force))?;
      for w in &saved.warnings {
        eprintln!("{} {w}", paint("33", "warning:"));
      }
      let c = repo.find_commit(saved.oid)?;
      println!("saved {} {}", paint("33", &repo::short(saved.oid)), repo::summary(&c));
      Ok(())
    }
    Cmd::Log { count } => log(repo, count),
    Cmd::Diff { rev } => diff(repo, rev.as_deref()),
    Cmd::Fold { range, message } => {
      let (newer, older) = match range.as_deref() {
        None => (0, 1),
        Some(r) if r.contains("..") => rev::range(repo, r)?,
        Some(r) => {
          let p = rev::position(repo, r)?;
          (p, p + 1)
        }
      };
      let tip = oplog::record(repo, &format!("fold {}..{}", newer + 1, older + 1), || {
        ops::fold(repo, newer, older, message.as_deref())
      })?;
      println!(
        "folded {} commits; HEAD is now {}",
        older - newer + 1,
        paint("33", &repo::short(tip))
      );
      Ok(())
    }
    Cmd::Reword { rev, message } => {
      let pos = rev::position(repo, &rev)?;
      oplog::record(repo, &format!("reword {}", pos + 1), || {
        ops::reword(repo, pos, &message)
      })?;
      println!("reworded #{}", pos + 1);
      Ok(())
    }
    Cmd::Drop { rev } => {
      let pos = rev::position(repo, &rev)?;
      let summary = repo::summary(&rev::resolve(repo, &rev)?);
      oplog::record(repo, &format!("drop {}", pos + 1), || ops::drop_commit(repo, pos))?;
      println!("dropped \"{summary}\"");
      Ok(())
    }
    Cmd::Absorb { dry_run } => {
      let plan = if dry_run {
        absorb::absorb(repo, true)?
      } else {
        oplog::record(repo, "absorb", || absorb::absorb(repo, false))?
      };
      for m in &plan.moves {
        println!(
          "{}  {}  -> #{} {}",
          m.path,
          paint("36", &m.header),
          m.pos + 1,
          m.summary
        );
      }
      let verb = if dry_run { "would absorb" } else { "absorbed" };
      println!(
        "{verb} {} hunk(s); {} left in the working tree",
        plan.moves.len(),
        plan.left
      );
      Ok(())
    }
    Cmd::Split { rev, paths, i, message } => {
      let pos = rev::position(repo, &rev)?;
      let tip = oplog::record(repo, &format!("split {}", pos + 1), || {
        if i {
          split_interactive(repo, pos, message.as_deref())
        } else if paths.is_empty() {
          Err(anyhow!("give paths for the first commit, or use -i to pick hunks"))
        } else {
          split::split_paths(repo, pos, &paths, message.as_deref())
        }
      })?;
      println!("split #{}; HEAD is now {}", pos + 1, paint("33", &repo::short(tip)));
      Ok(())
    }
    Cmd::Undo { force } => {
      let op = oplog::undo(repo, force)?;
      println!("undid: {}", op.cmd);
      if op.cmd == "sync" {
        println!("note: only local state was restored; the remote is unchanged");
      }
      Ok(())
    }
    Cmd::Redo { force } => {
      let op = oplog::redo(repo, force)?;
      println!("redid: {}", op.cmd);
      Ok(())
    }
    Cmd::Oplog { count } => {
      let log = oplog::load(repo)?;
      for (i, op) in log.ops.iter().enumerate().rev().take(count) {
        let undone = if i >= log.pos {
          paint("2", " (undone)")
        } else {
          String::new()
        };
        println!("{:>4}  {}  {}{}", i + 1, paint("2", &fmt_time(op.time)), op.cmd, undone);
      }
      Ok(())
    }
    Cmd::Br { name: None, .. } => {
      for b in ops::branches(repo)? {
        let mark = if b.current { paint("32", "*") } else { " ".into() };
        let name = if b.current { paint("32", &b.name) } else { b.name };
        println!("{mark} {name}  {}", paint("2", &b.summary));
      }
      Ok(())
    }
    Cmd::Br {
      name: Some(name),
      d: true,
      ..
    } => {
      oplog::record(repo, &format!("br -d {name}"), || ops::branch_delete(repo, &name))?;
      println!("deleted branch {name}");
      Ok(())
    }
    Cmd::Br {
      name: Some(name), at, ..
    } => {
      oplog::record(repo, &format!("br {name}"), || {
        ops::branch_create(repo, &name, at.as_deref())
      })?;
      println!("created branch {name}");
      Ok(())
    }
    Cmd::Sw { name, c } => {
      oplog::record(repo, &format!("sw {name}"), || ops::switch(repo, &name, c))?;
      println!("switched to {name}");
      Ok(())
    }
    Cmd::Sync { force } => {
      let r = oplog::record(repo, "sync", || ops::sync(repo, force))?;
      if r.fast_forwarded {
        println!("fast-forwarded to {}/{}", r.remote, r.branch);
      }
      if r.rebased > 0 {
        println!("rebased {} commit(s) onto {}/{}", r.rebased, r.remote, r.branch);
      }
      if r.pushed > 0 {
        println!("pushed {} commit(s) to {}/{}", r.pushed, r.remote, r.branch);
      } else {
        println!("up to date");
      }
      Ok(())
    }
    Cmd::Scan => {
      let found = scan::scan_worktree(repo)?;
      if found.is_empty() {
        println!("{}", paint("32", "no secrets found"));
        Ok(())
      } else {
        print_findings(&found);
        Err(anyhow!("secrets found"))
      }
    }
    Cmd::Gc => {
      let r = gc::gc(repo)?;
      println!(
        "packed {} objects, removed {} loose object(s) and {} old pack(s): {} -> {}",
        r.objects,
        r.removed_loose,
        r.removed_packs,
        guard::fmt_size(r.before),
        guard::fmt_size(r.after)
      );
      Ok(())
    }
    Cmd::Size { count } => {
      let r = gc::size(repo, count)?;
      println!("loose objects: {} ({})", r.loose_count, guard::fmt_size(r.loose_bytes));
      println!("packs:         {} ({})", r.pack_count, guard::fmt_size(r.pack_bytes));
      if r.loose_count > 0 {
        println!("{}", paint("2", "run `mig gc` to pack loose objects"));
      }
      println!("\nlargest files in history (uncompressed):");
      for (path, size) in r.largest {
        println!("  {:>10}  {path}", guard::fmt_size(size));
      }
      Ok(())
    }
    Cmd::Remote { cmd: None } => {
      for (name, url) in setup::remotes(repo)? {
        println!("{name}  {url}");
      }
      Ok(())
    }
    Cmd::Remote {
      cmd: Some(RemoteCmd::Add { name, url }),
    } => {
      setup::add_remote(repo, &name, &url)?;
      println!("added {name}");
      Ok(())
    }
    Cmd::Remote {
      cmd: Some(RemoteCmd::Rm { name }),
    } => {
      setup::remove_remote(repo, &name)?;
      println!("removed {name}");
      Ok(())
    }
    Cmd::Init { .. } | Cmd::Gui { .. } => unreachable!(),
  }
}

fn status(repo: &Repository) -> Result<()> {
  let branch = repo::branch_name(repo).unwrap_or_else(|| "(detached)".into());
  println!("on {}", paint("32", &branch));
  let changes = repo::changes(repo)?;
  if changes.is_empty() {
    println!("nothing changed");
  }
  for c in changes {
    let code = match c.kind {
      'A' | '?' => "32",
      'D' => "31",
      'C' => "1;31",
      _ => "33",
    };
    println!("  {} {}", paint(code, &c.kind.to_string()), c.path);
  }
  let pending = scan::scan_pending(repo)?;
  if !pending.is_empty() {
    print_findings(&pending);
  }
  Ok(())
}

fn decorations(repo: &Repository) -> Result<HashMap<Oid, Vec<String>>> {
  let mut map: HashMap<Oid, Vec<String>> = HashMap::new();
  for r in repo.references()? {
    let r = r?;
    if r.name().is_ok_and(|n| n == "HEAD" || n.ends_with("/HEAD")) {
      continue;
    }
    if let (Ok(name), Ok(c)) = (r.shorthand(), r.peel_to_commit()) {
      map.entry(c.id()).or_default().push(name.to_string());
    }
  }
  Ok(map)
}

fn log(repo: &Repository, count: usize) -> Result<()> {
  let deco = decorations(repo)?;
  for (i, c) in rev::chain(repo, count)?.iter().enumerate() {
    let refs = deco
      .get(&c.id())
      .map(|v| paint("36", &format!(" ({})", v.join(", "))))
      .unwrap_or_default();
    println!(
      "{:>4}  {}  {}  {}{}",
      i + 1,
      paint("33", &repo::short(c.id())),
      paint("2", &fmt_time(c.time().seconds())),
      repo::summary(c),
      refs
    );
  }
  Ok(())
}

fn diff(repo: &Repository, spec: Option<&str>) -> Result<()> {
  let d = match spec {
    Some(s) => {
      let c = rev::resolve(repo, s)?;
      let parent = c.parents().next().map(|p| p.tree()).transpose()?;
      repo.diff_tree_to_tree(parent.as_ref(), Some(&c.tree()?), None)?
    }
    None => {
      let head = repo::head_commit(repo)?.map(|c| c.tree()).transpose()?;
      let mut opts = DiffOptions::new();
      opts
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);
      repo.diff_tree_to_workdir_with_index(head.as_ref(), Some(&mut opts))?
    }
  };
  print!("{}", render_diff(&d, color())?);
  Ok(())
}

pub fn render_diff(d: &git2::Diff<'_>, ansi: bool) -> Result<String> {
  let mut out = String::new();
  d.print(DiffFormat::Patch, |_, _, line| {
    let text = String::from_utf8_lossy(line.content());
    let (prefix, code) = match line.origin() {
      '+' => ("+", "32"),
      '-' => ("-", "31"),
      ' ' => (" ", ""),
      'F' => ("", "1"),
      'H' => ("", "36"),
      _ => ("", ""),
    };
    let s = format!("{prefix}{text}");
    if ansi && !code.is_empty() {
      out.push_str(&format!("\x1b[{code}m{}\x1b[0m\n", s.trim_end_matches('\n')));
    } else {
      out.push_str(&s);
    }
    true
  })?;
  Ok(out)
}

fn split_interactive(repo: &Repository, pos: usize, message: Option<&str>) -> Result<Oid> {
  let stdin = std::io::stdin();
  let mut quit = false;
  split::split(repo, pos, message, &mut |fc, hunk| {
    if quit {
      return false;
    }
    let text = match (hunk, &fc.body) {
      (Some(h), _) => h.text.clone(),
      (None, crate::patch::Body::Whole { text }) => text.clone(),
      _ => String::new(),
    };
    println!("{}", paint("1", &fc.path));
    for l in text.lines() {
      let code = if l.starts_with('+') {
        "32"
      } else if l.starts_with('-') {
        "31"
      } else if l.starts_with("@@") {
        "36"
      } else {
        ""
      };
      println!("{}", if code.is_empty() { l.to_string() } else { paint(code, l) });
    }
    loop {
      print!("put this in the first commit? [y/n/q] ");
      let _ = std::io::Write::flush(&mut std::io::stdout());
      let mut line = String::new();
      if stdin.read_line(&mut line).unwrap_or(0) == 0 {
        quit = true;
        return false;
      }
      match line.trim() {
        "y" => return true,
        "n" => return false,
        "q" => {
          quit = true;
          return false;
        }
        _ => {}
      }
    }
  })
}

#[allow(clippy::too_many_arguments)]
fn init(
  dir: PathBuf,
  save: bool,
  message: Option<String>,
  force: bool,
  remote: Option<String>,
  github: Option<String>,
  private: bool,
  ssh: bool,
  no_ignore: bool,
) -> Result<()> {
  let preview = setup::preview(&dir)?;
  if let Some(k) = preview.kind {
    println!("detected a {} project", k.name());
  }
  if preview.files > 0 {
    println!("{} file(s), {}", preview.files, guard::fmt_size(preview.bytes));
  }
  let github = github.map(|n| setup::GithubOptions {
    name: if n.is_empty() { setup::default_name(&dir) } else { n },
    private,
    ssh,
  });
  let out = setup::create(setup::CreateOptions {
    dir,
    gitignore: if no_ignore { None } else { preview.gitignore },
    save,
    message,
    force,
    remote,
    github,
  })?;
  if out.created {
    println!("initialized {}", out.dir.display());
  } else {
    println!("{} is already a repository", out.dir.display());
  }
  if out.wrote_gitignore {
    println!("wrote .gitignore");
  }
  if let Some(g) = &out.github {
    println!("created {} on GitHub: {}", g.full_name, g.html_url);
  }
  for w in &out.warnings {
    eprintln!("{} {w}", paint("33", "warning:"));
  }
  if let Some(oid) = out.saved {
    println!("saved {} Initial commit", paint("33", &repo::short(oid)));
  }
  if let Some(b) = &out.blocked {
    if !b.secrets.is_empty() {
      print_findings(&b.secrets);
    }
    for (path, size) in &b.large {
      eprintln!("{}", paint("1;31", &format!("⚠ {path} is {}", guard::fmt_size(*size))));
    }
    eprintln!("first commit skipped; fix these or run `mig save --force`");
  } else if !save && preview.files > 0 {
    if !preview.secrets.is_empty() {
      print_findings(&preview.secrets);
    }
    for (path, size) in &preview.large {
      eprintln!(
        "{}",
        paint("33", &format!("note: {path} is {}", guard::fmt_size(*size)))
      );
    }
  }
  if out.pushed > 0 {
    println!("pushed {} commit(s)", out.pushed);
  }
  for n in &out.notes {
    eprintln!("{} {n}", paint("33", "note:"));
  }
  Ok(())
}
