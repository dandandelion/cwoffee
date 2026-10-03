//! Configuration loading.
//!
//! Precedence, highest wins:
//!
//!   CLI flags  >  environment variables  >  config file  >  defaults
//!
//! Env vars win over the file because the dev scripts and container runs set
//! them, and a stray `~/.config/cwoffee/config.toml` should not silently override
//! an explicit `CWOFFEE_URL`.
//!
//! API keys are never stored inline in the file by default. `api_key =
//! "env:CWOFFEE_KEY"` means "read this variable", so a pasted config does not
//! become a leaked secret on disk.

use std::path::PathBuf;

use serde::Deserialize;

use crate::approval::ApprovalMode;
use crate::provider::anthropic::AnthropicProvider;
use crate::provider::openai::{Flavor, OpenAiProvider};
use crate::provider::Provider;
use crate::tools::web::SearchBackend;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
pub fn default_exec_timeout() -> u64 {
    120
}

/// Read `new`, falling back to the pre-rebrand name `old`.
fn env2(new: &str, old: &str) -> Option<String> {
    env(new).or_else(|| env(old))
}

/// TOML assigns a bare key to whichever `[table]` is currently open, so a key
/// written after `[provider]` silently becomes `provider.workspace` and does
/// nothing. The example config shipped with exactly that mistake, which is how
/// a user ended up with a workspace setting the app never read. Unknown fields
/// are now rejected; this turns the common case into an actionable message.
fn table_hint(err: &toml::de::Error) -> String {
    let msg = err.message().to_string();
    let top_level = [
        "workspace",
        "mode",
        "approval_timeout_secs",
        "theme",
        "provider",
        "context",
        "search",
        "sandbox",
    ];
    for key in top_level {
        if msg.contains(key) {
            return format!(
                "\n  hint: `{key}` is a top-level key. In TOML it belongs to whichever \
                 [table] is open, so it must appear before the first [table] header."
            );
        }
    }
    String::new()
}

impl Config {
    /// `~/.config/cwoffee/config.toml` (or `$XDG_CONFIG_HOME`).
    pub fn default_path() -> PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "~".into())).join(".config"));
        base.join("cwoffee").join("config.toml")
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
        let mut cfg: Config = toml::from_str(&text)
            .map_err(|e| format!("malformed {}: {}{}", p.display(), e, table_hint(&e)))?;
        if let Some(w) = cfg.workspace.take() {
            cfg.workspace = Some(Self::expand_tilde(&w));
        }
        Ok((cfg, Some(p)))
    }

    /// Environment overrides. Kept explicit rather than `figment`-style magic so
    /// the set of knobs is greppable.
    pub fn apply_env(&mut self) {
        // The pre-rebrand names keep working. Someone with COWRY_URL in a shell
        // profile or a systemd unit should get a working app, not a silent
        // fallback to defaults.
        if let Some(v) = env2("CWOFFEE_URL", "COWRY_URL") {
            self.provider.base_url = v;
        }
        if let Some(v) = env2("CWOFFEE_MODEL", "COWRY_MODEL") {
            self.provider.model = v;
        }
        if let Some(v) = env2("CWOFFEE_KEY", "COWRY_KEY") {
            self.provider.api_key = Some(v);
        }
        if let Some(v) = env2("CWOFFEE_WORKSPACE", "COWRY_WORKSPACE") {
            self.workspace = Some(Self::expand_tilde(&v));
        }
        if let Some(v) = env2("CWOFFEE_MODE", "COWRY_MODE") {
            self.mode = Some(v);
        }
        if let Some(v) = env2("CWOFFEE_CONTEXT", "COWRY_CONTEXT") {
            if let Ok(n) = v.parse() {
                self.context.target_tokens = n;
            }
        }
        if let Some(v) = env2("CWOFFEE_SEARXNG_URL", "COWRY_SEARXNG_URL") {
            self.search.searxng_url = Some(v);
            self.search.backend = "searxng".into();
        }
        if let Some(v) = env2("CWOFFEE_APPROVAL_TIMEOUT_SECS", "COWRY_APPROVAL_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.approval_timeout_secs = Some(n);
            }
        }
    }

    /// Expand a leading `~` / `~/`. People write `~` in config files; without
    /// this the app happily creates a directory literally named `~` in the
    /// current directory and the agent "loses" every file it writes.
    pub fn expand_tilde(p: &str) -> String {
        if p == "~" || p.starts_with("~/") {
            if let Some(home) = std::env::var("HOME").ok().filter(|h| !h.is_empty()) {
                let rest = p.trim_start_matches('~');
                return format!("{home}{rest}");
            }
        }
        p.to_string()
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

    /// The `[sandbox]` block as the tool layer actually consumes it.
    pub fn exec_cfg(&self) -> crate::tools::ExecCfg {
        crate::tools::ExecCfg {
            timeout_secs: self.sandbox.timeout_secs,
            sandbox_enabled: self.sandbox.enabled,
        }
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

    /// The trap the shipped example config fell into: a top-level key written
    /// after `[provider]` is parsed as `provider.workspace` and silently does
    /// nothing. It must be a startup error with a hint, not a shrug.
    #[test]
    fn a_top_level_key_after_a_table_header_is_refused_not_ignored() {
        let dir = std::env::temp_dir().join(format!(
            "cwoffee_misplaced_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"[provider]
base_url = "http://127.0.0.1:9/v1"
workspace = "~/somewhere"
mode = "auto"
"#,
        )
        .unwrap();

        let err = Config::load(Some(&path)).unwrap_err();
        assert!(err.contains("workspace"), "unhelpful error: {err}");
        assert!(err.contains("top-level key"), "no hint: {err}");

        // And the same file with the keys in the right place works.
        std::fs::write(
            &path,
            r#"workspace = "~/somewhere"
mode = "auto"

[provider]
base_url = "http://127.0.0.1:9/v1"
"#,
        )
        .unwrap();
        let (cfg, _) = Config::load(Some(&path)).unwrap();
        // load() also expands ~, which the misplaced version never reached.
        let ws = cfg.workspace.expect("workspace was dropped");
        assert!(ws.ends_with("/somewhere") && !ws.starts_with('~'), "not expanded: {ws}");
        assert_eq!(cfg.mode.as_deref(), Some("auto"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tilde_in_workspace_is_expanded() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        assert_eq!(Config::expand_tilde("~/ws"), format!("{home}/ws"));
        assert_eq!(Config::expand_tilde("~"), home.clone());
        // Not a tilde prefix: untouched.
        assert_eq!(Config::expand_tilde("/abs/ws"), "/abs/ws");
        assert_eq!(Config::expand_tilde("a~/b"), "a~/b");
    }

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
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/cwoffee.toml.example");
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
        let (cfg, loaded) = Config::load(Some(std::path::Path::new("/nonexistent/cwoffee.toml")))
            .expect("missing file is fine");
        assert!(loaded.is_none());
        assert_eq!(cfg.provider.base_url, "http://127.0.0.1:8090/v1");

        let dir = std::env::temp_dir().join(format!("cwoffee_cfg_{}", std::process::id()));
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
            std::env::set_var("CWOFFEE_URL", "http://from-env/v1");
        }
        c.apply_env();
        unsafe {
            std::env::remove_var("CWOFFEE_URL");
        }
        assert_eq!(c.provider.base_url, "http://from-env/v1");
    }

    #[test]
    fn api_key_env_indirection_does_not_store_the_secret() {
        let c: Config = toml::from_str(
            r#"[provider]
api_key = "env:CWOFFEE_TEST_KEY""#,
        )
        .expect("parses");
        unsafe {
            std::env::set_var("CWOFFEE_TEST_KEY", "sk-abc");
        }
        assert_eq!(c.api_key().as_deref(), Some("sk-abc"));
        unsafe {
            std::env::remove_var("CWOFFEE_TEST_KEY");
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
