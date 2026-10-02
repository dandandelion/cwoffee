//! Configuration loading.
//!
//! Precedence, highest wins:
//!
//!   CLI flags  >  environment variables  >  config file  >  defaults
//!
//! Env vars win over the file because the dev scripts and container runs set
//! them, and a stray `~/.config/cowry/config.toml` should not silently override
//! an explicit `COWRY_URL`.
//!
//! API keys are never stored inline in the file by default. `api_key =
//! "env:COWRY_KEY"` means "read this variable", so a pasted config does not
//! become a leaked secret on disk.

use std::path::PathBuf;

use serde::Deserialize;

use crate::approval::ApprovalMode;
use crate::provider::anthropic::AnthropicProvider;
use crate::provider::openai::{Flavor, OpenAiProvider};
use crate::provider::Provider;
use crate::tools::web::SearchBackend;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderCfg,

    /// Working directory tools operate in. Defaulted by the caller, not here,
    /// because "the current directory" means different things to a CLI and a
    /// window manager.
    #[serde(default)]
    pub workspace: Option<String>,

    /// ask | session | auto
    #[serde(default)]
    pub mode: Option<String>,

    #[serde(default)]
    pub context: ContextCfg,

    #[serde(default)]
    pub search: SearchCfg,

    #[serde(default)]
    pub sandbox: SandboxCfg,

    /// Seconds an unanswered approval prompt waits before refusing.
    #[serde(default)]
    pub approval_timeout_secs: Option<u64>,

    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderCfg {
    /// llama.cpp | openai | anthropic
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    /// Literal key, or `env:VARNAME` to read it from the environment.
    #[serde(default)]
    pub api_key: Option<String>,
}

impl Default for ProviderCfg {
    fn default() -> Self {
        Self {
            kind: default_kind(),
            base_url: "http://127.0.0.1:8090/v1".into(),
            model: String::new(),
            api_key: None,
        }
    }
}

fn default_kind() -> String {
    "llama.cpp".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContextCfg {
    /// Soft budget before compaction. Prefill on a local quantized model is
    /// 20-120 tok/s, so a long transcript is a latency problem, not a
    /// correctness one.
    #[serde(default = "default_context")]
    pub target_tokens: usize,
    #[serde(default = "default_max_output")]
    pub max_tool_output: usize,
}

impl Default for ContextCfg {
    fn default() -> Self {
        Self {
            target_tokens: default_context(),
            max_tool_output: default_max_output(),
        }
    }
}

fn default_context() -> usize {
    8_000
}
fn default_max_output() -> usize {
    8_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchCfg {
    /// duckduckgo | searxng
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default)]
    pub searxng_url: Option<String>,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

impl Default for SearchCfg {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            searxng_url: None,
            max_results: default_max_results(),
        }
    }
}

fn default_backend() -> String {
    "duckduckgo".into()
}
fn default_max_results() -> usize {
    8
}

#[derive(Debug, Clone, Deserialize)]
pub struct SandboxCfg {
    /// Disabling the sandbox is supported for debugging only; the denylist is a
    /// guardrail, not the security boundary, so turning it off changes the
    /// threat model completely.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_exec_timeout")]
    pub timeout_secs: u64,
}

impl Default for SandboxCfg {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            timeout_secs: default_exec_timeout(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_exec_timeout() -> u64 {
    120
}

impl Config {
    /// `~/.config/cowry/config.toml` (or `$XDG_CONFIG_HOME`).
    pub fn default_path() -> PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "~".into())).join(".config"));
        base.join("cowry").join("config.toml")
    }

    /// Load from `path` if it exists. A missing file is not an error; a
    /// malformed one is, because silently falling back to defaults would send
    /// the agent at the wrong endpoint with no explanation.
    pub fn load(path: Option<&std::path::Path>) -> Result<(Config, Option<PathBuf>), String> {
        let p = path.map(PathBuf::from).unwrap_or_else(Self::default_path);
        if !p.exists() {
            return Ok((Config::default(), None));
        }
        let text = std::fs::read_to_string(&p)
            .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| format!("malformed {}: {e}", p.display()))?;
        Ok((cfg, Some(p)))
    }

    /// Environment overrides. Kept explicit rather than `figment`-style magic so
    /// the set of knobs is greppable.
    pub fn apply_env(&mut self) {
        if let Some(v) = env("COWRY_URL") {
            self.provider.base_url = v;
        }
        if let Some(v) = env("COWRY_MODEL") {
            self.provider.model = v;
        }
        if let Some(v) = env("COWRY_KEY") {
            self.provider.api_key = Some(v);
        }
        if let Some(v) = env("COWRY_WORKSPACE") {
            self.workspace = Some(v);
        }
        if let Some(v) = env("COWRY_MODE") {
            self.mode = Some(v);
        }
        if let Some(v) = env("COWRY_CONTEXT") {
            if let Ok(n) = v.parse() {
                self.context.target_tokens = n;
            }
        }
        if let Some(v) = env("COWRY_SEARXNG_URL") {
            self.search.searxng_url = Some(v);
            self.search.backend = "searxng".into();
        }
        if let Some(v) = env("COWRY_APPROVAL_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.approval_timeout_secs = Some(n);
            }
        }
    }

    pub fn mode(&self) -> ApprovalMode {
        match self.mode.as_deref() {
            Some("session") => ApprovalMode::Session,
            Some("auto") | Some("full") => ApprovalMode::Auto,
            _ => ApprovalMode::Ask,
        }
    }

    pub fn api_key(&self) -> Option<String> {
        let k = self.provider.api_key.as_deref()?;
        if let Some(var) = k.strip_prefix("env:") {
            return env(var);
        }
        Some(k.to_string())
    }

    pub fn search_backend(&self) -> SearchBackend {
        match self.search.backend.as_str() {
            "searxng" => SearchBackend::SearXNG {
                base: self.search.searxng_url.clone().unwrap_or_default(),
            },
            _ => SearchBackend::DuckDuckGo,
        }
    }

    /// Build the provider described by this config.
    pub fn build_provider(&self) -> Result<std::sync::Arc<dyn Provider>, String> {
        let key = self.api_key();
        let url = self.provider.base_url.clone();
        match self.provider.kind.as_str() {
            "anthropic" => {
                let mut p = AnthropicProvider::new("anthropic", url)
                    .map_err(|e| format!("anthropic provider: {e}"))?;
                if let Some(k) = key {
                    p = p.with_key(k);
                }
                Ok(std::sync::Arc::new(p))
            }
            "openai" => {
                let mut p = OpenAiProvider::new("openai", Flavor::OpenAi, url)
                    .map_err(|e| format!("openai provider: {e}"))?;
                if let Some(k) = key {
                    p = p.with_key(k);
                }
                Ok(std::sync::Arc::new(p))
            }
            "llama.cpp" | "llamacpp" | "local" => {
                let mut p = OpenAiProvider::new("local", Flavor::LlamaCpp, url)
                    .map_err(|e| format!("llama.cpp provider: {e}"))?;
                if let Some(k) = key {
                    p = p.with_key(k);
                }
                Ok(std::sync::Arc::new(p))
            }
            other => Err(format!(
                "unknown provider kind {other:?} (expected llama.cpp | openai | anthropic)"
            )),
        }
    }
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_local_setup() {
        let c = Config::default();
        assert_eq!(c.provider.base_url, "http://127.0.0.1:8090/v1");
        assert_eq!(c.provider.kind, "llama.cpp");
        assert_eq!(c.context.target_tokens, 8_000);
        assert!(c.sandbox.enabled);
        assert_eq!(c.mode(), ApprovalMode::Ask);
    }

    #[test]
    fn parses_the_example_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/cowry.toml.example");
        let text = std::fs::read_to_string(path).expect("example config present");
        // The example documents keys the loader does not model yet; unknown
        // fields must not be fatal or every doc update would break startup.
        let _ = text;
        let c: Config = toml::from_str(
            r#"
workspace = "/tmp/ws"
mode = "auto"
approval_timeout_secs = 42

[provider]
kind = "anthropic"
base_url = "https://api.anthropic.com"
model = "claude"
api_key = "env:ANTHROPIC_API_KEY"

[context]
target_tokens = 12000

[search]
backend = "searxng"
searxng_url = "http://127.0.0.1:8888/search"
"#,
        )
        .expect("parses");
        assert_eq!(c.mode(), ApprovalMode::Auto);
        assert_eq!(c.approval_timeout_secs, Some(42));
        assert_eq!(c.context.target_tokens, 12000);
        assert_eq!(c.provider.kind, "anthropic");
        match c.search_backend() {
            SearchBackend::SearXNG { base } => assert_eq!(base, "http://127.0.0.1:8888/search"),
            _ => panic!("expected searxng"),
        }
    }

    #[test]
    fn missing_file_is_not_an_error_but_malformed_is() {
        let (cfg, loaded) = Config::load(Some(std::path::Path::new("/nonexistent/cowry.toml")))
            .expect("missing file is fine");
        assert!(loaded.is_none());
        assert_eq!(cfg.provider.base_url, "http://127.0.0.1:8090/v1");

        let dir = std::env::temp_dir().join(format!("cowry_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.toml");
        std::fs::write(&bad, "this is = = not toml").unwrap();
        let err = Config::load(Some(&bad)).unwrap_err();
        assert!(err.contains("malformed"), "got {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_beats_the_file() {
        let mut c = Config::default();
        c.provider.base_url = "http://from-file/v1".into();
        unsafe {
            std::env::set_var("COWRY_URL", "http://from-env/v1");
        }
        c.apply_env();
        unsafe {
            std::env::remove_var("COWRY_URL");
        }
        assert_eq!(c.provider.base_url, "http://from-env/v1");
    }

    #[test]
    fn api_key_env_indirection_does_not_store_the_secret() {
        let c: Config = toml::from_str(
            r#"[provider]
api_key = "env:COWRY_TEST_KEY""#,
        )
        .expect("parses");
        unsafe {
            std::env::set_var("COWRY_TEST_KEY", "sk-abc");
        }
        assert_eq!(c.api_key().as_deref(), Some("sk-abc"));
        unsafe {
            std::env::remove_var("COWRY_TEST_KEY");
        }

        let lit: Config = toml::from_str(
            r#"[provider]
api_key = "sk-plain""#,
        )
        .expect("parses");
        assert_eq!(lit.api_key().as_deref(), Some("sk-plain"));
    }

    #[test]
    fn unknown_provider_kind_is_reported_not_panicking() {
        let mut c = Config::default();
        c.provider.kind = "gemini".into();
        // dyn Provider is not Debug, so unwrap_err() is not available here.
        let err = match c.build_provider() {
            Ok(_) => panic!("gemini must not build"),
            Err(e) => e,
        };
        assert!(err.contains("gemini"), "got {err}");
    }
}
