use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use chrono::{Local, TimeZone};
use clap::{Args, Parser, Subcommand, ValueEnum};
use git2::{DiffFormat, DiffOptions, Oid, Repository};

use crate::ai::AiConfig;
use crate::assist::ContextOpts;
use crate::guard::{self, SaveBlocked};
use crate::scan::{Finding, SecretsFound};
use crate::{absorb, agent, ai, assist, gc, growth, oplog, ops, repo, rev, scan, setup, split, workspace};

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

AI (mig ai):
  msg                Suggest a commit message for the current changes (or: mig save --ai)
  explain, review    Explain or review a commit or the current changes
  tidy               Suggest how to fold recent commits, and apply it
  config, key        Choose the provider and model, store an API key

Agent sessions (mig agent):
  start, cp, log, restore, finish, abort, status
                     Checkpoint and squash (or throw away) an AI agent's work

For AI tools:
  context            Markdown overview of the repository, secrets redacted
  mcp                Run as an MCP server over stdio
  ws                 Folders the GUI window and the MCP server work in (ls, add, rm, rename)

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
  /// AI helpers: commit messages, explanations, reviews and history tidying
  #[command(subcommand)]
  Ai(AiCmd),
  /// Checkpoint an AI agent's work, then squash it into one commit or throw it away
  #[command(subcommand)]
  Agent(AgentCmd),
  /// Print a Markdown overview of the repository for an AI chat (secrets are redacted)
  Context {
    /// Include the uncommitted diff
    #[arg(long)]
    diff: bool,
    /// Include these files in full
    #[arg(short, long = "file", value_name = "PATH")]
    files: Vec<String>,
    /// Number of recent commits to list
    #[arg(short = 'n', default_value_t = 15)]
    count: usize,
  },
  /// Run an MCP (Model Context Protocol) server on stdin/stdout for AI clients
  Mcp,
  /// Folders open in the GUI window; AI tools (MCP) may work only in these
  #[command(subcommand)]
  Ws(WsCmd),

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
enum WsCmd {
  /// List the open folders
  Ls,
  /// Add a repository (default: the current one)
  Add {
    path: Option<PathBuf>,
    #[arg(long)]
    name: Option<String>,
  },
  /// Remove a folder from the list (its files are not touched)
  Rm { name: String },
  /// Give a folder another name
  Rename { name: String, new: String },
}

#[derive(Subcommand)]
enum AiCmd {
  /// Suggest a commit message for the uncommitted changes
  Msg,
  /// Explain a commit, or the uncommitted changes
  Explain { rev: Option<String> },
  /// Review a commit, or the uncommitted changes, for problems
  Review { rev: Option<String> },
  /// Suggest folding the latest unpushed commits into meaningful ones
  Tidy {
    /// How many recent commits to consider
    #[arg(short = 'n', default_value_t = 20)]
    count: usize,
    /// Apply the plan without asking
    #[arg(short, long)]
    yes: bool,
  },
  /// Show or change the provider, model, base URL and answer language
  Config {
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    /// Override the provider's endpoint (required for `custom`)
    #[arg(long)]
    base_url: Option<String>,
    /// Language for generated text, e.g. Japanese (empty for English)
    #[arg(long)]
    language: Option<String>,
  },
  /// Store an API key for a provider (read from stdin), or remove it with --clear
  Key {
    provider: String,
    #[arg(long)]
    clear: bool,
  },
  /// List the supported providers
  Providers,
}

#[derive(Subcommand)]
enum AgentCmd {
  /// Snapshot the working tree and start a session
  Start {
    #[arg(default_value = "agent")]
    name: String,
  },
  /// Checkpoint the working tree (no commit is made)
  Cp {
    #[arg(short, long, default_value = "checkpoint")]
    message: String,
  },
  /// List the session's checkpoints
  Log,
  /// Bring the working tree back to a checkpoint (number from `log`, or an id)
  Restore { checkpoint: String },
  /// Squash everything since start into one commit and end the session
  Finish {
    #[arg(short, long)]
    message: Option<String>,
    /// Let the AI write the message
    #[arg(long)]
    ai: bool,
  },
  /// Throw away everything since start and end the session
  Abort,
  /// Show the current session
  Status,
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
  /// Let the AI write the message
  #[arg(long)]
  ai: bool,
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
  /// How many of the largest files to list
  #[arg(short = 'n', default_value_t = 10)]
  count: usize,
  /// Show how the size changed over the history instead
  #[arg(long)]
  graph: bool,
  /// Axis of the graph: one point per commit, or per day, week or month
  #[arg(long, value_enum, default_value = "commit")]
  by: By,
  /// What to measure: bytes of tracked files, number of files, or lines of text
  #[arg(long, value_enum, default_value = "size")]
  metric: Measure,
  /// Only the newest this many commits
  #[arg(long, default_value_t = growth::DEFAULT_LAST)]
  last: usize,
  /// Print the numbers as JSON
  #[arg(long)]
  json: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum By {
  Commit,
  Day,
  Week,
  Month,
}

#[derive(Clone, Copy, ValueEnum)]
enum Measure {
  Size,
  Files,
  Lines,
}

struct SizeOpts {
  count: usize,
  graph: Option<(By, Measure, usize, bool)>,
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
  Size(SizeOpts),
  Agent(AgentCmd),
  Context(ContextOpts),
}

enum Top {
  Init(InitArgs),
  Ai(AiCmd),
  Mcp,
  Ws(WsCmd),
  #[cfg_attr(not(feature = "gui"), allow(dead_code))]
  Gui(Option<PathBuf>),
  Repo(Action),
}

impl Cmd {
  fn flatten(self) -> Result<Top> {
    use Action as A;
    let a = match self {
      Cmd::Init(i) | Cmd::Repo(RepoCmd::Init(i)) => return Ok(Top::Init(i)),
      Cmd::Gui { target } => return Ok(Top::Gui(target)),
      Cmd::Ai(a) => return Ok(Top::Ai(a)),
      Cmd::Mcp => return Ok(Top::Mcp),
      Cmd::Ws(w) => return Ok(Top::Ws(w)),
      Cmd::Agent(a) => A::Agent(a),
      Cmd::Context { diff, files, count } => A::Context(ContextOpts {
        diff,
        files,
        commits: count,
      }),
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
      Cmd::Size(s) | Cmd::Repo(RepoCmd::Size(s)) => A::Size(SizeOpts {
        count: s.count,
        graph: (s.graph || s.json).then_some((s.by, s.metric, s.last, s.json)),
      }),
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
    Top::Ai(a) => return run_ai(&dir, a),
    Top::Mcp => return crate::mcp::serve(&dir),
    Top::Ws(w) => return run_ws(&dir, w),
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
    Action::Save(SaveArgs {
      mut message,
      force,
      ai,
      paths,
    }) => {
      if ai && message.is_none() {
        let r = assist::commit_message(repo, &AiConfig::load())?;
        note_redacted(r.redacted);
        message = Some(r.text);
      }
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
    Action::Size(SizeOpts {
      graph: Some((by, metric, last, json)),
      ..
    }) => size_graph(repo, by, metric, last, json),
    Action::Size(SizeOpts { count, .. }) => {
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
    Action::Agent(cmd) => run_agent(repo, cmd),
    Action::Context(opts) => {
      let (text, redacted) = assist::context(repo, &opts)?;
      print!("{text}");
      note_redacted(redacted);
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

fn note_redacted(n: usize) {
  if n > 0 {
    eprintln!(
      "{} {n} possible secret(s) were replaced with [REDACTED]",
      paint("33", "note:")
    );
  }
}

fn run_ai(dir: &std::path::Path, cmd: AiCmd) -> Result<()> {
  let mut cfg = AiConfig::load();
  match cmd {
    AiCmd::Providers => {
      for p in ai::PROVIDERS {
        let (_, src) = ai::find_key(p);
        let key = match src {
          ai::KeySource::Env(v) => format!("key from ${v}"),
          ai::KeySource::Stored => "key stored".into(),
          ai::KeySource::NotNeeded => "no key needed".into(),
          ai::KeySource::Missing => format!("no key (set ${} or `mig ai key {}`)", p.key_env, p.id),
        };
        let mark = if p.id == cfg.provider {
          paint("32", "*")
        } else {
          " ".into()
        };
        println!("{mark} {:<11} {:<26} {}", p.id, p.name, paint("2", &key));
      }
      Ok(())
    }
    AiCmd::Config {
      provider,
      model,
      base_url,
      language,
    } => {
      let changed = provider.is_some() || model.is_some() || base_url.is_some() || language.is_some();
      if let Some(p) = provider {
        ai::provider(&p)?;
        if p != cfg.provider {
          cfg.model.clear();
          cfg.base_url.clear();
        }
        cfg.provider = p;
      }
      if let Some(m) = model {
        cfg.model = m;
      }
      if let Some(u) = base_url {
        cfg.base_url = u;
      }
      if let Some(l) = language {
        cfg.language = l;
      }
      if changed {
        cfg.save()?;
      }
      let p = cfg.provider()?;
      println!("provider  {} ({})", p.id, p.name);
      println!(
        "model     {}",
        cfg.model().unwrap_or_else(|e| paint("33", &e.to_string()))
      );
      println!(
        "base url  {}",
        cfg.base_url().unwrap_or_else(|e| paint("33", &e.to_string()))
      );
      println!(
        "language  {}",
        if cfg.language.is_empty() {
          "English"
        } else {
          &cfg.language
        }
      );
      let (_, src) = ai::find_key(p);
      println!("api key   {src:?}");
      Ok(())
    }
    AiCmd::Key { provider, clear } => {
      ai::provider(&provider)?;
      if clear {
        ai::store_key(&provider, None)?;
        println!("removed the stored key for {provider}");
        return Ok(());
      }
      if std::io::stdin().is_terminal() {
        eprint!("paste the API key for {provider} and press Enter: ");
      }
      let mut key = String::new();
      std::io::stdin().read_line(&mut key)?;
      if key.trim().is_empty() {
        return Err(anyhow!("no key given"));
      }
      ai::store_key(&provider, Some(&key))?;
      println!("stored (in your user config folder, readable only by you)");
      Ok(())
    }
    AiCmd::Msg => {
      let repo = repo::open(dir)?;
      let r = assist::commit_message(&repo, &cfg)?;
      note_redacted(r.redacted);
      println!("{}", r.text);
      Ok(())
    }
    AiCmd::Explain { rev } => {
      let repo = repo::open(dir)?;
      let r = assist::explain(&repo, &cfg, rev.as_deref())?;
      note_redacted(r.redacted);
      println!("{}", r.text);
      Ok(())
    }
    AiCmd::Review { rev } => {
      let repo = repo::open(dir)?;
      let r = assist::review(&repo, &cfg, rev.as_deref())?;
      note_redacted(r.redacted);
      println!("{}", r.text);
      Ok(())
    }
    AiCmd::Tidy { count, yes } => {
      let repo = repo::open(dir)?;
      let groups = assist::tidy_plan(&repo, &cfg, count)?;
      if groups.is_empty() {
        println!("the history already looks tidy");
        return Ok(());
      }
      let chain = rev::chain(&repo, count)?;
      for g in &groups {
        println!(
          "{}",
          paint(
            "1",
            &format!("#{}..#{} -> {}", g.from, g.to, g.message.lines().next().unwrap_or(""))
          )
        );
        for c in &chain[g.from - 1..g.to] {
          println!("    {} {}", paint("33", &repo::short(c.id())), repo::summary(c));
        }
      }
      if !yes {
        if !std::io::stdin().is_terminal() {
          println!("re-run with --yes to apply");
          return Ok(());
        }
        print!("apply this plan? [y/N] ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut a = String::new();
        std::io::stdin().read_line(&mut a)?;
        if a.trim() != "y" {
          println!("left as is");
          return Ok(());
        }
      }
      assist::apply_tidy(&repo, &groups)?;
      println!("folded {} group(s); `mig undo` reverts it", groups.len());
      Ok(())
    }
  }
}

fn run_agent(repo: &Repository, cmd: AgentCmd) -> Result<()> {
  match cmd {
    AgentCmd::Start { name } => {
      let s = agent::start(repo, &name)?;
      println!("agent session \"{}\" started on {}", s.name, s.branch);
      println!("checkpoint with `mig agent cp`, end with `mig agent finish` or `mig agent abort`");
    }
    AgentCmd::Cp { message } => match agent::checkpoint(repo, &message)? {
      Some(o) => println!("checkpoint {}", paint("33", &repo::short(o))),
      None => println!("nothing changed since the last checkpoint"),
    },
    AgentCmd::Log => {
      for (i, c) in agent::checkpoints(repo)?.iter().enumerate() {
        println!(
          "{:>4}  {}  {}  {}",
          i + 1,
          paint("33", &repo::short(c.oid)),
          paint("2", &fmt_time(c.time)),
          c.message
        );
      }
    }
    AgentCmd::Restore { checkpoint } => {
      let o = agent::restore(repo, &checkpoint)?;
      println!("working tree restored to {}", paint("33", &repo::short(o)));
    }
    AgentCmd::Finish { message, ai } => {
      let f = agent::finish(repo, message.as_deref(), ai)?;
      println!(
        "session finished: {} commit(s) since start; HEAD is {}",
        f.commits,
        paint("33", &f.head.map(repo::short).unwrap_or_default())
      );
    }
    AgentCmd::Abort => {
      let saved = agent::abort(repo)?;
      println!("session aborted; everything since start was discarded");
      println!(
        "the discarded work is kept as {}; `mig agent restore {}` brings it back",
        paint("33", &repo::short(saved)),
        repo::short(saved)
      );
    }
    AgentCmd::Status => match agent::status(repo)? {
      None => println!("no agent session"),
      Some(s) => {
        println!(
          "session   {} (on {}, since {})",
          s.session.name,
          s.session.branch,
          fmt_time(s.session.started)
        );
        println!(
          "checkpoints {}, commits since start {}, changed files {}",
          s.checkpoints, s.commits, s.changed_files
        );
      }
    },
  }
  Ok(())
}

const GRAPH_ROWS: usize = 40;
const BAR_WIDTH: usize = 48;

fn fmt_value(m: growth::Metric, v: u64) -> String {
  match m {
    growth::Metric::Bytes => guard::fmt_size(v),
    _ => v.to_string(),
  }
}

fn fmt_delta(m: growth::Metric, d: i64) -> String {
  let s = fmt_value(m, d.unsigned_abs());
  match d {
    0 => String::new(),
    d if d > 0 => paint("32", &format!("+{s}")),
    _ => paint("31", &format!("-{s}")),
  }
}

fn size_graph(repo: &Repository, by: By, measure: Measure, last: usize, json: bool) -> Result<()> {
  use growth::{Metric, Unit};
  let metric = match measure {
    Measure::Size => Metric::Bytes,
    Measure::Files => Metric::Files,
    Measure::Lines => Metric::Lines,
  };
  let unit = match by {
    By::Commit => Unit::Commit,
    By::Day => Unit::Day,
    By::Week => Unit::Week,
    By::Month => Unit::Month,
  };
  let all = growth::history(repo)?;
  let points = growth::tail(&all, last);
  if points.is_empty() {
    println!("no commits yet");
    return Ok(());
  }
  // (label, summary, bytes, files, lines)
  let rows: Vec<(String, String, u64, u64, u64)> = if unit == Unit::Commit {
    let offset = all.len() - points.len();
    points
      .iter()
      .enumerate()
      .map(|(i, p)| {
        (
          format!("#{:<4} {}", offset + i + 1, &p.oid[..7]),
          p.summary.clone(),
          p.bytes,
          p.files,
          p.lines,
        )
      })
      .collect()
  } else {
    growth::buckets(points, unit)
      .into_iter()
      .map(|b| (b.label, format!("{} commit(s)", b.commits), b.bytes, b.files, b.lines))
      .collect()
  };
  let value = |r: &(String, String, u64, u64, u64)| match metric {
    Metric::Bytes => r.2,
    Metric::Files => r.3,
    Metric::Lines => r.4,
  };
  if json {
    let items: Vec<_> = rows
      .iter()
      .map(|r| serde_json::json!({"label": r.0.trim(), "note": r.1, "bytes": r.2, "files": r.3, "lines": r.4}))
      .collect();
    println!(
      "{}",
      serde_json::to_string_pretty(&serde_json::json!({"by": format!("{by:?}").to_lowercase(), "points": items}))?
    );
    return Ok(());
  }
  let shown = growth::thin(&rows, GRAPH_ROWS);
  let top = shown.iter().map(&value).max().unwrap_or(0).max(1);
  let mut prev = rows.iter().take_while(|r| r.0 != shown[0].0).last().map(&value);
  for r in &shown {
    let v = value(r);
    let bar = ((v as u128 * BAR_WIDTH as u128) / top as u128) as usize;
    let d = prev.map_or(0, |p| v as i64 - p as i64);
    println!(
      "{}  {}{}  {:>9} {}",
      paint("2", &r.0),
      paint("36", &"█".repeat(bar.max(usize::from(v > 0)))),
      " ".repeat(BAR_WIDTH - bar.max(usize::from(v > 0))),
      fmt_value(metric, v),
      fmt_delta(metric, d)
    );
    prev = Some(v);
  }
  if shown.len() < rows.len() {
    println!(
      "{}",
      paint("2", &format!("showing {} of {} points", shown.len(), rows.len()))
    );
  }
  Ok(())
}

fn run_ws(dir: &std::path::Path, cmd: WsCmd) -> Result<()> {
  match cmd {
    WsCmd::Ls => {
      let list = workspace::load();
      if list.is_empty() {
        println!("no folders; add one with `mig ws add`");
      }
      for w in list {
        println!("{}  {}", paint("36", &w.name), w.path.display());
      }
    }
    WsCmd::Add { path, name } => {
      let w = workspace::add(&dir.join(path.unwrap_or_default()), name.as_deref())?;
      println!("{}  {}", paint("36", &w.name), w.path.display());
    }
    WsCmd::Rm { name } => {
      let w = workspace::remove(&name)?;
      println!("removed {} ({})", w.name, w.path.display());
    }
    WsCmd::Rename { name, new } => {
      workspace::rename(&name, &new)?;
      println!("renamed {name} to {new}");
    }
  }
  Ok(())
}
