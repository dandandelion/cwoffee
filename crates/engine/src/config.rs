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

    /// Sampling overrides for the inference endpoint. Every key is optional, and
    /// a key that is absent is not sent at all - the server's default then
    /// decides, which is the right answer for an endpoint someone else tuned.
    ///
    /// The section exists because temperature 1.0 - llama.cpp's default, and what
    /// this project's own endpoint reports - is a fine creative setting and a poor
    /// one for tool-call JSON: a long agentic turn can drift, and the failure
    /// shows up as garbage at the tail of an answer, far from its cause.
    #[serde(default)]
    pub sampling: crate::provider::Sampling,

    /// Seconds an unanswered approval prompt waits before refusing.
    #[serde(default)]
    pub approval_timeout_secs: Option<u64>,

    /// Hard cap on tokens the model may produce in one round-trip. It covers
    /// thinking and answer **together**, which is why a low number looks like the
    /// model "didn't get to the answer": the thinking ate the budget.
    ///
    /// Unset means Cwoffee asks for no cap and the server's own `-np` /
    /// `n_predict` applies. Either way a turn that stops at the limit is now
    /// reported and continued instead of ending mid-sentence.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,

    #[serde(default)]
    pub theme: Option<String>,

    /// Provider round-trips allowed inside one user turn. **0 or absent = no cap.**
    ///
    /// This used to be a hardcoded 14, which was not a runaway guard but a work
    /// budget: a session asked to deep-read an 8-sheet workbook used all 14
    /// iterations on reads and was then forced to summarise work it had not
    /// finished. Set a number when you want a hard stop on a runaway model.
    #[serde(default)]
    pub max_tool_iters: Option<usize>,
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
    ///
    /// Left unset, the budget comes from the model's own context window: 72% of
    /// it, minus headroom. It used to be a fixed 8 000 tokens, which compacted a
    /// 262 144-token model at 8k and then told the user the context was full.
    #[serde(default)]
    pub target_tokens: Option<usize>,
    #[serde(default = "default_max_output")]
    pub max_tool_output: usize,
}

impl Default for ContextCfg {
    fn default() -> Self {
        Self {
            target_tokens: None,
            max_tool_output: default_max_output(),
        }
    }
}

fn default_max_output() -> usize {
    8_000
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCfg {
    /// duckduckgo | searxng | auto
    ///
    /// `auto` is the one that needs nothing from the person: Cwoffee runs its own
    /// SearXNG in a container on first search and stops it when it goes idle. See
    /// `tools::searxng`.
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default)]
    pub searxng_url: Option<String>,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    /// `auto` only: the port Cwoffee publishes *its* instance on. Not 8888, which
    /// is where an instance from elsewhere usually already lives.
    #[serde(default = "default_auto_port")]
    pub auto_port: u16,
    /// `auto` only: how long an unused container is worth keeping.
    #[serde(default = "default_auto_idle")]
    pub auto_idle_secs: u64,
    /// `auto` only: where the generated `settings.yml` / `limiter.toml` live.
    /// Default: `<config dir>/searxng`.
    #[serde(default)]
    pub auto_dir: Option<String>,
}

impl Default for SearchCfg {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            searxng_url: None,
            max_results: default_max_results(),
            auto_port: default_auto_port(),
            auto_idle_secs: default_auto_idle(),
            auto_dir: None,
        }
    }
}

fn default_backend() -> String {
    "duckduckgo".into()
}
fn default_max_results() -> usize {
    8
}
fn default_auto_port() -> u16 {
    crate::tools::searxng::DEFAULT_AUTO_PORT
}
fn default_auto_idle() -> u64 {
    crate::tools::searxng::DEFAULT_IDLE_SECS
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
            if let Ok(n) = v.parse::<usize>() {
                self.context.target_tokens = Some(n);
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

    /// Edit `[search]` keys in a config file's *text*, leaving every other line -
    /// comments included - exactly as it was. A settings panel that re-serialised
    /// the whole file would flatten a hand-annotated config into a generated one,
    /// and the notes people leave themselves ("this endpoint is the slow box") are
    /// the thing most worth keeping.
    ///
    /// Values are TOML literals, already quoted: `toml_str("auto")` or `"8899"`.
    /// Returns a description of each change so the caller can say what it did.
    pub fn set_search_keys_in(text: &str, kvs: &[(&str, &str)]) -> (String, Vec<String>) {
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        let header = lines.iter().position(|l| l.trim() == "[search]");
        let mut changed = Vec::new();
        let Some(h) = header else {
            if !text.is_empty() && !text.ends_with('\n') {
                lines.push(String::new());
            }
            lines.push("[search]".into());
            for (k, v) in kvs {
                lines.push(format!("{k} = {v}"));
                changed.push(format!("{k} = {v} (new)"));
            }
            let mut out = lines.join("\n");
            out.push('\n');
            return (out, changed)
        };
        // The section ends at the next header, so a `[sandbox] backend = ...` (no
        // such key today, but any future collision) cannot be edited by accident.
        let end = (h + 1..lines.len())
            .find(|i| {
                let t = lines[*i].trim();
                t.starts_with('[') && t.ends_with(']')
            })
            .unwrap_or(lines.len());
        for (k, v) in kvs {
            // A commented-out key counts: `# searxng_url = "..."` is the example in
            // the shipped config, and adding a second line would leave two, one of
            // them a lie.
            let at = (h + 1..end).find(|i| {
                let t = lines[*i].trim();
                let t = t.trim_start_matches('#').trim();
                t.starts_with(&format!("{k} ")) || t.starts_with(&format!("{k}="))
            });
            match at {
                Some(i) => {
                    let old = lines[i].trim().to_string();
                    lines[i] = format!("{k} = {v}");
                    changed.push(format!("{k}: {old} -> {k} = {v}"));
                }
                None => {
                    // Insert at the end of the section rather than at the top: the
                    // first key in a section often carries the section's comment.
                    let mut insert = end;
                    while insert > h + 1 && lines[insert - 1].trim().is_empty() {
                        insert -= 1;
                    }
                    lines.insert(insert, format!("{k} = {v}"));
                    changed.push(format!("{k} = {v} (new)"));
                }
            }
        }
        let mut out = lines.join("\n");
        if text.ends_with('\n') || !out.ends_with('\n') {
            out.push('\n');
        }
        (out, changed)
    }

    /// A TOML string literal, with the quotes and escapes a hand-written config
    /// would have.
    pub fn toml_str(v: &str) -> String {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// Write `[search]` keys to `path`, creating the file if the person has never
    /// needed one. This is the persistence behind the settings panel: a provider
    /// that resets on restart is a provider that was never really enabled.
    pub fn set_search_keys(path: &std::path::Path, kvs: &[(&str, &str)]) -> Result<Vec<String>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let (out, changed) = Self::set_search_keys_in(&text, kvs);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        // Write-then-rename: a crash half-way through a config write leaves the
        // old file intact rather than a truncated one the next boot refuses.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, out.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The file can come to hold an endpoint key, so it is not world-readable.
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
        Ok(changed)
    }

    /// 0 means "no cap"; see the field comment for why that is the default.
    pub fn max_tool_iters(&self) -> usize {
        self.max_tool_iters.unwrap_or(0)
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

    /// Validate `[sampling]` before anything talks to the model. A typo'd range
    /// that is quietly ignored turns into "the model behaves oddly" with no trace
    /// of the cause, which is the most expensive kind of config bug there is.
    pub fn sampling_checked(&self) -> Result<crate::provider::Sampling, String> {
        self.sampling.clone().checked().map(|()| self.sampling.clone())
    }

    pub fn search_backend(&self) -> SearchBackend {
        match self.search.backend.as_str() {
            "searxng" => SearchBackend::SearXNG {
                base: self.search.searxng_url.clone().unwrap_or_default(),
            },
            "auto" => SearchBackend::Auto(crate::tools::searxng::AutoSearXNG::new(
                self.search.auto_port,
                self.search.auto_idle_secs,
                self.search_auto_dir(),
            )),
            _ => SearchBackend::DuckDuckGo,
        }
    }

    /// The directory `auto` owns. Under the config dir, next to config.toml, so a
    /// person can find and read the settings we generated rather than having them
    /// appear in a cache nobody looks in.
    pub fn search_auto_dir(&self) -> std::path::PathBuf {
        match self.search.auto_dir.as_deref().filter(|s| !s.is_empty()) {
            Some(d) => std::path::PathBuf::from(Self::expand_tilde(d)),
            None => Self::default_path().parent().map(|p| p.to_path_buf()).unwrap_or_else(|| std::path::PathBuf::from(".")).join("searxng"),
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
    fn a_search_key_is_replaced_where_it_lives() {
        let text = "# my notes: this is the slow box\nmode = \"ask\"\n\n[search]\nbackend = \"duckduckgo\"\nmax_results = 8\n\n[sandbox]\ntimeout_secs = 30\n";
        let (out, changed) =
            Config::set_search_keys_in(text, &[("backend", &Config::toml_str("auto"))]);
        assert!(out.contains("backend = \"auto\""), "{out}");
        assert!(!out.contains("duckduckgo"), "{out}");
        // The person's comment and the neighbouring sections survive. This is the
        // whole reason the edit is textual instead of re-serialising the file.
        assert!(out.contains("# my notes: this is the slow box"));
        assert!(out.contains("[sandbox]\ntimeout_secs = 30"));
        assert_eq!(changed.len(), 1);
        assert!(changed[0].contains("backend"), "{changed:?}");
    }

    #[test]
    fn a_commented_key_is_uncommented_not_duplicated() {
        // The shipped example carries `# searxng_url = "..."`. Leaving it commented
        // and adding a live line would put two answers for one key in the file.
        let text = "[search]\nbackend = \"searxng\"\n# searxng_url = \"http://127.0.0.1:8888/search\"\n";
        let (out, _) = Config::set_search_keys_in(
            text,
            &[("searxng_url", &Config::toml_str("http://127.0.0.1:8080/search"))],
        );
        assert_eq!(out.matches("searxng_url").count(), 1, "{out}");
        assert!(out.contains("searxng_url = \"http://127.0.0.1:8080/search\""), "{out}");
        assert!(!out.contains("# searxng_url"), "{out}");
    }

    #[test]
    fn a_missing_section_is_appended() {
        let text = "[provider]\nbase_url = \"http://127.0.0.1:8090/v1\"\n";
        let (out, changed) =
            Config::set_search_keys_in(text, &[("backend", &Config::toml_str("auto"))]);
        assert!(out.contains("[provider]\nbase_url"), "{out}");
        assert!(out.contains("\n[search]\nbackend = \"auto\"\n"), "{out}");
        assert!(changed[0].ends_with("(new)"), "{changed:?}");
        // And the result parses, which is the only promise that matters.
        let cfg: Config = toml::from_str(&out).expect("appended section must parse");
        assert_eq!(cfg.search.backend, "auto");
    }

    #[test]
    fn the_same_key_in_another_section_is_not_touched() {
        // Key lookup is scoped to the [search] section. Without that, a future
        // `[provider] backend = ...` (a real name: kind is called backend elsewhere)
        // would be edited by the search panel.
        let text = "[other]\nbackend = \"keep me\"\n\n[search]\nbackend = \"duckduckgo\"\n";
        let (out, _) = Config::set_search_keys_in(text, &[("backend", &Config::toml_str("auto"))]);
        assert!(out.contains("[other]\nbackend = \"keep me\""), "{out}");
        assert!(out.contains("[search]\nbackend = \"auto\""), "{out}");
    }

    #[test]
    fn written_search_keys_parse_back() {
        // Round trip through the parser: the point of writing the file is that the
        // next boot reads the same thing this panel believes it saved.
        let text = "[search]\nbackend = \"duckduckgo\"\n";
        let auto = Config::toml_str("auto");
        let url = Config::toml_str("http://127.0.0.1:8080/search");
        let kvs = [
            ("backend", auto.as_str()),
            ("auto_port", "8901"),
            ("auto_idle_secs", "90"),
            ("searxng_url", url.as_str()),
        ];
        let (out, _) = Config::set_search_keys_in(text, &kvs);
        let cfg: Config = toml::from_str(&out).expect("round trip must parse");
        assert_eq!(cfg.search.backend, "auto");
        assert_eq!(cfg.search.auto_port, 8901);
        assert_eq!(cfg.search.auto_idle_secs, 90);
        match cfg.search_backend() {
            SearchBackend::Auto(a) => {
                assert_eq!(a.port, 8901);
                assert_eq!(a.idle_secs, 90);
            }
            other => panic!("expected the managed backend, got {other:?}"),
        }
    }

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
        assert_eq!(c.context.target_tokens, None, "unset means derive it from the model");
        assert!(c.sandbox.enabled);
        assert_eq!(c.mode(), ApprovalMode::Ask);
    }

    #[test]
    fn parses_the_example_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/cwoffee.toml.example");
        let text = std::fs::read_to_string(path).expect("example config present");
        // The example is the file people copy, so it has to parse with the same
        // `deny_unknown_fields` the loader applies. Every key it documents must be
        // one the parser models; otherwise a doc update silently teaches people a
        // key that makes their config a startup error.
        let example: Config = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("config/cwoffee.toml.example must parse: {e}"));
        assert_eq!(example.search.backend, "auto", "the example ships the managed provider");
        assert_eq!(
            example.max_tool_iters(),
            0,
            "copying the example must not reintroduce a work budget on turns"
        );
        // The example is what people copy, so copying it must not change how the
        // model behaves. Sampling is documented but left unset.
        assert!(
            example.sampling.is_empty(),
            "the example must not quietly configure sampling: {:?}",
            example.sampling
        );
        match example.search_backend() {
            SearchBackend::Auto(a) => assert_eq!(a.port, crate::tools::searxng::DEFAULT_AUTO_PORT),
            other => panic!("expected the managed backend in the example, got {other:?}"),
        }
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
        assert_eq!(c.context.target_tokens, Some(12000));
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

    /// `[sampling]` exists so a person can tune an endpoint without Cwoffee
    /// deciding for them. The half that matters is the middle test: a key that is
    /// absent must stay absent from the request, not become our guess.
    #[test]
    fn the_tool_iteration_cap_is_a_choice_not_a_constant() {
        // Absent means no cap, which is the default behaviour now.
        let c: Config = toml::from_str("[provider]\nkind = 'llama.cpp'").expect("parses");
        assert_eq!(c.max_tool_iters(), 0, "no cap unless the file says otherwise");

        // A number means a hard stop, honoured exactly.
        let c: Config = toml::from_str("max_tool_iters = 40").expect("parses");
        assert_eq!(c.max_tool_iters(), 40);

        // 0 spelled out is the same as absent: "no cap", not "zero iterations".
        let c: Config = toml::from_str("max_tool_iters = 0").expect("parses");
        assert_eq!(c.max_tool_iters(), 0);
    }

    #[test]
    fn sampling_keys_are_optional_and_reach_the_host() {
        let c: Config = toml::from_str(
            r#"[provider]
kind = "llama.cpp"

[sampling]
temperature = 0.7
top_p = 0.9
top_k = 20
min_p = 0.05
seed = 1234"#,
        )
        .expect("parses");
        let s = c.sampling_checked().expect("in range");
        assert_eq!(s.temperature, Some(0.7));
        assert_eq!(s.top_p, Some(0.9));
        assert_eq!(s.top_k, Some(20));
        assert_eq!(s.min_p, Some(0.05));
        assert_eq!(s.seed, Some(1234));
    }

    #[test]
    fn an_absent_sampling_key_is_not_replaced_by_our_opinion() {
        let c: Config = toml::from_str(
            r#"[provider]
kind = "llama.cpp"

[sampling]
temperature = 0.6"#,
        )
        .expect("parses");
        let s = c.sampling_checked().expect("in range");
        assert_eq!(s.temperature, Some(0.6));
        // The whole point: one key set does not imply the others get filled in.
        assert_eq!(
            (s.top_p, s.top_k, s.min_p, s.seed),
            (None, None, None, None),
            "an unset key must stay unset"
        );
        assert!(!s.is_empty());
        assert!(
            Config::default().sampling.is_empty(),
            "a config with no [sampling] must send no sampling keys at all"
        );
    }

    #[test]
    fn an_out_of_range_or_misspelt_sampling_key_names_itself() {
        let bad: Config = toml::from_str(
            r#"[provider]
kind = "llama.cpp"

[sampling]
temperature = 11.0"#,
        )
        .expect("parses");
        let err = bad.sampling_checked().unwrap_err();
        assert!(err.contains("temperature") && err.contains("11"), "got {err}");

        let zero_k: Config = toml::from_str(
            r#"[sampling]
top_k = 0"#,
        )
        .expect("parses");
        let err = zero_k.sampling_checked().unwrap_err();
        assert!(err.contains("top_k"), "got {err}");

        // deny_unknown_fields means `topP` is a startup error rather than a key
        // that looks configured and does nothing.
        let typo = toml::from_str::<Config>(
            r#"[sampling]
topP = 0.9"#,
        );
        assert!(typo.is_err(), "a camelCase sampling key must be refused");
        let msg = typo.unwrap_err().to_string();
        assert!(msg.contains("topP"), "the message should name the typo: {msg}");
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
