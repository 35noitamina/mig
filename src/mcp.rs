// A Model Context Protocol server over stdio (newline-delimited JSON-RPC 2.0), so AI clients can
// use mig as a set of tools. Only protocol messages go to stdout.
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use git2::Repository;
use serde_json::{Value, json};

use crate::assist::{self, ContextOpts};
use crate::guard::SaveBlocked;
use crate::{agent, ai, files, oplog, ops, repo, rev, scan, workspace};

const MAX_DIFF: usize = 100_000;

fn tool(name: &str, description: &str, mut properties: Value, required: &[&str]) -> Value {
  if name != "repos" {
    properties["repo"] = json!({
      "type": "string",
      "description": "Folder to work in, by name from `repos` (default: the folder the server started in, or the only open one)"
    });
  }
  json!({
    "name": name,
    "description": description,
    "inputSchema": {"type": "object", "properties": properties, "required": required},
  })
}

fn tools() -> Vec<Value> {
  let none = json!({});
  vec![
    tool(
      "repos",
      "List the folders this server may work in (the ones open in the mig window, plus the folder it started in), with branch and running agent sessions.",
      json!({}),
      &[],
    ),
    tool(
      "ls",
      "List a folder inside the repository, honouring .gitignore. Shows sizes and change marks (M modified, A added, ? untracked).",
      json!({
        "path": {"type": "string", "description": "Folder, relative to the repository root (default \".\")"},
        "depth": {"type": "integer", "minimum": 1, "maximum": 6, "description": "Levels to show (default 1)"}
      }),
      &[],
    ),
    tool(
      "read",
      "Read a text file with line numbers. Long files are cut; use from/to to page. Secrets are masked as [REDACTED]. Binary and huge files are not shown.",
      json!({
        "path": {"type": "string"},
        "from": {"type": "integer", "minimum": 1, "description": "First line (default 1)"},
        "to": {"type": "integer", "minimum": 1, "description": "Last line (default: 400 lines from `from`, at most 2000)"}
      }),
      &["path"],
    ),
    tool(
      "read_many",
      "Read several files in one call. Each entry is a path, or {path, from, to}. A failed file does not stop the others.",
      json!({"files": {"type": "array", "items": {"anyOf": [
        {"type": "string"},
        {"type": "object", "properties": {"path": {"type": "string"}, "from": {"type": "integer"}, "to": {"type": "integer"}}, "required": ["path"]}
      ]}}}),
      &["files"],
    ),
    tool(
      "search",
      "Search file contents with a regular expression (like grep -rn), honouring .gitignore. Prints path:line:text, with path-line-text for context lines.",
      json!({
        "pattern": {"type": "string", "description": "Regular expression"},
        "path": {"type": "string", "description": "Folder or file to search (default the whole repository)"},
        "glob": {"type": "string", "description": "Only files matching this glob, e.g. \"*.rs\""},
        "context": {"type": "integer", "minimum": 0, "maximum": 5, "description": "Lines of context around each match"},
        "max": {"type": "integer", "minimum": 1, "maximum": 500, "description": "Stop after this many matches (default 50)"},
        "ignore_case": {"type": "boolean"}
      }),
      &["pattern"],
    ),
    tool(
      "edit",
      "Replace text in a file by exact match. Every `old` must occur exactly once (or set all=true), otherwise nothing is changed. Edits apply in order, all or nothing. Returns a small diff. Refuses text that looks like a secret. Checkpoints when an agent session is active.",
      json!({
        "path": {"type": "string"},
        "edits": {"type": "array", "items": {"type": "object", "properties": {
          "old": {"type": "string", "description": "Text to find, copied without line numbers"},
          "new": {"type": "string", "description": "Replacement (empty deletes)"},
          "all": {"type": "boolean", "description": "Replace every occurrence"}
        }, "required": ["old"]}},
        "dry_run": {"type": "boolean", "description": "Show the diff without writing"}
      }),
      &["path", "edits"],
    ),
    tool(
      "write",
      "Create a file or replace it entirely; missing folders are created. Refuses content that looks like a secret. Prefer edit for small changes.",
      json!({"path": {"type": "string"}, "content": {"type": "string"}}),
      &["path", "content"],
    ),
    tool(
      "move",
      "Move or rename a file or folder inside the repository. Fails if the destination exists.",
      json!({"from": {"type": "string"}, "to": {"type": "string"}}),
      &["from", "to"],
    ),
    tool(
      "delete",
      "Delete a file, or a folder with recursive=true.",
      json!({"path": {"type": "string"}, "recursive": {"type": "boolean"}}),
      &["path"],
    ),
    tool(
      "status",
      "Branch, changed files and the agent session, if any.",
      none.clone(),
      &[],
    ),
    tool(
      "diff",
      "Diff of uncommitted changes, or of one commit when `rev` is given (a `log` number such as \"1\", or a commit id). Secrets are redacted.",
      json!({"rev": {"type": "string"}}),
      &[],
    ),
    tool(
      "log",
      "Recent commits, newest first, numbered for use as `rev`.",
      json!({"count": {"type": "integer", "minimum": 1, "maximum": 200}}),
      &[],
    ),
    tool(
      "context",
      "A Markdown overview of the repository (files, recent commits, changes) with secrets redacted.",
      json!({
        "diff": {"type": "boolean", "description": "Include the uncommitted diff"},
        "files": {"type": "array", "items": {"type": "string"}, "description": "Files to include in full"}
      }),
      &[],
    ),
    tool(
      "save",
      "Commit every change with the given message. Refuses when secrets or very large files are found.",
      json!({"message": {"type": "string"}}),
      &["message"],
    ),
    tool(
      "undo",
      "Undo the last mig operation (commit, fold, sync, ...).",
      none.clone(),
      &[],
    ),
    tool(
      "scan",
      "Look for API keys and other secrets in the working tree.",
      none.clone(),
      &[],
    ),
    tool(
      "agent_start",
      "Start an agent session: snapshots the working tree so all later work can be squashed or thrown away.",
      json!({"name": {"type": "string", "description": "What the session is for"}}),
      &[],
    ),
    tool(
      "checkpoint",
      "Snapshot the working tree inside the agent session without committing. Use after each meaningful step.",
      json!({"message": {"type": "string"}}),
      &[],
    ),
    tool(
      "agent_finish",
      "End the agent session, turning all its work into one commit with this message.",
      json!({"message": {"type": "string"}}),
      &["message"],
    ),
    tool(
      "agent_abort",
      "End the agent session and restore the working tree and branch to how they were at the start. The discarded work stays recoverable.",
      none,
      &[],
    ),
  ]
}

fn text(v: &Value, key: &str) -> Option<String> {
  v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn run_tool(repo: &Repository, name: &str, args: &Value) -> Result<String> {
  match name {
    "status" => {
      let mut out = format!(
        "branch: {}\n",
        repo::branch_name(repo).unwrap_or_else(|| "(detached)".into())
      );
      let changes = repo::changes(repo)?;
      if changes.is_empty() {
        out.push_str("no uncommitted changes\n");
      }
      for c in changes.iter().take(300) {
        out.push_str(&format!("{} {}\n", c.kind, c.path));
      }
      if let Some(s) = agent::status(repo)? {
        out.push_str(&format!(
          "agent session \"{}\": {} checkpoint(s), {} commit(s) since start\n",
          s.session.name, s.checkpoints, s.commits
        ));
      }
      Ok(out)
    }
    "ls" => files::ls(repo, args),
    "read" => files::read(repo, args),
    "read_many" => files::read_many(repo, args),
    "search" => files::search(repo, args),
    "edit" => files::edit(repo, args),
    "write" => files::write(repo, args),
    "move" => files::move_path(repo, args),
    "delete" => files::delete(repo, args),
    "diff" => {
      let d = match text(args, "rev") {
        Some(r) => {
          let c = rev::resolve(repo, &r)?;
          let parent = c.parents().next().map(|p| p.tree()).transpose()?;
          repo.diff_tree_to_tree(parent.as_ref(), Some(&c.tree()?), None)?
        }
        None => {
          let head = repo::head_commit(repo)?.map(|c| c.tree()).transpose()?;
          let mut o = git2::DiffOptions::new();
          o.include_untracked(true)
            .recurse_untracked_dirs(true)
            .show_untracked_content(true)
            .max_size(2 << 20);
          repo.diff_tree_to_workdir_with_index(head.as_ref(), Some(&mut o))?
        }
      };
      let (d, _) = ai::clip(&crate::cli::render_diff(&d, false)?, MAX_DIFF);
      let (d, _) = scan::redact(&d);
      Ok(if d.trim().is_empty() { "no changes".into() } else { d })
    }
    "log" => {
      let n = args.get("count").and_then(Value::as_u64).unwrap_or(20) as usize;
      Ok(
        rev::chain(repo, n)?
          .iter()
          .enumerate()
          .map(|(i, c)| format!("{} {} {}\n", i + 1, repo::short(c.id()), repo::summary(c)))
          .collect(),
      )
    }
    "context" => {
      let opts = ContextOpts {
        diff: args.get("diff").and_then(Value::as_bool).unwrap_or(false),
        files: args
          .get("files")
          .and_then(Value::as_array)
          .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
          .unwrap_or_default(),
        commits: 15,
      };
      Ok(assist::context(repo, &opts)?.0)
    }
    "save" => {
      let msg = text(args, "message").ok_or_else(|| anyhow!("message is required"))?;
      let s = oplog::record(repo, "save", || ops::save(repo, Some(&msg), &[], false)).map_err(|e| match e
        .downcast_ref::<SaveBlocked>()
      {
        Some(b) => anyhow!(
          "{b}: {}",
          b.secrets
            .iter()
            .map(|f| format!("{}:{} {}", f.path, f.line, f.rule))
            .chain(b.large.iter().map(|(p, _)| format!("{p} (too large)")))
            .collect::<Vec<_>>()
            .join(", ")
        ),
        None => e,
      })?;
      Ok(format!("saved {}", repo::short(s.oid)))
    }
    "undo" => Ok(format!("undid: {}", oplog::undo(repo, false)?.cmd)),
    "scan" => {
      let found = scan::scan_worktree(repo)?;
      Ok(if found.is_empty() {
        "no secrets found".into()
      } else {
        found
          .iter()
          .map(|f| format!("{}:{} {} {}\n", f.path, f.line, f.rule, f.masked))
          .collect()
      })
    }
    "agent_start" => {
      let s = agent::start(repo, &text(args, "name").unwrap_or_else(|| "agent".into()))?;
      Ok(format!("agent session \"{}\" started on {}", s.name, s.branch))
    }
    "checkpoint" => {
      let msg = text(args, "message").unwrap_or_else(|| "checkpoint".into());
      Ok(match agent::checkpoint(repo, &msg)? {
        Some(o) => format!("checkpoint {}", repo::short(o)),
        None => "nothing changed since the last checkpoint".into(),
      })
    }
    "agent_finish" => {
      let msg = text(args, "message").ok_or_else(|| anyhow!("message is required"))?;
      let f = agent::finish(repo, Some(&msg), false)?;
      Ok(format!(
        "session finished; HEAD is {}",
        f.head.map(repo::short).unwrap_or_default()
      ))
    }
    "agent_abort" => Ok(format!(
      "session aborted; discarded work kept as {}",
      repo::short(agent::abort(repo)?)
    )),
    _ => Err(anyhow!("unknown tool {name}")),
  }
}

// One per connection: the folder the server started in.
struct Server {
  launch: Option<PathBuf>,
}

impl Server {
  fn repos(&self) -> String {
    let mut out = String::new();
    let mut list = workspace::load();
    if let Some(l) = &self.launch
      && !list.iter().any(|w| workspace::same(&w.path, l))
    {
      list.insert(
        0,
        workspace::Workspace {
          name: l
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
          path: l.clone(),
        },
      );
    }
    if list.is_empty() {
      return "no folders; open some in the mig window or add them with `mig ws add`".into();
    }
    for w in list {
      let detail = match repo::open(&w.path) {
        Ok(r) => {
          let branch = repo::branch_name(&r).unwrap_or_else(|| "(detached)".into());
          let agent = agent::session(&r)
            .map(|s| format!("  agent: {}", s.name))
            .unwrap_or_default();
          format!("{branch}{agent}")
        }
        Err(_) => "(missing)".into(),
      };
      let start = if self.launch.as_ref().is_some_and(|l| workspace::same(l, &w.path)) {
        "  [default]"
      } else {
        ""
      };
      out.push_str(&format!("{}  {}  {detail}{start}\n", w.name, w.path.display()));
    }
    out
  }

  fn call(&self, name: &str, args: &Value) -> Result<String> {
    if name == "repos" {
      return Ok(self.repos());
    }
    let root = workspace::pick(self.launch.as_deref(), args.get("repo").and_then(Value::as_str))?;
    run_tool(&repo::open(&root)?, name, args)
  }

  fn handle(&self, msg: &Value) -> std::result::Result<Value, (i64, String)> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match method {
      "initialize" => Ok(json!({
        "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "mig", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
      })),
      "ping" => Ok(json!({})),
      "tools/list" => Ok(json!({"tools": tools()})),
      "tools/call" => {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let (text, is_error) = match self.call(name, &args) {
          Ok(t) => (t, false),
          Err(e) => (format!("{e:#}"), true),
        };
        Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
      }
      _ => Err((-32601, format!("method not found: {method}"))),
    }
  }
}

const INSTRUCTIONS: &str = "Version control and file tools. Several folders may be open: call `repos` to see them and pass `repo` to choose one (the default is the folder the server started in). Call agent_start before changing files: then every write, edit, move and delete is checkpointed and the whole session can be undone with agent_abort or squashed with agent_finish. Prefer search and read with from/to over reading whole files, and edit over write.";

pub fn serve(dir: &Path) -> Result<()> {
  let server = Server {
    launch: repo::open(dir).ok().and_then(|r| {
      r.workdir()
        .map(|w| w.canonicalize().unwrap_or_else(|_| w.to_path_buf()))
    }),
  };
  let stdin = std::io::stdin();
  let mut stdout = std::io::stdout();
  for line in stdin.lock().lines() {
    let line = line?;
    if line.trim().is_empty() {
      continue;
    }
    let reply = match serde_json::from_str::<Value>(&line) {
      Err(e) => json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}}),
      Ok(msg) => {
        // Notifications (no id) get no reply.
        let Some(id) = msg.get("id").cloned() else { continue };
        match server.handle(&msg) {
          Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
          Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        }
      }
    };
    writeln!(stdout, "{reply}")?;
    stdout.flush()?;
  }
  Ok(())
}
