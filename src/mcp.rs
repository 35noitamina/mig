// A Model Context Protocol server over stdio (newline-delimited JSON-RPC 2.0), so AI clients can
// use mig as a set of tools. Only protocol messages go to stdout.
use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Result, anyhow};
use git2::Repository;
use serde_json::{Value, json};

use crate::assist::{self, ContextOpts};
use crate::guard::SaveBlocked;
use crate::{agent, ai, oplog, ops, repo, rev, scan};

const MAX_DIFF: usize = 100_000;

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
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

fn handle(dir: &Path, msg: &Value) -> std::result::Result<Value, (i64, String)> {
  let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
  let params = msg.get("params").cloned().unwrap_or(Value::Null);
  match method {
    "initialize" => Ok(json!({
      "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
      "capabilities": {"tools": {}},
      "serverInfo": {"name": "mig", "version": env!("CARGO_PKG_VERSION")},
      "instructions": "Version control for this folder. Prefer agent_start before changing files, checkpoint after each step, and agent_finish (or agent_abort) at the end.",
    })),
    "ping" => Ok(json!({})),
    "tools/list" => Ok(json!({"tools": tools()})),
    "tools/call" => {
      let name = params.get("name").and_then(Value::as_str).unwrap_or("");
      let args = params.get("arguments").cloned().unwrap_or(json!({}));
      let r = repo::open(dir).and_then(|repo| run_tool(&repo, name, &args));
      let (text, is_error) = match r {
        Ok(t) => (t, false),
        Err(e) => (format!("{e:#}"), true),
      };
      Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
    }
    _ => Err((-32601, format!("method not found: {method}"))),
  }
}

pub fn serve(dir: &Path) -> Result<()> {
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
        match handle(dir, &msg) {
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
