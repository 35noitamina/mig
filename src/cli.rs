use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use chrono::{Local, TimeZone};
use clap::{Args, Parser, Subcommand};
use git2::{DiffFormat, DiffOptions, Oid, Repository};

use crate::guard::{self, SaveBlocked};
use crate::scan::{Finding, SecretsFound};
use crate::{absorb, gc, oplog, ops, repo, rev, scan, setup, split};

const HELP: &str = "\
{about}

{usage-heading} {usage}

Daily:
  st                 Show changed files
  save               Commit every change (no staging needed)
  sync               Fetch, rebase onto upstream and push
  log                Show history; the numbers work as revisions
  diff               Show uncommitted changes, or the changes of a commit
  undo, redo         Undo or redo the last operation
  sw                 Switch branch (-c to create it)

History (mig history | mig h):
  fold               Fold commits into one
  reword             Change a commit message
  drop               Remove a commit
  split              Split a commit in two
  absorb             Move edits into the commits that last touched those lines
  log, diff          Same as the daily commands

Branches (mig branch | mig br):
  list, new, rm, switch

Remotes (mig remote):
  list, add, rm, publish (create the repository on GitHub and push)

Operations (mig op):
  undo, redo, log

Repository (mig repo):
  init, gc, size, scan

GUI:
  gui [here | PATH]  Open the GUI (welcome screen, or the repository in PATH)

Run `mig <group> --help` or `mig <group> <command> --help` for details.

Options:
{options}";

#[derive(Parser)]
#[command(
  name = "mig",
  version,
  about = "A friendlier, git-compatible version control tool",
  help_template = HELP
)]
struct Cli {
  /// Run as if started in <DIR>
  #[arg(short = 'C', global = true, value_name = "DIR")]
  dir: Option<PathBuf>,
  #[command(subcommand)]
  cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
  /// Show changed files
  #[command(alias = "status")]
  St,
  /// Commit every change (no staging needed)
  Save(SaveArgs),
  /// Fetch, rebase onto upstream and push
  Sync {
    /// Push even if secrets are detected
    #[arg(long)]
    force: bool,
  },
  /// Show history; numbers can be used as revisions
  Log(LogArgs),
  /// Show uncommitted changes, or the changes of a commit
  Diff(DiffArgs),
  /// Undo the last operation
  Undo(ForceArgs),
  /// Redo an undone operation
  Redo(ForceArgs),
  /// Switch branch
  #[command(alias = "switch")]
  Sw(SwitchArgs),

  /// Look at and rewrite history
  #[command(visible_alias = "h", subcommand)]
  History(HistoryCmd),
  /// List, create, delete and switch branches
  #[command(visible_alias = "br", args_conflicts_with_subcommands = true)]
  Branch {
    #[command(subcommand)]
    cmd: Option<BranchCmd>,
    // Older `mig br NAME [AT]` / `mig br -d NAME` forms.
    #[arg(hide = true)]
    name: Option<String>,
    #[arg(hide = true)]
    at: Option<String>,
    #[arg(short, hide = true)]
    d: bool,
  },
  /// Manage remotes and publish to GitHub
  Remote {
    #[command(subcommand)]
    cmd: Option<RemoteCmd>,
  },
  /// Undo, redo and the operation log
  #[command(subcommand)]
  Op(OpCmd),
  /// Create, tidy up and inspect the repository
  #[command(subcommand)]
  Repo(RepoCmd),
  /// Open the GUI on the welcome screen; `mig gui here` (or a path) opens that folder's repository
  Gui { target: Option<PathBuf> },

  // Earlier top-level spellings, kept working but out of the help.
  #[command(hide = true)]
  Init(InitArgs),
  #[command(hide = true)]
  Fold(FoldArgs),
  #[command(hide = true)]
  Reword(RewordArgs),
  #[command(hide = true)]
  Drop(RevArgs),
  #[command(hide = true)]
  Absorb(AbsorbArgs),
  #[command(hide = true)]
  Split(SplitArgs),
  #[command(hide = true)]
  Oplog(CountArgs),
  #[command(hide = true)]
  Scan,
  #[command(hide = true)]
  Gc,
  #[command(hide = true)]
  Size(SizeArgs),
}

#[derive(Subcommand)]
enum HistoryCmd {
  /// Show history; numbers can be used as revisions
  Log(LogArgs),
  /// Show uncommitted changes, or the changes of a commit
  Diff(DiffArgs),
  /// Fold commits into one: `fold 2..5`, `fold 3` (into its parent), `fold` (1 into 2)
  Fold(FoldArgs),
  /// Change a commit message
  Reword(RewordArgs),
  /// Remove a commit from history
  Drop(RevArgs),
  /// Split a commit in two: the given paths (or hunks picked with -i) go first
  Split(SplitArgs),
  /// Move uncommitted edits into the unpushed commits that last touched those lines
  Absorb(AbsorbArgs),
}

#[derive(Subcommand)]
enum BranchCmd {
  /// List branches
  #[command(alias = "ls")]
  List,
  /// Create a branch at HEAD or at REV
  New { name: String, at: Option<String> },
  /// Delete a branch
  #[command(alias = "delete")]
  Rm { name: String },
  /// Switch to a branch
  #[command(alias = "sw")]
  Switch(SwitchArgs),
}

#[derive(Subcommand)]
enum RemoteCmd {
  /// List remotes
  #[command(alias = "ls")]
  List,
  /// Add a remote
  Add { name: String, url: String },
  /// Remove a remote
  #[command(alias = "remove")]
  Rm { name: String },
  /// Create this repository on GitHub, add it as origin and push
  Publish(PublishArgs),
}

#[derive(Subcommand)]
enum OpCmd {
  /// Undo the last operation
  Undo(ForceArgs),
  /// Redo an undone operation
  Redo(ForceArgs),
  /// Show the operation log
  Log(CountArgs),
}

#[derive(Subcommand)]
enum RepoCmd {
  /// Create a repository in a new or existing folder
  Init(InitArgs),
  /// Pack objects with delta compression and remove redundant files
  Gc,
  /// Show repository size and the largest files in history
  Size(SizeArgs),
  /// Scan the working tree for API keys and other secrets
  Scan,
}

#[derive(Args)]
struct SaveArgs {
  #[arg(short, long)]
  message: Option<String>,
  /// Commit even if secrets or large files are detected
  #[arg(long)]
  force: bool,
  /// Limit to these paths
  paths: Vec<String>,
}

#[derive(Args)]
struct LogArgs {
  #[arg(short = 'n', default_value_t = 20)]
  count: usize,
}

#[derive(Args)]
struct CountArgs {
  #[arg(short = 'n', default_value_t = 20)]
  count: usize,
}

#[derive(Args)]
struct SizeArgs {
  #[arg(short = 'n', default_value_t = 10)]
  count: usize,
}

#[derive(Args)]
struct DiffArgs {
  rev: Option<String>,
}

#[derive(Args)]
struct ForceArgs {
  /// Discard unsaved changes
  #[arg(long)]
  force: bool,
}

#[derive(Args)]
struct SwitchArgs {
  name: String,
  /// Create the branch first
  #[arg(short)]
  c: bool,
}

#[derive(Args)]
struct FoldArgs {
  range: Option<String>,
  #[arg(short, long)]
  message: Option<String>,
}

#[derive(Args)]
struct RewordArgs {
  rev: String,
  #[arg(short, long)]
  message: String,
}

#[derive(Args)]
struct RevArgs {
  rev: String,
}

#[derive(Args)]
struct AbsorbArgs {
  /// Only show where each change would go
  #[arg(short = 'n', long)]
  dry_run: bool,
}

#[derive(Args)]
struct SplitArgs {
  rev: String,
  paths: Vec<String>,
  /// Pick hunks interactively
  #[arg(short)]
  i: bool,
  /// Message for the first commit
  #[arg(short, long)]
  message: Option<String>,
}

#[derive(Args)]
struct PublishArgs {
  /// [OWNER/]NAME on GitHub; defaults to the folder name
  name: Option<String>,
  /// Make the repository private
  #[arg(long)]
  private: bool,
  /// Use the SSH URL for origin
  #[arg(long)]
  ssh: bool,
}

#[derive(Args)]
struct InitArgs {
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
}

// Every command, whichever group or spelling it was reached through.
enum Action {
  St,
  Save(SaveArgs),
  Sync(bool),
  Log(usize),
  Diff(Option<String>),
  Undo(bool),
  Redo(bool),
  OpLog(usize),
  Fold(FoldArgs),
  Reword(RewordArgs),
  Drop(String),
  Split(SplitArgs),
  Absorb(bool),
  BranchList,
  BranchNew(String, Option<String>),
  BranchRm(String),
  Switch(SwitchArgs),
  RemoteList,
  RemoteAdd(String, String),
  RemoteRm(String),
  Publish(PublishArgs),
  Scan,
  Gc,
  Size(usize),
}

enum Top {
  Init(InitArgs),
  Gui(Option<PathBuf>),
  Repo(Action),
}

impl Cmd {
  fn flatten(self) -> Result<Top> {
    use Action as A;
    let a = match self {
      Cmd::Init(i) | Cmd::Repo(RepoCmd::Init(i)) => return Ok(Top::Init(i)),
      Cmd::Gui { target } => return Ok(Top::Gui(target)),
      Cmd::St => A::St,
      Cmd::Save(s) => A::Save(s),
      Cmd::Sync { force } => A::Sync(force),
      Cmd::Log(l) | Cmd::History(HistoryCmd::Log(l)) => A::Log(l.count),
      Cmd::Diff(d) | Cmd::History(HistoryCmd::Diff(d)) => A::Diff(d.rev),
      Cmd::Undo(f) | Cmd::Op(OpCmd::Undo(f)) => A::Undo(f.force),
      Cmd::Redo(f) | Cmd::Op(OpCmd::Redo(f)) => A::Redo(f.force),
      Cmd::Oplog(c) | Cmd::Op(OpCmd::Log(c)) => A::OpLog(c.count),
      Cmd::Fold(f) | Cmd::History(HistoryCmd::Fold(f)) => A::Fold(f),
      Cmd::Reword(r) | Cmd::History(HistoryCmd::Reword(r)) => A::Reword(r),
      Cmd::Drop(r) | Cmd::History(HistoryCmd::Drop(r)) => A::Drop(r.rev),
      Cmd::Split(s) | Cmd::History(HistoryCmd::Split(s)) => A::Split(s),
      Cmd::Absorb(a) | Cmd::History(HistoryCmd::Absorb(a)) => A::Absorb(a.dry_run),
      Cmd::Sw(s)
      | Cmd::Branch {
        cmd: Some(BranchCmd::Switch(s)),
        ..
      } => A::Switch(s),
      Cmd::Branch {
        cmd: Some(BranchCmd::List),
        ..
      } => A::BranchList,
      Cmd::Branch {
        cmd: Some(BranchCmd::New { name, at }),
        ..
      } => A::BranchNew(name, at),
      Cmd::Branch {
        cmd: Some(BranchCmd::Rm { name }),
        ..
      } => A::BranchRm(name),
      Cmd::Branch {
        cmd: None,
        name: None,
        d: true,
        ..
      } => return Err(anyhow!("`-d` needs a branch name")),
      Cmd::Branch {
        cmd: None, name: None, ..
      } => A::BranchList,
      Cmd::Branch {
        cmd: None,
        name: Some(n),
        d: true,
        ..
      } => A::BranchRm(n),
      Cmd::Branch {
        cmd: None,
        name: Some(n),
        at,
        ..
      } => A::BranchNew(n, at),
      Cmd::Remote { cmd: None }
      | Cmd::Remote {
        cmd: Some(RemoteCmd::List),
      } => A::RemoteList,
      Cmd::Remote {
        cmd: Some(RemoteCmd::Add { name, url }),
      } => A::RemoteAdd(name, url),
      Cmd::Remote {
        cmd: Some(RemoteCmd::Rm { name }),
      } => A::RemoteRm(name),
      Cmd::Remote {
        cmd: Some(RemoteCmd::Publish(p)),
      } => A::Publish(p),
      Cmd::Scan | Cmd::Repo(RepoCmd::Scan) => A::Scan,
      Cmd::Gc | Cmd::Repo(RepoCmd::Gc) => A::Gc,
      Cmd::Size(s) | Cmd::Repo(RepoCmd::Size(s)) => A::Size(s.count),
    };
    Ok(Top::Repo(a))
  }
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
  let action = match cli.cmd.flatten()? {
    Top::Init(i) => {
      return init(
        dir.join(i.path.unwrap_or_default()),
        i.save,
        i.message,
        i.force,
        i.remote,
        i.github,
        i.private,
        i.ssh,
        i.no_ignore,
      );
    }
    #[cfg(feature = "gui")]
    Top::Gui(target) => {
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
    Top::Gui(_) => return Err(anyhow!("this build has no GUI (rebuild with --features gui)")),
    Top::Repo(a) => a,
  };

  let repo = repo::open(&dir)?;
  let result = dispatch(&repo, action);
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

fn dispatch(repo: &Repository, action: Action) -> Result<()> {
  match action {
    Action::St => status(repo),
    Action::Save(SaveArgs { message, force, paths }) => {
      let saved = oplog::record(repo, "save", || ops::save(repo, message.as_deref(), &paths, force))?;
      for w in &saved.warnings {
        eprintln!("{} {w}", paint("33", "warning:"));
      }
      let c = repo.find_commit(saved.oid)?;
      println!("saved {} {}", paint("33", &repo::short(saved.oid)), repo::summary(&c));
      Ok(())
    }
    Action::Log(count) => log(repo, count),
    Action::Diff(rev) => diff(repo, rev.as_deref()),
    Action::Fold(FoldArgs { range, message }) => {
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
    Action::Reword(RewordArgs { rev, message }) => {
      let pos = rev::position(repo, &rev)?;
      oplog::record(repo, &format!("reword {}", pos + 1), || {
        ops::reword(repo, pos, &message)
      })?;
      println!("reworded #{}", pos + 1);
      Ok(())
    }
    Action::Drop(rev) => {
      let pos = rev::position(repo, &rev)?;
      let summary = repo::summary(&rev::resolve(repo, &rev)?);
      oplog::record(repo, &format!("drop {}", pos + 1), || ops::drop_commit(repo, pos))?;
      println!("dropped \"{summary}\"");
      Ok(())
    }
    Action::Absorb(dry_run) => {
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
    Action::Split(SplitArgs { rev, paths, i, message }) => {
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
    Action::Undo(force) => {
      let op = oplog::undo(repo, force)?;
      println!("undid: {}", op.cmd);
      if op.cmd == "sync" {
        println!("note: only local state was restored; the remote is unchanged");
      }
      Ok(())
    }
    Action::Redo(force) => {
      let op = oplog::redo(repo, force)?;
      println!("redid: {}", op.cmd);
      Ok(())
    }
    Action::OpLog(count) => {
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
    Action::BranchList => {
      for b in ops::branches(repo)? {
        let mark = if b.current { paint("32", "*") } else { " ".into() };
        let name = if b.current { paint("32", &b.name) } else { b.name };
        println!("{mark} {name}  {}", paint("2", &b.summary));
      }
      Ok(())
    }
    Action::BranchRm(name) => {
      oplog::record(repo, &format!("br -d {name}"), || ops::branch_delete(repo, &name))?;
      println!("deleted branch {name}");
      Ok(())
    }
    Action::BranchNew(name, at) => {
      oplog::record(repo, &format!("br {name}"), || {
        ops::branch_create(repo, &name, at.as_deref())
      })?;
      println!("created branch {name}");
      Ok(())
    }
    Action::Switch(SwitchArgs { name, c }) => {
      oplog::record(repo, &format!("sw {name}"), || ops::switch(repo, &name, c))?;
      println!("switched to {name}");
      Ok(())
    }
    Action::Sync(force) => {
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
    Action::Scan => {
      let found = scan::scan_worktree(repo)?;
      if found.is_empty() {
        println!("{}", paint("32", "no secrets found"));
        Ok(())
      } else {
        print_findings(&found);
        Err(anyhow!("secrets found"))
      }
    }
    Action::Gc => {
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
    Action::Size(count) => {
      let r = gc::size(repo, count)?;
      println!("loose objects: {} ({})", r.loose_count, guard::fmt_size(r.loose_bytes));
      println!("packs:         {} ({})", r.pack_count, guard::fmt_size(r.pack_bytes));
      if r.loose_count > 0 {
        println!("{}", paint("2", "run `mig repo gc` to pack loose objects"));
      }
      println!("\nlargest files in history (uncompressed):");
      for (path, size) in r.largest {
        println!("  {:>10}  {path}", guard::fmt_size(size));
      }
      Ok(())
    }
    Action::RemoteList => {
      for (name, url) in setup::remotes(repo)? {
        println!("{name}  {url}");
      }
      Ok(())
    }
    Action::RemoteAdd(name, url) => {
      setup::add_remote(repo, &name, &url)?;
      println!("added {name}");
      Ok(())
    }
    Action::RemoteRm(name) => {
      setup::remove_remote(repo, &name)?;
      println!("removed {name}");
      Ok(())
    }
    Action::Publish(p) => {
      let gh = setup::GithubOptions {
        name: p
          .name
          .unwrap_or_else(|| repo.workdir().map(setup::default_name).unwrap_or_default()),
        private: p.private,
        ssh: p.ssh,
      };
      let created = setup::create_github(repo, &gh)?;
      println!("created {} on GitHub: {}", created.full_name, created.html_url);
      match setup::push_initial(repo, false) {
        Ok(0) => println!("nothing to push yet; run `mig save` then `mig sync`"),
        Ok(n) => println!("pushed {n} commit(s)"),
        Err(e) => eprintln!("{} {e}", paint("33", "note:")),
      }
      Ok(())
    }
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
