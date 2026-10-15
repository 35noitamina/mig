use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::json;

// MIG_GITHUB_API lets tests point at a local server.
fn api_base() -> String {
  std::env::var("MIG_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into())
}

// GITHUB_TOKEN / GH_TOKEN, or the token the `gh` CLI is logged in with.
pub fn token() -> Option<String> {
  for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
    if let Ok(t) = std::env::var(var)
      && !t.trim().is_empty()
    {
      return Some(t.trim().to_string());
    }
  }
  let out = std::process::Command::new("gh").args(["auth", "token"]).output().ok()?;
  let t = String::from_utf8(out.stdout).ok()?.trim().to_string();
  (out.status.success() && !t.is_empty()).then_some(t)
}

#[derive(Deserialize, Debug)]
pub struct Created {
  pub full_name: String,
  pub html_url: String,
  pub clone_url: String,
  pub ssh_url: String,
}

#[derive(Deserialize)]
struct ApiError {
  message: String,
  #[serde(default)]
  errors: Vec<serde_json::Value>,
}

// `name` may be "repo" (your account) or "org/repo".
pub fn create_repo(name: &str, private: bool, description: &str) -> Result<Created> {
  let token = token().ok_or_else(|| {
    anyhow!("no GitHub token: set GITHUB_TOKEN (a token with the repo scope) or log in with `gh auth login`")
  })?;
  let (url, repo_name) = match name.split_once('/') {
    Some((org, n)) => (format!("{}/orgs/{org}/repos", api_base()), n),
    None => (format!("{}/user/repos", api_base()), name),
  };
  if repo_name.is_empty() {
    bail!("repository name is empty");
  }
  let mut resp = ureq::post(&url)
    .config()
    .http_status_as_error(false)
    .build()
    .header("Authorization", &format!("Bearer {token}"))
    .header("Accept", "application/vnd.github+json")
    .header("X-GitHub-Api-Version", "2022-11-28")
    .header("User-Agent", "mig")
    .send_json(json!({ "name": repo_name, "private": private, "description": description }))
    .context("could not reach GitHub")?;
  let status = resp.status().as_u16();
  if status == 201 {
    return Ok(resp.body_mut().read_json()?);
  }
  let detail = resp
    .body_mut()
    .read_json::<ApiError>()
    .map(|e| {
      let extra: Vec<String> = e
        .errors
        .iter()
        .filter_map(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
        .collect();
      if extra.is_empty() {
        e.message
      } else {
        format!("{} ({})", e.message, extra.join(", "))
      }
    })
    .unwrap_or_default();
  match status {
    401 => bail!("GitHub rejected the token: {detail}"),
    403 => bail!("the token is not allowed to create repositories: {detail}"),
    422 => bail!("GitHub could not create {name}: {detail}"),
    _ => bail!("GitHub returned {status}: {detail}"),
  }
}
