use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::scan;

const TIMEOUT: Duration = Duration::from_secs(180);
const ANTHROPIC_DEFAULT_MODEL: &str = "claude-opus-5-5";
// Diffs beyond this are cut, and the model is told so.
pub const MAX_INPUT_CHARS: usize = 120_000;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Wire {
  Anthropic,
  OpenAi,
}

pub struct Provider {
  pub id: &'static str,
  pub name: &'static str,
  pub wire: Wire,
  pub base_url: &'static str,
  pub key_env: &'static str,
  pub default_model: Option<&'static str>,
}

// Everything but Anthropic speaks the OpenAI Chat Completions format.
pub const PROVIDERS: &[Provider] = &[
  Provider {
    id: "anthropic",
    name: "Anthropic",
    wire: Wire::Anthropic,
    base_url: "https://api.anthropic.com",
    key_env: "ANTHROPIC_API_KEY",
    default_model: Some(ANTHROPIC_DEFAULT_MODEL),
  },
  Provider {
    id: "openai",
    name: "OpenAI",
    wire: Wire::OpenAi,
    base_url: "https://api.openai.com/v1",
    key_env: "OPENAI_API_KEY",
    default_model: None,
  },
  Provider {
    id: "openrouter",
    name: "OpenRouter",
    wire: Wire::OpenAi,
    base_url: "https://openrouter.ai/api/v1",
    key_env: "OPENROUTER_API_KEY",
    default_model: None,
  },
  Provider {
    id: "gemini",
    name: "Google Gemini",
    wire: Wire::OpenAi,
    base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
    key_env: "GEMINI_API_KEY",
    default_model: None,
  },
  Provider {
    id: "groq",
    name: "Groq",
    wire: Wire::OpenAi,
    base_url: "https://api.groq.com/openai/v1",
    key_env: "GROQ_API_KEY",
    default_model: None,
  },
  Provider {
    id: "mistral",
    name: "Mistral",
    wire: Wire::OpenAi,
    base_url: "https://api.mistral.ai/v1",
    key_env: "MISTRAL_API_KEY",
    default_model: None,
  },
  Provider {
    id: "deepseek",
    name: "DeepSeek",
    wire: Wire::OpenAi,
    base_url: "https://api.deepseek.com/v1",
    key_env: "DEEPSEEK_API_KEY",
    default_model: None,
  },
  Provider {
    id: "xai",
    name: "xAI",
    wire: Wire::OpenAi,
    base_url: "https://api.x.ai/v1",
    key_env: "XAI_API_KEY",
    default_model: None,
  },
  Provider {
    id: "ollama",
    name: "Ollama (local)",
    wire: Wire::OpenAi,
    base_url: "http://localhost:11434/v1",
    key_env: "",
    default_model: None,
  },
  Provider {
    id: "custom",
    name: "Other (OpenAI-compatible)",
    wire: Wire::OpenAi,
    base_url: "",
    key_env: "",
    default_model: None,
  },
];

pub fn provider(id: &str) -> Result<&'static Provider> {
  PROVIDERS.iter().find(|p| p.id == id).ok_or_else(|| {
    let ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
    anyhow!("unknown AI provider {id:?}; use one of: {}", ids.join(", "))
  })
}

// ~/.config/mig/ai.toml (no secrets in here).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct AiConfig {
  pub provider: String,
  pub model: String,
  pub base_url: String,
  // Language for generated text; empty means English.
  pub language: String,
}

impl Default for AiConfig {
  fn default() -> Self {
    AiConfig {
      provider: "anthropic".into(),
      model: String::new(),
      base_url: String::new(),
      language: String::new(),
    }
  }
}

fn config_dir() -> Result<PathBuf> {
  Ok(
    dirs::config_dir()
      .context("no config folder on this system")?
      .join("mig"),
  )
}

impl AiConfig {
  pub fn load() -> AiConfig {
    config_dir()
      .ok()
      .and_then(|d| std::fs::read_to_string(d.join("ai.toml")).ok())
      .and_then(|s| toml::from_str(&s).ok())
      .unwrap_or_default()
  }

  pub fn save(&self) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("ai.toml"), toml::to_string_pretty(self)?)?;
    Ok(())
  }

  pub fn provider(&self) -> Result<&'static Provider> {
    provider(&self.provider)
  }

  pub fn model(&self) -> Result<String> {
    if !self.model.trim().is_empty() {
      return Ok(self.model.trim().to_string());
    }
    let p = self.provider()?;
    p.default_model.map(str::to_string).ok_or_else(|| {
      anyhow!(
        "choose a model for {} (`mig ai config --model NAME` or Settings → AI)",
        p.name
      )
    })
  }

  pub fn base_url(&self) -> Result<String> {
    let url = if self.base_url.trim().is_empty() {
      self.provider()?.base_url
    } else {
      self.base_url.trim()
    };
    if url.is_empty() {
      bail!("set a base URL for the custom provider (`mig ai config --base-url URL`)");
    }
    Ok(url.trim_end_matches('/').to_string())
  }
}

// API keys live apart from the rest of the settings, readable only by the user.
fn credentials_file() -> Result<PathBuf> {
  Ok(config_dir()?.join("credentials.toml"))
}

fn load_credentials() -> BTreeMap<String, String> {
  credentials_file()
    .ok()
    .and_then(|p| std::fs::read_to_string(p).ok())
    .and_then(|s| toml::from_str(&s).ok())
    .unwrap_or_default()
}

pub fn store_key(provider_id: &str, key: Option<&str>) -> Result<()> {
  let mut creds = load_credentials();
  match key.map(str::trim).filter(|k| !k.is_empty()) {
    Some(k) => creds.insert(provider_id.to_string(), k.to_string()),
    None => creds.remove(provider_id),
  };
  let path = credentials_file()?;
  std::fs::create_dir_all(path.parent().unwrap())?;
  std::fs::write(&path, toml::to_string(&creds)?)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
  }
  Ok(())
}

#[derive(Debug, PartialEq)]
pub enum KeySource {
  Env(&'static str),
  Stored,
  NotNeeded,
  Missing,
}

pub fn find_key(p: &Provider) -> (Option<String>, KeySource) {
  if !p.key_env.is_empty()
    && let Ok(k) = std::env::var(p.key_env)
    && !k.trim().is_empty()
  {
    return (Some(k.trim().to_string()), KeySource::Env(p.key_env));
  }
  if let Some(k) = load_credentials().get(p.id) {
    return (Some(k.clone()), KeySource::Stored);
  }
  if p.id == "ollama" || p.id == "custom" {
    return (None, KeySource::NotNeeded);
  }
  (None, KeySource::Missing)
}

pub struct Reply {
  pub text: String,
  pub redacted: usize,
}

fn agent() -> ureq::Agent {
  ureq::Agent::config_builder()
    .timeout_global(Some(TIMEOUT))
    .http_status_as_error(false)
    .build()
    .into()
}

// One system + user exchange. Secrets in `user` are redacted before anything is sent.
pub fn ask(cfg: &AiConfig, system: &str, user: &str) -> Result<Reply> {
  let p = cfg.provider()?;
  let model = cfg.model()?;
  let base = cfg.base_url()?;
  let (key, source) = find_key(p);
  if source == KeySource::Missing {
    bail!(
      "no API key for {}: set {} or save one with `mig ai key {}` (or Settings → AI)",
      p.name,
      p.key_env,
      p.id
    );
  }
  let mut system = system.to_string();
  if !cfg.language.trim().is_empty() {
    system.push_str(&format!("\nWrite your answer in {}.", cfg.language.trim()));
  }
  let (user, redacted) = scan::redact(user);
  let text = match p.wire {
    Wire::Anthropic => anthropic(&base, &model, key.as_deref().unwrap_or(""), &system, &user)?,
    Wire::OpenAi => openai(&base, &model, key.as_deref(), &system, &user)?,
  };
  Ok(Reply {
    text: text.trim().to_string(),
    redacted,
  })
}

fn error_message(status: u16, body: &Value) -> String {
  let msg = body
    .pointer("/error/message")
    .or_else(|| body.pointer("/message"))
    .and_then(Value::as_str)
    .unwrap_or("");
  match status {
    401 | 403 => format!("the provider rejected the API key ({status}): {msg}"),
    404 => format!("not found ({status}); check the model name and base URL: {msg}"),
    429 => format!("rate limited ({status}); try again later: {msg}"),
    _ => format!("the provider returned {status}: {msg}"),
  }
}

fn anthropic(base: &str, model: &str, key: &str, system: &str, user: &str) -> Result<String> {
  let mut body = json!({
    "model": model,
    "max_tokens": 16000,
    "system": system,
    "messages": [{"role": "user", "content": user}],
  });
  // Server-side fallback re-runs a declined request on another model; only on the first-party API
  // and the models that accept it.
  let fallback = base == "https://api.anthropic.com"
    && [
      "claude-fable-5-1",
      "claude-opus-5-5",
      "claude-opus-5",
      "claude-sonnet-5-5",
    ]
    .contains(&model);
  let mut req = agent()
    .post(&format!("{base}/v1/messages"))
    .header("x-api-key", key)
    .header("anthropic-version", "2023-06-01")
    .header("content-type", "application/json");
  if fallback {
    body["fallbacks"] = json!("default");
    req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
  }
  let mut resp = req.send_json(&body).context("could not reach the Anthropic API")?;
  let status = resp.status().as_u16();
  let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
  if status != 200 {
    bail!(error_message(status, &v));
  }
  if v["stop_reason"] == "refusal" {
    let why = v
      .pointer("/stop_details/explanation")
      .and_then(Value::as_str)
      .unwrap_or("no reason given");
    bail!("the model declined this request: {why}");
  }
  let text: Vec<&str> = v["content"]
    .as_array()
    .map(|blocks| {
      blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect()
    })
    .unwrap_or_default();
  if text.is_empty() {
    bail!("the model returned no text");
  }
  Ok(text.join(""))
}

fn openai(base: &str, model: &str, key: Option<&str>, system: &str, user: &str) -> Result<String> {
  let body = json!({
    "model": model,
    "messages": [
      {"role": "system", "content": system},
      {"role": "user", "content": user},
    ],
  });
  let mut req = agent()
    .post(&format!("{base}/chat/completions"))
    .header("content-type", "application/json");
  if let Some(k) = key {
    req = req.header("authorization", &format!("Bearer {k}"));
  }
  if base.contains("openrouter.ai") {
    req = req.header("X-Title", "mig");
  }
  let mut resp = req
    .send_json(&body)
    .with_context(|| format!("could not reach {base}"))?;
  let status = resp.status().as_u16();
  let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
  if status != 200 {
    bail!(error_message(status, &v));
  }
  let choice = &v["choices"][0];
  if choice["finish_reason"] == "content_filter" {
    bail!("the provider filtered this response");
  }
  choice["message"]["content"]
    .as_str()
    .map(str::to_string)
    .filter(|t| !t.trim().is_empty())
    .ok_or_else(|| anyhow!("the model returned no text"))
}

// Cut long input on a character boundary and say so, rather than dropping it silently.
pub fn clip(text: &str, max: usize) -> (String, bool) {
  if text.len() <= max {
    return (text.to_string(), false);
  }
  let mut end = max;
  while !text.is_char_boundary(end) {
    end -= 1;
  }
  (
    format!(
      "{}\n[... truncated: {} more bytes not shown ...]\n",
      &text[..end],
      text.len() - end
    ),
    true,
  )
}
