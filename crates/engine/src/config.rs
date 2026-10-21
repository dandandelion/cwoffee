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

    /// off | low | medium | high | xhigh - the thinking chip in the composer.
    ///
    /// Declared here because it has to be: this parser rejects unknown keys on
    /// purpose, and the persistence layer writes the choices a user makes in the
    /// window. When that writer invented `reasoning` and `model` at the top level
    /// without the reader knowing either name, the file stopped parsing on the next
    /// boot and the app refused to start. A key the writer produces and the reader
    /// does not declare is not a missing feature - it is a broken start.
    #[serde(default)]
    pub reasoning: Option<String>,

    /// `cowork` or `chat`. Declared here because the window writes what a user picks:
    /// this parser rejects unknown keys on purpose, so a key the writer produces and
    /// the reader does not declare is a config file that stops booting.
    #[serde(default)]
    pub persona: Option<String>,

    #[serde(default)]
    pub context: ContextCfg,

    /// Every endpoint registered from the model screen, keyed by name.
    ///
    /// Declared here because this parser rejects unknown fields on purpose: a table the writer
    /// produces and the reader does not declare is not a missing feature, it is a config file
    /// that stops booting. (That has happened to this file before.)
    #[serde(default)]
    pub providers: std::collections::BTreeMap<String, ProviderRow>,

    /// What is known about each model — window, sampling, thinking levels, capabilities —
    /// keyed by `provider/model`. Same reason as `providers`: the reader has to know the table
    /// the settings screen writes, and "not set" is expressed by the key being absent.
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, ModelRow>,

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
    /// Which tools the model is **shown**, not which exist: `full` declares every mounted tool
    /// in every prompt (~12 KB, measured); `lean` declares only the tools the registry says
    /// must always be present and leaves the rest behind `explore`. Named for what it changes,
    /// because "tools" would read like an on/off switch for the tools themselves.
    #[serde(default)]
    pub tool_exposure: Option<String>,
    /// How long an `ask_user` card stays open before the model is told nobody
    /// answered. Absent means 180 seconds. Raise it when the answer is not yours to
    /// give: a question the user has to take to someone else cannot be answered
    /// inside three minutes, which is what session 1306 found. Range 15..=3600.
    #[serde(default)]
    pub question_timeout_secs: Option<u64>,
    /// How long a **manual compaction** may take before cwoffee stops waiting and says
    /// so. Absent means 1800 seconds. This is not a formality: folding a conversation
    /// around 200k tokens is a model call that summarizes all of it, and on a local
    /// endpoint that runs the clock at reading speed it takes many minutes. A short
    /// timeout does not make compaction faster; it makes a working compaction look
    /// broken, and the user pays for the summary twice.
    #[serde(default)]
    pub compact_timeout_secs: Option<u64>,
    /// Name conversations with a small model running on this machine, instead of cutting
    /// the opening line at forty-eight characters. **Off by default**: the helper peaks
    /// near 870 MB of resident memory for a few seconds per conversation, on a machine
    /// that is also running the model process and the UI, and it is not on any critical
    /// path — a conversation is never made to wait for its name.
    #[serde(default)]
    pub titlegen: Option<bool>,
    #[serde(default)]
    pub titlegen_python: Option<String>,
    #[serde(default)]
    pub titlegen_script: Option<String>,
    #[serde(default)]
    pub titlegen_model: Option<String>,
    #[serde(default)]
    pub titlegen_cache: Option<String>,
    #[serde(default)]
    pub titlegen_threads: Option<usize>,
    #[serde(default)]
    pub titlegen_timeout_secs: Option<u64>,
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
    /// What the exec sandbox may reach over the network:
    ///   "off"   - no network at all (the old behaviour)
    ///   "proxy" - network, but package managers go through an allowlist proxy
    ///             in this process (default)
    ///   "open"  - network, nothing in between
    /// `tools::net` states what each one does and does not guarantee.
    #[serde(default = "default_net_mode")]
    pub network: String,
    /// Hosts the proxy admits. Absent means the built-in package-registry list;
    /// `["*"]` means everything, with the proxy's log still running.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    /// Bytes through the proxy per exec call before it stops piping.
    #[serde(default = "default_net_mb")]
    pub max_mb_per_call: u64,
}

impl Default for SandboxCfg {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            timeout_secs: default_exec_timeout(),
            network: default_net_mode(),
            allow: None,
            max_mb_per_call: default_net_mb(),
        }
    }
}

fn default_true() -> bool {
    true
}
pub fn default_net_mode() -> String {
    // Proxy, not open: `npm install` should work out of the box, and an agent with
    // shell access should not be able to dial an arbitrary host on day one either.
    "proxy".into()
}

fn default_net_mb() -> u64 {
    512
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
        "persona",
        "approval_timeout_secs",
        "compact_timeout_secs",
        "titlegen",
        "titlegen_python",
        "titlegen_script",
        "question_timeout_secs",
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
        Self::set_keys_in(text, Some("search"), kvs)
    }

    /// Edit `[context]` keys in a config file's text. Same rules as
    /// `set_search_keys_in`: every other line, comments included, stays as it was.
    pub fn set_context_keys_in(text: &str, kvs: &[(&str, &str)]) -> (String, Vec<String>) {
        Self::set_keys_in(text, Some("context"), kvs)
    }

    /// Remove keys from one region of a config file's text.
    ///
    /// "Not set" is a state no number can express. For `context.target_tokens` it means
    /// *derive the ceiling from what the endpoint states about its window*, and the only
    /// way back to it is to delete the line — writing some sentinel number would pin a
    /// value while claiming to free the setting. A commented-out key is left alone and
    /// reported: an example shipped in the config file is documentation, not a setting,
    /// and deleting documentation because somebody pressed "derive from the model" would
    /// be a quiet loss of somebody's own notes.
    pub fn unset_keys_in(text: &str, table: Option<&str>, keys: &[&str]) -> (String, Vec<String>) {
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        let mut changed = Vec::new();
        let is_header = |l: &str| match table {
            Some(name) => l.trim() == format!("[{name}]"),
            None => l.trim().starts_with('[') && l.trim().ends_with(']'),
        };
        let h = match table {
            Some(_) => lines.iter().position(|l| is_header(l)),
            None => Some(0),
        };
        let Some(h) = h else { return (text.to_string(), changed) };
        // The region runs to the next table header, so a same-named key in another
        // section cannot be removed by accident. At the top level the region ends at the
        // first header, and line 0 is a key row, not a header.
        let mut end = (h + 1..lines.len())
            .find(|i| {
                let t = lines[*i].trim();
                t.starts_with('[') && t.ends_with(']')
            })
            .unwrap_or(lines.len());
        let body = if table.is_some() { h + 1 } else { 0 };
        for k in keys {
            let at = (body..end).find(|i| {
                let t = lines[*i].trim();
                t.starts_with(&format!("{k} ")) || t.starts_with(&format!("{k}="))
            });
            match at {
                Some(i) => {
                    let old = lines[i].trim().to_string();
                    lines.remove(i);
                    // The region just shrank, so `end` has to be recomputed. It was found
                    // against the old line list; clearing a second key in the same call then
                    // searched past the end of the file and panicked. The settings screen is
                    // the first caller that clears several keys at once — a model row whose
                    // fields go back to "ask the endpoint" is exactly that.
                    end = (body..lines.len())
                        .find(|j| {
                            let t = lines[*j].trim();
                            t.starts_with('[') && t.ends_with(']')
                        })
                        .unwrap_or(lines.len());
                    changed.push(format!("removed {old}"));
                }
                None => {
                    let commented = (body..end).find(|i| {
                        let t = lines[*i].trim().trim_start_matches('#').trim();
                        t.starts_with(&format!("{k} ")) || t.starts_with(&format!("{k}="))
                    });
                    match commented {
                        Some(i) => changed.push(format!(
                            "{} was already commented out: {}",
                            k,
                            lines[i].trim()
                        )),
                        // Nothing to remove. The caller says "back to derived", and a
                        // file that never pinned the value was already there.
                        None => changed.push(format!("{k} was not set")),
                    }
                }
            }
        }
        let mut out = lines.join("\n");
        if text.ends_with('\n') || !out.ends_with('\n') {
            out.push('\n');
        }
        (out, changed)
    }

    /// Edit keys in one region of a TOML file.
    ///
    /// `table` is a section name without brackets; `None` is the **top level**,
    /// which in TOML means "before the first `[header]`". That distinction is not
    /// cosmetic: a bare key written after `[provider]` parses as `provider.mode`
    /// and does nothing at all, which is how a saved setting appears to be saved
    /// and then does not apply.
    pub fn set_keys_in(
        text: &str,
        table: Option<&str>,
        kvs: &[(&str, &str)],
    ) -> (String, Vec<String>) {
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        let is_header = |l: &str| {
            let t = l.trim();
            match table {
                Some(name) => t == format!("[{name}]"),
                None => t.starts_with('[') && t.ends_with(']'),
            }
        };
        let mut changed = Vec::new();

        // (start, end) of the region the keys belong to, in line indices.
        let region = match table {
            Some(name) => {
                let h = lines.iter().position(|l| is_header(l));
                let Some(h) = h else {
                    if !text.is_empty() && !text.ends_with('\n') {
                        lines.push(String::new());
                    }
                    lines.push(format!("[{name}]"));
                    for (k, v) in kvs {
                        lines.push(format!("{k} = {v}"));
                        changed.push(format!("{k} = {v} (new)"));
                    }
                    let mut out = lines.join("\n");
                    out.push('\n');
                    return (out, changed);
                };
                // The section ends at the next header, so a `[sandbox] backend = ...` (no
                // such key today, but any future collision) cannot be edited by accident.
                let end = (h + 1..lines.len())
                    .find(|i| {
                        let t = lines[*i].trim();
                        t.starts_with('[') && t.ends_with(']')
                    })
                    .unwrap_or(lines.len());
                (h, end)
            }
            // Top level: everything before the first table header.
            None => {
                let end = lines.iter().position(|l| is_header(l)).unwrap_or(lines.len());
                (0, end)
            }
        };
        let (h, end) = region;
        // Inside a table, line `h` is the `[header]` itself and is not a key row. At
        // the top level `h` is 0 and line 0 is a key row - and a file whose very first
        // line is `mode = "ask"` used to have that line skipped, so the writer added a
        // second `mode` and the file then held one key twice.
        let body = if table.is_some() { h + 1 } else { h };
        for (k, v) in kvs {
            // A commented-out key counts: `# searxng_url = "..."` is the example in
            // the shipped config, and adding a second line would leave two, one of
            // them a lie.
            let at = (body..end).find(|i| {
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
                    while insert > body && lines[insert - 1].trim().is_empty() {
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
        Self::set_keys(path, Some("search"), kvs)
    }

    /// Write `[context]` keys to `path`.
    pub fn set_context_keys(
        path: &std::path::Path,
        kvs: &[(&str, &str)],
    ) -> Result<Vec<String>, String> {
        Self::set_keys(path, Some("context"), kvs)
    }

    /// Remove keys from one table of `path`, with the same care as `set_keys`: the
    /// edited text is parsed back before the file is replaced, and the replacement is a
    /// rename. A settings control that can free a setting has to be as careful as one
    /// that sets it — the failure mode of a bad removal is a config the next boot
    /// cannot open.
    pub fn unset_keys(
        path: &std::path::Path,
        table: Option<&str>,
        keys: &[&str],
    ) -> Result<Vec<String>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let (out, changed) = Self::unset_keys_in(&text, table, keys);
        if let Err(e) = toml::from_str::<Config>(&out) {
            return Err(format!("the edited file would not parse, so nothing was written: {e}"));
        }
        if changed.is_empty() {
            return Ok(changed);
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, out.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
        Ok(changed)
    }

    /// Remove keys from `[context]` in `path`.
    pub fn unset_context_keys(path: &std::path::Path, keys: &[&str]) -> Result<Vec<String>, String> {
        Self::unset_keys(path, Some("context"), keys)
    }

    /// Write **top-level** keys to `path`.
    ///
    /// Legal top-level keys are the ones declared on `Config` itself: `workspace`,
    /// `mode`, `reasoning`, `theme`, `max_tool_iters`, `approval_timeout_secs`,
    /// `question_timeout_secs`, `max_output_tokens`. `model` is **not** one of them:
    /// it belongs to `[provider]`, and a second one at the top level is a file the
    /// next start cannot read.
    ///
    /// The reason these are written at all: a choice made in the UI used to live only
    /// in the running process, so the next start read `mode = "ask"` from the file and
    /// the app was doing something the user had explicitly turned off. Two clients of
    /// one engine (the window and the gateway on a phone) then disagreed with each
    /// other as well, because whichever one started last re-read the file. A toggle
    /// you can see is a toggle that survives a restart.
    pub fn set_top_keys(path: &std::path::Path, kvs: &[(&str, &str)]) -> Result<Vec<String>, String> {
        Self::set_keys(path, None, kvs)
    }

    /// Write a key inside `[provider]` - today that means `model`.
    pub fn set_provider_keys(
        path: &std::path::Path,
        kvs: &[(&str, &str)],
    ) -> Result<Vec<String>, String> {
        Self::set_keys(path, Some("provider"), kvs)
    }

    fn set_keys(
        path: &std::path::Path,
        table: Option<&str>,
        kvs: &[(&str, &str)],
    ) -> Result<Vec<String>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let (out, changed) = Self::set_keys_in(&text, table, kvs);
        // Read the edited file back before replacing it. The parser rejects unknown
        // keys on purpose, so a writer that invents a key produces a config the
        // *next start* cannot open - and that failure surfaces hours later, in a
        // different process, with nothing pointing at the click that caused it. This
        // app did exactly that: `reasoning` and `model` were written at the top level,
        // where no such field is declared, and the app stopped booting. Verifying here
        // turns it into a refused write and one log line.
        if let Err(e) = toml::from_str::<Config>(&out) {
            return Err(format!(
                "the edited file would not parse, so nothing was written: {e}"
            ));
        }
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

    /// `full` unless someone wrote otherwise. Anything unrecognised falls back to `full`
    /// rather than to lean: a typo in a config file should not silently hide most of the
    /// app's capabilities, which is a change in behaviour, whereas declaring the full set is
    /// what every version of this app has always done.
    pub fn tool_exposure(&self) -> String {
        self.tool_exposure
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| "full".to_string())
    }

    pub fn lean_tools(&self) -> bool {
        self.tool_exposure() == "lean"
    }

    /// A sentence for a status panel or a gate: what is declared, and what the word in the
    /// config file actually was when it was not one of the two we know.
    pub fn tool_exposure_note(&self) -> String {
        match self.tool_exposure() {
            ref w if w == "lean" => "lean: the model is shown only the always-declared tools and reaches the rest through `explore`".to_string(),
            ref w if w == "full" => "full: every mounted tool is declared to the model".to_string(),
            other => format!("full (the config said {other:?}, which is not a tool exposure mode, so the default stands)"),
        }
    }

    pub fn question_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.question_timeout_secs.unwrap_or(180))
    }

    /// Accept either a path or a bare program name. Someone writing `titlegen_python =
    /// "python3"` means the one on PATH, and a check that only looks for a file at the
    /// literal string reports "no python" while a perfectly good python sits right there.
    fn resolve_exec(p: &std::path::Path) -> std::path::PathBuf {
        if p.components().count() == 1 {
            if let Ok(path) = std::env::var("PATH") {
                for dir in std::env::split_paths(&path) {
                    let cand = dir.join(p);
                    if cand.is_file() {
                        return cand;
                    }
                }
            }
        }
        p.to_path_buf()
    }

    /// Assemble the title-helper settings: config file first, then the environment, so a
    /// gate can point the feature at a scripted helper without editing anyone's config.
    pub fn titlegen_cfg(&self) -> crate::titlegen::TitleGenCfg {
        let mut cfg = crate::titlegen::TitleGenCfg::default();
        let s = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        // Held as owned strings first: the environment values are temporaries, and a
        // reference into one of them would not outlive the call.
        let env_python = s("CWOFFEE_TITLEGEN_PYTHON");
        let env_script = s("CWOFFEE_TITLEGEN_SCRIPT");
        let env_model = s("CWOFFEE_TITLEGEN_MODEL");
        let env_cache = s("CWOFFEE_TITLEGEN_CACHE");
        let env_threads = s("CWOFFEE_TITLEGEN_THREADS");
        let env_timeout = s("CWOFFEE_TITLEGEN_TIMEOUT_SECS");
        let env_flag = s("CWOFFEE_TITLEGEN");
        cfg.enabled = self.titlegen.unwrap_or(false);
        cfg.python = self
            .titlegen_python
            .as_deref()
            .or_else(|| env_python.as_deref())
            .map(PathBuf::from)
            .map(|p| Self::resolve_exec(&p))
            .unwrap_or(cfg.python);
        cfg.script = self
            .titlegen_script
            .as_deref()
            .or_else(|| env_script.as_deref())
            .map(PathBuf::from)
            .map(|p| Self::resolve_exec(&p))
            .unwrap_or(cfg.script);
        cfg.model = self
            .titlegen_model
            .as_deref()
            .or_else(|| env_model.as_deref())
            .map(str::to_string)
            .unwrap_or(cfg.model);
        cfg.cache = self
            .titlegen_cache
            .as_deref()
            .or_else(|| env_cache.as_deref())
            .map(PathBuf::from)
            .unwrap_or(cfg.cache);
        cfg.threads = self
            .titlegen_threads
            .or_else(|| env_threads.as_deref().and_then(|v| v.parse().ok()))
            .unwrap_or(cfg.threads)
            .clamp(1, 16);
        cfg.timeout = std::time::Duration::from_secs(
            self.titlegen_timeout_secs
                .or_else(|| env_timeout.as_deref().and_then(|v| v.parse().ok()))
                .unwrap_or(45),
        );
        if matches!(env_flag.as_deref(), Some("1") | Some("true") | Some("yes")) {
            cfg.enabled = true;
        }
        if matches!(env_flag.as_deref(), Some("0") | Some("false") | Some("no")) {
            cfg.enabled = false;
        }
        cfg
    }

    /// The budget for one manual compaction. Deliberately generous, and deliberately a
    /// knob: a ~200k-token fold was measured taking well past five minutes on this box,
    /// and the only thing a tight ceiling accomplishes is a false report of failure.
    pub fn compact_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.compact_timeout_secs.unwrap_or(1800))
    }

    /// Validate the question timeout. A tiny value makes `ask_user` useless (the card
    /// closes before it can be read), and an enormous one can park a turn for an hour
    /// on a phone that is in a pocket. Both are typos, so both name the key.
    pub fn question_timeout_checked(&self) -> Result<std::time::Duration, String> {
        match self.question_timeout_secs {
            None => Ok(std::time::Duration::from_secs(180)),
            Some(secs) => {
                if !(15..=3600).contains(&secs) {
                    return Err(format!(
                        "question_timeout_secs = {secs} is outside 15..=3600 \
                         (180 is the default)"
                    ));
                }
                Ok(std::time::Duration::from_secs(secs))
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

    /// The thinking level written in the file, or `None` when the file says nothing
    /// and the shell's own default applies.
    ///
    /// Checked rather than defaulted quietly: `reasoning = "thinking"` is a typo, and
    /// an ignored typo here means the model runs at a level nobody chose - the same
    /// class of problem as a sampling value that gets bent instead of refused.
    pub fn reasoning_checked(&self) -> Result<Option<crate::provider::Reasoning>, String> {
        use crate::provider::Reasoning;
        let Some(word) = self.reasoning.as_deref() else {
            return Ok(None);
        };
        match word {
            "off" => Ok(Some(Reasoning::Off)),
            "low" => Ok(Some(Reasoning::Low)),
            "medium" => Ok(Some(Reasoning::Medium)),
            "high" => Ok(Some(Reasoning::High)),
            "xhigh" => Ok(Some(Reasoning::XHigh)),
            other => Err(format!(
                "reasoning = {other:?} is not a thinking level; expected one of: off, low, medium, high, xhigh"
            )),
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
            net: crate::tools::net::NetCfg {
                mode: crate::tools::net::NetMode::parse(&self.sandbox.network)
                    .unwrap_or_default(),
                allow: self
                    .sandbox
                    .allow
                    .clone()
                    .unwrap_or_else(crate::tools::net::default_allow),
                max_bytes_per_call: self.sandbox.max_mb_per_call.saturating_mul(1 << 20),
            },
        }
    }

    /// Validate the egress block before anything runs. A mode word that does not
    /// parse would otherwise fall back to the default silently, which is the worst
    /// possible outcome for a security-shaped setting: the user writes `off`, gets
    /// `proxy`, and sees no reason to doubt it.
    pub fn net_checked(&self) -> Result<(), String> {
        let mode = crate::tools::net::NetMode::parse(&self.sandbox.network)
            .ok_or_else(|| format!("[sandbox] network = {:?} is not one of off, proxy, open", self.sandbox.network))?;
        if let Some(list) = &self.sandbox.allow {
            for h in list {
                let h = h.trim();
                if h.is_empty() {
                    return Err("[sandbox] allow contains an empty entry".into());
                }
                if h.contains("://") || h.contains('/') || h.contains(' ') || h.contains(':') {
                    return Err(format!(
                        "[sandbox] allow entry {h:?} is not a hostname: list hosts (example.com)                          or wildcard subdomains (*.example.com), not URLs or host:port"
                    ));
                }
            }
        }
        if self.sandbox.max_mb_per_call == 0 {
            return Err("[sandbox] max_mb_per_call = 0 would stop every download; use \
                        [sandbox] network = \"off\" to block the network instead".into());
        }
        let _ = mode;
        Ok(())
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

// ----------------------------------------------------------------------------------------
// Providers and models, as rows rather than as one slot
// ----------------------------------------------------------------------------------------

/// One endpoint the user has registered. The single `ProviderCfg` in `Config` is the *chosen*
/// provider; this is the list a settings screen needs, and the reason a second endpoint no
/// longer replaces the first.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ProviderRow {
    /// `openai`, `anthropic`, `llama.cpp` — the wire dialect, not a brand.
    pub kind: String,
    pub base_url: String,
    /// Empty means "this endpoint takes no key", which is a decision and not a missing value.
    #[serde(default)]
    pub api_key: String,
    /// Model ids registered under this provider. A probe that adds an endpoint fills the list,
    /// so "add the whole endpoint and everything on it" is one action.
    #[serde(default)]
    pub models: Vec<String>,
}

/// What the user — or a probe — says about one model. Every field is optional: left out means
/// "ask the endpoint", present means "this box, the phone and the next restart agree".
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ModelRow {
    /// The window this app treats the model as having. A probe prefills it; overriding it is
    /// the point, because a server's `n_ctx` changes without telling the client, and a meter
    /// built on a stale number is a wrong number.
    #[serde(default)]
    pub context_window: Option<usize>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub min_p: Option<f32>,
    #[serde(default)]
    pub seed: Option<u64>,
    /// The thinking levels this endpoint accepts, in the order it accepts them.
    #[serde(default)]
    pub efforts: Vec<String>,
    /// Capability as observed: `Some(true)` probed yes, `Some(false)` probed and refused,
    /// `None` never asked. The last two are different claims and the UI says them differently.
    #[serde(default)]
    pub supports_tools: Option<bool>,
    #[serde(default)]
    pub supports_vision: Option<bool>,
    /// Where the numbers came from: "probed from http://...", "typed by you", "the endpoint's
    /// /props". A settings screen that cannot say that is why people go editing the file.
    #[serde(default)]
    pub source: String,
}

/// A TOML basic string, quoted and escaped by hand.
///
/// `toml::to_string` on a bare string serialises a *document*, and a document cannot be one
/// string: it returns an empty literal, which writes `base_url = ` and breaks the file. The
/// settings writer takes verbatim value text, so quoting belongs here.
fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The TOML header a provider id lives under. Ids contain dots and slashes, which TOML reads
/// as key-path separators unless the key is quoted — `[providers.glm-5.1]` is a three-level
/// path, not one endpoint.
pub fn provider_header(name: &str) -> String {
    let bare = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        format!("providers.{name}")
    } else {
        format!("providers.{}", q(name))
    }
}

pub fn model_header(id: &str) -> String {
    format!("models.{}", q(id))
}

/// `[providers.<name>]` rows, sorted. Reading parses; writing never does, so the file's
/// comments, ordering and hand-written sections survive every settings write.
pub fn provider_rows_in(path: &std::path::Path) -> Vec<(String, ProviderRow)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let v: toml::Value = match text.parse() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(tbl) = v.get("providers").and_then(|x| x.as_table()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, val) in tbl {
        if let Ok(row) = val.clone().try_into() {
            out.push((name.clone(), row))
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn model_rows_in(path: &std::path::Path) -> Vec<(String, ModelRow)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let v: toml::Value = match text.parse() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(tbl) = v.get("models").and_then(|x| x.as_table()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (id, val) in tbl {
        if let Ok(row) = val.clone().try_into() {
            out.push((id.clone(), row))
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Write one provider row with the same line surgery the rest of the settings use: the file
/// stays hand-editable, and a commented example someone left near `[search]` is still there
/// after the model screen adds an endpoint.
pub fn set_provider_row_in(
    path: &std::path::Path,
    name: &str,
    row: &ProviderRow,
) -> Result<Vec<String>, String> {
    if name.trim().is_empty() {
        return Err("a provider needs a name — it is what the model list is filed under".into())
    }
    if row.base_url.trim().is_empty() {
        return Err("a provider needs an API endpoint".into())
    }
    if !(row.base_url.starts_with("http://") || row.base_url.starts_with("https://")) {
        return Err(format!(
            "{name}: the endpoint has to start with http:// or https://; what was written was `{}`",
            row.base_url
        ))
    }
    if !matches!(row.kind.as_str(), "openai" | "anthropic" | "llama.cpp" | "local") {
        return Err(format!(
            "{name}: unknown dialect `{}`; the app speaks openai, anthropic and llama.cpp",
            row.kind
        ))
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("config dir: {e}"))?;
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let models: Vec<String> = row.models.iter().map(|m| q(m)).collect();
    let kvs: Vec<(String, String)> = vec![
        ("kind".into(), q(&row.kind)),
        ("base_url".into(), q(&row.base_url)),
        ("api_key".into(), q(&row.api_key)),
        ("models".into(), format!("[{}]", models.join(", "))),
    ];
    let refs: Vec<(&str, &str)> = kvs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let (next, changed) = Config::set_keys_in(&text, Some(&provider_header(name)), &refs);
    std::fs::write(path, next).map_err(|e| format!("config write failed: {e}"))?;
    Ok(changed)
}

/// The window the person stored for one model, if they stored one.
///
/// Read before the endpoint's own claim by both cores: the context meter, the ceiling derived
/// from it, and pi's fold threshold are all built from this answer. A llama.cpp server's
/// `n_ctx` gets changed without telling the client, and a meter built on the endpoint's claim
/// then keeps a confident wrong number.
pub fn window_override_in(path: &std::path::Path, model: &str) -> Option<usize> {
    model_rows_in(path)
        .into_iter()
        .find(|(id, _)| id == model)
        .and_then(|(_, r)| r.context_window)
}

/// Remove one provider's section, reporting whether anything was there.
pub fn forget_provider_in(path: &std::path::Path, name: &str) -> Result<bool, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };
    let (next, removed) = drop_region(&text, &provider_header(name));
    if removed {
        std::fs::write(path, next).map_err(|e| format!("config write failed: {e}"))?;
    }
    Ok(removed)
}

/// Write one model row. A field the caller clears is *removed*, never written as a sentinel:
/// no number means "ask the endpoint", and the way this file already says that is to leave the
/// key out — the context ceiling works the same way, and a stale `context_window` line left
/// behind after someone pressed "ask the endpoint" is a number the meter keeps believing.
pub fn set_model_row_in(
    path: &std::path::Path,
    id: &str,
    row: &ModelRow,
) -> Result<Vec<String>, String> {
    if id.trim().is_empty() {
        return Err("a model row needs an id (provider/model)".into())
    }
    if let Some(w) = row.context_window {
        if w < 1024 {
            return Err(format!(
                "{id}: a window of {w} tokens is below the 1024 even a small prompt needs; leave the field empty to ask the endpoint instead"
            ))
        }
    }
    if let Some(t) = row.temperature {
        if !(0.0..=2.0).contains(&t) {
            return Err(format!("{id}: temperature has to be between 0 and 2 (got {t})"))
        }
    }
    if let Some(pp) = row.top_p {
        if !(0.0..=1.0).contains(&pp) {
            return Err(format!("{id}: top_p has to be between 0 and 1 (got {pp})"))
        }
    }
    if let (Some(w), Some(mx)) = (row.context_window, row.max_tokens) {
        if mx >= w {
            return Err(format!(
                "{id}: an output cap of {mx} tokens does not fit a window of {w}; the prompt needs room too"
            ));
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("config dir: {e}"))?;
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let header = model_header(id);
    // The keys this row owns. A field the caller left empty is cleared rather than skipped, so
    // the row cannot keep a fact the person just tried to remove.
    let mut set: Vec<(String, String)> = Vec::new();
    let mut clear: Vec<&str> = Vec::new();
    // A nested fn rather than a closure: a closure that captures both vectors holds them
    // mutably for its whole lifetime, and the calls below then conflict with each other.
    fn push_num(
        set: &mut Vec<(String, String)>,
        clear: &mut Vec<&'static str>,
        key: &'static str,
        v: Option<usize>,
    ) {
        match v {
            Some(x) => set.push((key.into(), x.to_string())),
            None => clear.push(key),
        }
    }
    push_num(&mut set, &mut clear, "context_window", row.context_window);
    push_num(&mut set, &mut clear, "max_tokens", row.max_tokens);
    match row.temperature {
        Some(v) => set.push(("temperature".into(), format!("{v}"))),
        None => clear.push("temperature"),
    }
    match row.top_p {
        Some(v) => set.push(("top_p".into(), format!("{v}"))),
        None => clear.push("top_p"),
    }
    push_num(&mut set, &mut clear, "top_k", row.top_k);
    match row.min_p {
        Some(v) => set.push(("min_p".into(), format!("{v}"))),
        None => clear.push("min_p"),
    }
    match row.seed {
        Some(v) => set.push(("seed".into(), v.to_string())),
        None => clear.push("seed"),
    }
    if row.efforts.is_empty() {
        clear.push("efforts")
    } else {
        let list: Vec<String> = row.efforts.iter().map(|e| q(e)).collect();
        set.push(("efforts".into(), format!("[{}]", list.join(", "))))
    }
    match row.supports_tools {
        Some(v) => set.push(("supports_tools".into(), v.to_string())),
        None => clear.push("supports_tools"),
    }
    match row.supports_vision {
        Some(v) => set.push(("supports_vision".into(), v.to_string())),
        None => clear.push("supports_vision"),
    }
    if row.source.is_empty() {
        clear.push("source")
    } else {
        set.push(("source".into(), q(&row.source)))
    }
    let set_refs: Vec<(&str, &str)> = set.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let (next, mut changed) = Config::set_keys_in(&text, Some(&header), &set_refs);
    let (next, removed) = Config::unset_keys_in(&next, Some(&header), &clear);
    changed.extend(removed.into_iter().map(|k| format!("{k} removed")));
    std::fs::write(path, next).map_err(|e| format!("config write failed: {e}"))?;
    Ok(changed)
}

pub fn forget_model_in(path: &std::path::Path, id: &str) -> Result<bool, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };
    let (next, removed) = drop_region(&text, &model_header(id));
    if removed {
        std::fs::write(path, next).map_err(|e| format!("config write failed: {e}"))?;
    }
    Ok(removed)
}

/// What the settings screen sends for an endpoint. `name` empty means "call it after the
/// host", which is what people expect when they paste a URL and press Add.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProviderReq {
    #[serde(default)]
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub kind: String,
}

impl ProviderReq {
    /// The dialect to use. Inferred from the URL when not stated, the same way the single
    /// `[provider]` section infers it — a localhost endpoint with no kind said is a local
    /// server, and making the user spell that out is how people end up picking `openai` for a
    /// llama.cpp box and losing tool calling.
    pub fn kind_or_inferred(&self) -> String {
        let k = self.kind.trim().to_ascii_lowercase();
        if matches!(k.as_str(), "openai" | "anthropic" | "llama.cpp" | "local") {
            return k
        }
        let u = self.base_url.to_ascii_lowercase();
        if u.contains("anthropic") || u.contains(":15721") {
            "anthropic".to_string()
        } else if u.contains("localhost") || u.starts_with("http://127.") || u.contains(":8080") || u.contains("/completion") {
            "llama.cpp".to_string()
        } else {
            "openai".to_string()
        }
    }

    /// A name for the endpoint when the user did not type one: the host, port and path, which
    /// is what distinguishes two endpoints on the same machine.
    pub fn name_or_inferred(&self) -> String {
        let n = self.name.trim();
        if !n.is_empty() {
            return n.to_string()
        }
        let rest = self
            .base_url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&self.base_url);
        rest.trim_end_matches('/').replace(['/', ':'], "-")
    }

    pub fn to_row(&self) -> ProviderRow {
        ProviderRow {
            kind: self.kind_or_inferred(),
            base_url: self.base_url.trim().to_string(),
            api_key: self.api_key.clone(),
            models: Vec::new(),
        }
    }

    fn cfg(&self) -> ProviderCfg {
        ProviderCfg {
            kind: self.kind_or_inferred(),
            base_url: self.base_url.trim().to_string(),
            model: String::new(),
            api_key: if self.api_key.trim().is_empty() {
                None
            } else {
                Some(self.api_key.clone())
            },
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderView {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    /// Whether a key is stored, never the key itself. A settings screen that echoes a secret
    /// into a text input puts it on every screenshot and every phone screen-share.
    pub has_key: bool,
    pub models: Vec<String>,
    pub current: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelView {
    pub id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// A row that exists in `[models."<id>"]` at all means the user or a probe wrote something
    /// about this model. The screen says which fields are decisions and which are absent.
    pub configured: bool,
    pub current: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderStatus {
    pub chosen: String,
    pub chosen_provider: String,
    /// Where the chosen model answers from: the URL of the endpoint row it is filed under, or
    /// the top-level `[provider]` section when that is what the app was started against. It
    /// belongs on the endpoint picture rather than on a model row: a model row's own `server`
    /// field names the *serving implementation* ("cwoffee-mock 1.0", a llama.cpp build), and a
    /// core that cannot know that must not fill the slot with an address. The settings screen
    /// needs an address — to prefill the form, to describe the endpoint it is about to write —
    /// so both cores answer from one place rather than each inventing one.
    pub chosen_base_url: String,
    pub providers: Vec<ProviderView>,
    pub models: Vec<ModelView>,
    /// `[sampling]` for the chosen model, so the screen can show what is in force without
    /// implying that the per-model row is the only source.
    pub sampling: crate::provider::Sampling,
}

/// A probed model, with the id the probe found it under.
///
/// The config row itself is keyed by id in the file, so `ModelRow` has no id field; a probe
/// result and an add request do need one, and `#[serde(flatten)]` keeps the wire shape flat for
/// the settings screen (id, context_window, supports_tools, ...) instead of nesting a row
/// inside a row.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ModelSpec {
    pub id: String,
    #[serde(flatten)]
    pub row: ModelRow,
}

/// The provider half of a `provider/model` id — which endpoint row a model belongs to.
pub fn provider_of_model(id: &str) -> String {
    match id.split_once('/') {
        Some((p, rest)) if !p.is_empty() && !rest.is_empty() => p.to_string(),
        _ => String::new(),
    }
}

/// The provider/model picture, built from the file alone.
pub fn provider_status_with(path: &std::path::Path, cfg: &Config) -> ProviderStatus {
    let chosen = cfg.provider.model.trim().to_string();
    let chosen_provider = provider_of_model(&chosen);
    let rows = provider_rows_in(path);
    // The address the chosen model answers from: its own endpoint row when it has one, the
    // top-level section otherwise. Not whichever row happens to share a name with something
    // else — the row has to be the one this model is filed under.
    let chosen_base_url = {
        let from_row = rows
            .iter()
            .find(|(n, _)| !chosen_provider.is_empty() && n == &chosen_provider)
            .map(|(_, r)| r.base_url.trim().to_string())
            .unwrap_or_default();
        if from_row.is_empty() {
            cfg.provider.base_url.trim().to_string()
        } else {
            from_row
        }
    };
    let mut models = model_rows_in(path);
    // A model with no `[models."id"]` row still belongs on the screen: it is the model the app
    // answers with, and a settings screen that only lists what someone has already configured
    // is a screen that cannot configure anything.
    if !chosen.is_empty() && !models.iter().any(|(id, _)| *id == chosen) {
        models.push((chosen.clone(), ModelRow::default()));
    }
    let mut views: Vec<ModelView> = models
        .iter()
        .map(|(id, r)| ModelView {
            id: id.clone(),
            provider: provider_of_model(id),
            context_window: r.context_window,
            max_tokens: r.max_tokens,
            temperature: r.temperature,
            top_p: r.top_p,
            top_k: r.top_k,
            min_p: r.min_p,
            seed: r.seed,
            efforts: r.efforts.clone(),
            supports_tools: r.supports_tools,
            supports_vision: r.supports_vision,
            source: r.source.clone(),
            configured: !r.clone().eq(&ModelRow::default()),
            current: *id == chosen,
        })
        .collect();
    views.sort_by(|a, b| a.id.cmp(&b.id));
    ProviderStatus {
        chosen,
        chosen_provider: chosen_provider.to_string(),
        chosen_base_url: chosen_base_url.clone(),
        providers: rows
            .iter()
            .map(|(name, r)| ProviderView {
                name: name.clone(),
                kind: r.kind.clone(),
                base_url: r.base_url.clone(),
                has_key: !r.api_key.trim().is_empty(),
                models: r.models.clone(),
                current: *name == chosen_provider,
            })
            .collect(),
        models: views,
        sampling: cfg.sampling_checked().unwrap_or_default(),
    }
}

/// Ask one endpoint for its model list and answer in a sentence. Used by "Test connection":
/// the point of the button is to find out now, with nothing written.
pub async fn probe_endpoint(req: &ProviderReq) -> String {
    // Build through `Config`, the one place that knows how to turn a provider section into a
    // client (flavor, key indirection, sampling). A second construction path here would be a
    // second policy about what an endpoint is, and it would drift.
    let cfg = Config {
        provider: req.cfg(),
        ..Default::default()
    };
    match cfg.build_provider() {
        Err(e) => return format!("could not use that endpoint: {e}"),
        Ok(p) => match p.list_models().await {
            Err(e) => format!("{}: {e}", req.base_url),
            Ok(list) => {
                let shown: Vec<&str> = list.iter().take(12).map(|s| s.as_str()).collect();
                let more = list.len().saturating_sub(shown.len());
                format!(
                    "{} answered with {} model(s){}: {}",
                    req.base_url,
                    list.len(),
                    if more > 0 { format!(", {more} more not listed") } else { String::new() },
                    shown.join(", ")
                )
            }
        },
    }
}

/// Ask one endpoint what each model can do. Probing is what turns "I typed a URL" into
/// "the app knows this model takes images and folds at 131072 tokens".
pub async fn probe_models(req: &ProviderReq) -> Result<Vec<ModelSpec>, String> {
    let cfg = Config {
        provider: req.cfg(),
        ..Default::default()
    };
    let p = cfg
        .build_provider()
        .map_err(|e| format!("could not use that endpoint: {e}"))?;
    let infos = p
        .list_models_info()
        .await
        .map_err(|e| format!("{}: {e}", req.base_url))?;
    Ok(infos
        .into_iter()
        .map(|m| ModelSpec {
            id: m.id.clone(),
            row: ModelRow {
            context_window: m.context_tokens.map(|x| x as usize),
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            min_p: None,
            seed: None,
            // `reasoning_efforts`: the probe already asked the server which levels it takes.
            efforts: m.reasoning_efforts.clone(),
            supports_tools: m.tools,
            // An empty modality list means the endpoint never said: "unknown" and "text only"
            // are different answers, and the screen has to be able to tell them apart.
            supports_vision: if m.input_modalities.is_empty() {
                None
            } else {
                Some(m.input_modalities.iter().any(|x| x == "image"))
            },
            source: format!("probed from {}", req.base_url),
            },
        })
        .collect())
}

/// Register an endpoint plus the models a probe found on it. One action, one sentence, and
/// the sentence names what a later restart will still know.
pub fn add_provider_with(path: &std::path::Path, req: &ProviderReq, caps: &[ModelSpec]) -> String {
    let name = req.name_or_inferred();
    let mut row = req.to_row();
    row.models = caps.iter().map(|c| c.id.trim().to_string()).filter(|m| !m.is_empty()).collect();
    if row.models.is_empty() {
        // A probe is what names models. Adding an endpoint with nothing on it would create a
        // provider row the screen shows as empty forever, which is a lie about the endpoint.
        return format!(
            "{name}: nothing was probed, so nothing was added — press Probe models first; if the endpoint will not answer a probe, that is the thing to fix"
        );
    }
    if let Err(e) = set_provider_row_in(path, &name, &row) {
        return format!("not added: {e}");
    }
    let mut wrote = 0usize;
    for c in &row.models {
        let seed = caps
            .iter()
            .find(|x| &x.id == c)
            .map(|x| x.row.clone())
            .unwrap_or_default();
        if let Err(e) = set_model_row_in(path, c, &seed) {
            return format!("added {name} with {wrote} of {} model(s), then stopped: {e}", row.models.len());
        }
        wrote += 1;
    }
    format!(
        "{name} added at {u} with {wrote} model(s); each one's window and capabilities are saved, so the context ceiling and the fold follow them from the next start",
        u = row.base_url
    )
}

pub fn remove_provider_with(path: &std::path::Path, name: &str) -> String {
    match forget_provider_in(path, name) {
        Err(e) => format!("could not remove {name}: {e}"),
        Ok(false) => format!("no provider named {name} was registered — nothing changed"),
        Ok(true) => format!(
            "{name} removed from the endpoint list. Its models' rows stay, so the ceiling and sampling they describe are untouched; remove those under the model itself if you meant more than the endpoint."
        ),
    }
}

/// Write one model row and say, in words, what changes when.
pub fn set_model_with(path: &std::path::Path, id: &str, row: &ModelRow) -> String {
    let mut row = row.clone();
    // Half of a model row is what the settings screen edits and half is what a probe found.
    // The edited half follows the screen: a field left empty is a decision to stop pinning it,
    // so it is dropped. The probed half does not appear as a field at all, so a request that
    // says nothing about it must not erase it — pressing Save next to "Max context" has no
    // business un-probing whether the endpoint takes tool calls. The same reasoning covers
    // `source`, which says where the numbers came from: provenance belongs to the host that
    // asked the endpoint, not to whatever client is holding the form.
    if let Some((_, stored)) = model_rows_in(path).into_iter().find(|(k, _)| k == id) {
        if row.supports_tools.is_none() {
            row.supports_tools = stored.supports_tools;
        }
        if row.supports_vision.is_none() {
            row.supports_vision = stored.supports_vision;
        }
        if row.source.trim().is_empty() {
            row.source = stored.source;
        }
    }
    let bits = describe_model_write(id, &row);
    match set_model_row_in(path, id, &row) {
        Err(e) => format!("not saved: {e}"),
        Ok(changed) => format!("{} ({})", bits.join("; "), changed.len()),
    }
}

pub fn forget_model_with(path: &std::path::Path, id: &str) -> String {
    match forget_model_in(path, id) {
        Err(e) => format!("could not forget {id}: {e}"),
        Ok(false) => format!("nothing was configured for {id} — nothing changed"),
        Ok(true) => format!(
            "{id}: every stored fact about it is gone; the window, the ceiling and the fold go back to being derived from what the endpoint states"
        ),
    }
}

/// The sentence a model write produces: which fields are now decisions, which went back to
/// being derived, and which of them a running model process will not notice until it restarts.
fn describe_model_write(id: &str, row: &ModelRow) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(w) = row.context_window {
        out.push(format!("{id} window set to {w} tokens (the meter and the fold follow it)"));
    } else {
        out.push(format!("{id} window freed: it comes from the endpoint again"));
    }
    let mut sam = Vec::new();
    if let Some(t) = row.temperature {
        sam.push(format!("temperature {t}"))
    }
    if let Some(p) = row.top_p {
        sam.push(format!("top_p {p}"))
    }
    if let Some(k) = row.top_k {
        sam.push(format!("top_k {k}"))
    }
    if let Some(m) = row.min_p {
        sam.push(format!("min_p {m}"))
    }
    if let Some(s) = row.seed {
        sam.push(format!("seed {s}"))
    }
    if !sam.is_empty() {
        out.push(format!("sampling: {}", sam.join(", ")));
    }
    if !row.efforts.is_empty() {
        out.push(format!("thinking levels: {}", row.efforts.join("/")));
    }
    out.push("a model process already running keeps what it started with; the next one reads this".into());
    out
}

/// Delete one `[header]` region: the header line and its rows up to the next header.
///
/// Comment lines inside the region go with it, because they belong to the section being
/// removed; comments before the header belong to the previous section and are untouched.
fn drop_region(text: &str, header: &str) -> (String, bool) {
    let want = format!("[{header}]");
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let Some(h) = lines.iter().position(|l| l.trim() == want) else {
        return (text.to_string(), false);
    };
    let end = (h + 1..lines.len())
        .find(|i| {
            let t = lines[*i].trim();
            t.starts_with('[') && t.ends_with(']')
        })
        .unwrap_or(lines.len());
    lines.drain(h..end);
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    (out, true)
}

#[cfg(test)]
mod tests {
    /// The settings screen asks "what do you know about endpoints and models?" without going
    /// on the network, and then adds a provider from a probe it already ran. Both halves have
    /// to answer in sentences a person can read, and the second has to leave the file in a
    /// state the next boot reads back.
    #[test]
    fn a_provider_added_from_a_probe_comes_back_with_its_models_attached() {
        let dir = std::env::temp_dir().join(format!("cw-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "mode = \"ask\"\n\n[provider]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"local/qwen\"\n").unwrap();

        let cfg = Config::load(Some(&path)).unwrap().0;
        let st = provider_status_with(&path, &cfg);
        assert_eq!(st.chosen, "local/qwen", "{st:?}");
        assert_eq!(st.chosen_provider, "local", "{st:?}");
        // The model the app answers with appears even though nobody wrote a `[models]` row for
        // it: the screen needs something to configure.
        assert!(st.models.iter().any(|m| m.id == "local/qwen"), "{st:?}");
        assert!(st.providers.is_empty(), "no endpoints were registered yet: {st:?}");

        let req = ProviderReq {
            name: "box".into(),
            base_url: "http://192.168.0.122:8080/v1".into(),
            api_key: String::new(),
            kind: String::new(), // inferred: a :8080 host is a llama.cpp box
        };
        let caps = vec![
            ModelSpec {
                id: "box/qwen3.6-27b".into(),
                row: ModelRow {
                    context_window: Some(131_072),
                    supports_tools: Some(true),
                    supports_vision: Some(false),
                    efforts: vec!["minimal".into(), "high".into()],
                    source: "probed from http://192.168.0.122:8080/v1".into(),
                    ..Default::default()
                },
            },
            ModelSpec {
                id: "box/qwen3-4b".into(),
                row: ModelRow {
                    context_window: Some(131_072),
                    supports_tools: Some(false),
                    ..Default::default()
                },
            },
        ];
        let said = add_provider_with(&path, &req, &caps);
        assert!(said.contains("box added"), "unhelpful: {said}");
        assert!(said.contains("2 model(s)"), "unhelpful: {said}");

        let cfg = Config::load(Some(&path)).unwrap().0;
        let st = provider_status_with(&path, &cfg);
        assert_eq!(st.providers.len(), 1, "{st:?}");
        let p0 = &st.providers[0];
        assert_eq!(p0.name, "box");
        assert_eq!(p0.kind, "llama.cpp", "kind was not inferred from the URL: {p0:?}");
        assert_eq!(p0.models.len(), 2, "{p0:?}");
        assert!(!p0.has_key, "an endpoint with no key must not claim one");
        let big = st.models.iter().find(|m| m.id == "box/qwen3.6-27b").expect("probed row");
        assert_eq!(big.context_window, Some(131_072));
        assert_eq!(big.supports_vision, Some(false), "text-only must not read as unknown");
        assert_eq!(big.efforts.len(), 2);
        assert!(big.configured);
        let small = st.models.iter().find(|m| m.id == "box/qwen3-4b").expect("second row");
        assert_eq!(small.supports_vision, None, "never asked must not read as text-only");

        // Adding without a probe is refused in words rather than creating an empty provider.
        let said = add_provider_with(&path, &req, &[]);
        assert!(said.contains("nothing was probed"), "unhelpful: {said}");
        assert_eq!(provider_rows_in(&path).len(), 1, "an empty provider row was created anyway");

        // The chosen model's provider is named as current, so the screen can say which
        // endpoint is answering without the client guessing.
        let st = provider_status_with(&path, &cfg);
        assert!(st.providers.iter().any(|p| p.name == "box" && !p.current));
        let said = remove_provider_with(&path, "box");
        assert!(said.contains("removed"), "unhelpful: {said}");
        assert!(provider_rows_in(&path).is_empty());
        // The model rows survive an endpoint removal on purpose, and the sentence says so —
        // silently deleting a person's window and sampling decisions would be worse.
        assert!(!model_rows_in(&path).is_empty(), "removing an endpoint ate the model rows");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A probe against nothing has to say so, not return an empty list: "no models" and "the
    /// endpoint did not answer" are different claims, and the second one is the one the user
    /// needs to see.
    #[tokio::test]
    async fn probing_an_endpoint_that_is_not_there_names_the_endpoint() {
        let req = ProviderReq {
            name: String::new(),
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: String::new(),
            kind: "openai".into(),
        };
        let said = probe_endpoint(&req).await;
        assert!(said.contains("127.0.0.1:9"), "the sentence must name the endpoint: {said}");
        let err = probe_models(&req).await.expect_err("a dead endpoint is an error, not an empty list");
        assert!(err.contains("127.0.0.1:9"), "unhelpful: {err}");
    }


    /// A provider row written by the settings screen must land in the same hand-editable
    /// file the user reads, with their comments intact, and an id containing dots must stay
    /// one row rather than becoming a three-level path.
    #[test]
    fn an_endpoint_written_by_the_ui_keeps_the_file_readable() {
        let dir = std::env::temp_dir().join(format!("cw-prov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# my notes - do not lose me\nmode = \"ask\"\n\n# example: searxng_url = \"...\"\n[search]\nbackend = \"searxng\"\n",
        )
        .unwrap();

        let changed = set_provider_row_in(
            &path,
            "glm-5.1",
            &ProviderRow {
                kind: "openai".into(),
                base_url: "http://192.168.0.122:8080/v1".into(),
                api_key: "secret".into(),
                models: vec!["glm-5.1/glm-5.1".into()],
            },
        )
        .unwrap();
        assert!(!changed.is_empty(), "the writer should report what it wrote");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my notes - do not lose me"), "a settings write ate a comment: {text}");
        assert!(
            text.contains("# example: searxng_url"),
            "a settings write ate the commented example: {text}"
        );
        assert!(text.contains("[search]\nbackend = \"searxng\""), "the search section moved: {text}");
        // One row per id. `[providers.glm-5.1]` would be a path, so the key is quoted.
        assert!(
            text.contains("[providers.\"glm-5.1\"]"),
            "the dotted id was not quoted as one key: {text}"
        );

        let rows = provider_rows_in(&path);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, "glm-5.1");
        assert_eq!(rows[0].1.base_url, "http://192.168.0.122:8080/v1");
        assert_eq!(rows[0].1.models, vec!["glm-5.1/glm-5.1".to_string()]);

        // A second endpoint joins instead of replacing the first - the bug the single
        // `[provider]` slot makes inevitable today.
        set_provider_row_in(
            &path,
            "local",
            &ProviderRow {
                kind: "llama.cpp".into(),
                base_url: "http://127.0.0.1:8080".into(),
                api_key: String::new(),
                models: vec!["qwen3:4b".into()],
            },
        )
        .unwrap();
        let rows = provider_rows_in(&path);
        assert_eq!(rows.len(), 2, "a second endpoint replaced the first: {rows:?}");
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("mode = \"ask\""),
            "a provider write touched a key it does not own"
        );

        assert!(forget_provider_in(&path, "glm-5.1").unwrap(), "the row was there to remove");
        assert!(!forget_provider_in(&path, "glm-5.1").unwrap(), "removing it twice reports nothing removed");
        assert_eq!(provider_rows_in(&path).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A model row is a set of optional facts. Clearing one has to delete the key: a stale
    /// `context_window` line left behind after someone pressed "ask the endpoint" would be a
    /// number the meter keeps believing.
    #[test]
    fn the_endpoint_picture_names_the_address_the_chosen_model_answers_from() {
        // Two shapes the app can start in: an endpoint filed as a `[providers."x"]` row, and the
        // top-level `[provider]` section when nobody has registered anything. The settings screen
        // needs an address in both — it prefills a form with it and describes an endpoint with it
        // — and a model row's own `server` field cannot carry it, because that field names the
        // serving implementation ("cwoffee-mock 1.0"), which a pi core has no way to know.
        let dir = std::env::temp_dir().join(format!("cw-cbase-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[provider]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:8090/v1\"\nmodel = \"solo/model-one\"\n\n[providers.\"the box\"]\nkind = \"openai\"\nbase_url = \"http://192.168.0.9:8080/v1\"\nmodels = [\"the box/model-two\"]\n",
        )
        .unwrap();
        let (cfg, _) = Config::load(Some(&path)).unwrap();

        let st = provider_status_with(&path, &cfg);
        assert_eq!(
            st.chosen_base_url,
            "http://127.0.0.1:8090/v1",
            "an unregistered endpoint still has the address the app answers at: {st:?}"
        );

        let mut cfg2 = cfg.clone();
        cfg2.provider.model = "the box/model-two".into();
        let st2 = provider_status_with(&path, &cfg2);
        assert_eq!(
            st2.chosen_base_url,
            "http://192.168.0.9:8080/v1",
            "a model filed under a registered row answers at that row, not at the default: {st2:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_from_the_model_screen_keeps_the_facts_it_was_never_shown() {
        // The model row has two halves. One is fields the screen edits; the other is what a
        // probe found, and the screen never shows it as a field. Pressing Save beside
        // "Max context" must not un-probe a capability or forget where a number came from.
        let dir = std::env::temp_dir().join(format!("cw-msave-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);
        let req = ProviderReq {
            name: "box".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: String::new(),
            kind: "openai".into(),
        };
        let caps = vec![ModelSpec {
            id: "box/thing".into(),
            row: ModelRow {
                context_window: Some(32768),
                supports_tools: Some(true),
                supports_vision: Some(false),
                source: "probed from http://127.0.0.1:9/v1".into(),
                ..Default::default()
            },
        }];
        let added = add_provider_with(&path, &req, &caps);
        assert!(added.contains("box"), "{added}");

        // The screen's whole request: one number typed into one field.
        let row = ModelRow {
            context_window: Some(40000),
            ..Default::default()
        };
        let saved = set_model_with(&path, "box/thing", &row);
        assert!(saved.contains("40000"), "{saved}");
        let rows = model_rows_in(&path);
        let got = &rows
            .iter()
            .find(|(k, _)| k == "box/thing")
            .expect("the row is still filed")
            .1;
        assert_eq!(got.context_window, Some(40000), "the window that was typed did not land");
        assert_eq!(
            got.supports_tools,
            Some(true),
            "Save un-probed a capability the screen never offered as a field"
        );
        assert_eq!(
            got.supports_vision,
            Some(false),
            "a probed refusal is not the same as never having asked, and neither is `None`"
        );
        assert!(
            got.source.contains("probed from"),
            "provenance belongs to the side that asked the endpoint: {got:?}"
        );

        // An explicit answer still replaces: the merge covers only what the request said
        // nothing about.
        let row2 = ModelRow {
            context_window: Some(40000),
            supports_tools: Some(false),
            ..Default::default()
        };
        set_model_with(&path, "box/thing", &row2);
        let again = model_rows_in(&path);
        let got2 = &again
            .iter()
            .find(|(k, _)| k == "box/thing")
            .expect("still filed")
            .1;
        assert_eq!(
            got2.supports_tools,
            Some(false),
            "an answer written on purpose has to outrank the one a probe left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cleared_model_field_leaves_no_stale_number_behind() {
        let dir = std::env::temp_dir().join(format!("cw-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        set_model_row_in(
            &path,
            "qwen/qwen3.6-27b",
            &ModelRow {
                context_window: Some(202_752),
                temperature: Some(0.6),
                efforts: vec!["minimal".into(), "high".into()],
                supports_tools: Some(true),
                source: "probed from http://192.168.0.122:8080".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("context_window = 202752"), "{text}");
        assert!(text.contains("efforts = [\"minimal\", \"high\"]"), "{text}");

        let rows = model_rows_in(&path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.context_window, Some(202_752));
        assert_eq!(rows[0].1.temperature, Some(0.6));

        // Now clear the window and the temperature, keeping the rest.
        set_model_row_in(
            &path,
            "qwen/qwen3.6-27b",
            &ModelRow {
                efforts: vec!["minimal".into(), "high".into()],
                supports_tools: Some(true),
                source: "probed from http://192.168.0.122:8080".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("context_window"), "a cleared window left its number: {text}");
        assert!(!text.contains("temperature"), "a cleared temperature left its number: {text}");
        let rows = model_rows_in(&path);
        assert_eq!(rows[0].1.context_window, None);
        assert_eq!(rows[0].1.efforts, vec!["minimal".to_string(), "high".to_string()]);

        // Refusals, in words, with no write.
        let before = std::fs::read_to_string(&path).unwrap();
        let e = set_model_row_in(&path, "x/y", &ModelRow { context_window: Some(300), ..Default::default() })
            .expect_err("a 300-token window is not a setting");
        assert!(e.contains("1024"), "unhelpful: {e}");
        let e = set_model_row_in(&path, "x/y", &ModelRow { temperature: Some(7.0), ..Default::default() })
            .expect_err("temperature 7 is not a setting");
        assert!(e.contains("between 0 and 2"), "unhelpful: {e}");
        let e = set_model_row_in(
            &path,
            "x/y",
            &ModelRow { context_window: Some(4096), max_tokens: Some(9000), ..Default::default() },
        )
        .expect_err("an output cap larger than the window is not a setting");
        assert!(e.contains("does not fit"), "unhelpful: {e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "a refused write changed the file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An endpoint row is refused before anything is written: the file is the user's, and a
    /// half-written endpoint is a broken file.
    #[test]
    fn an_endpoint_is_refused_in_words_and_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("cw-badprov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "mode = \"ask\"\n").unwrap();
        let e = set_provider_row_in(
            &path,
            "x",
            &ProviderRow { kind: "openai".into(), base_url: "192.168.0.122:8080".into(), ..Default::default() },
        )
        .expect_err("a URL without a scheme is not an endpoint");
        assert!(e.contains("http://"), "unhelpful: {e}");
        let e = set_provider_row_in(
            &path,
            "x",
            &ProviderRow { kind: "mistral".into(), base_url: "http://x".into(), ..Default::default() },
        )
        .expect_err("an unknown dialect is not an endpoint");
        assert!(e.contains("openai"), "unhelpful: {e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mode = \"ask\"\n", "a refused endpoint wrote anyway");
        let _ = std::fs::remove_dir_all(&dir);
    }


    /// The default is what every previous version did, and a typo must not hide the app's
    /// capabilities: falling back to `full` is the only direction that cannot lose a feature
    /// because someone typed `Tool_Mode = "Lite"`.
    #[test]
    fn tool_exposure_defaults_to_full_and_only_says_lean_when_told_to() {
        let c = Config {
            tool_exposure: None,
            ..Config::default()
        };
        assert_eq!(c.tool_exposure(), "full");
        assert!(!c.lean_tools());
        let c = Config {
            tool_exposure: Some(" LEAN ".into()),
            ..Config::default()
        };
        assert!(c.lean_tools(), "case and stray spaces are not a behaviour change");
        let c = Config {
            tool_exposure: Some("lite".into()),
            ..Config::default()
        };
        assert!(!c.lean_tools(), "an unknown mode must not remove tools");
        assert!(
            c.tool_exposure_note().contains("not a tool exposure mode"),
            "and it has to say so: {}",
            c.tool_exposure_note()
        );
    }


    #[test]
    fn a_context_ceiling_can_be_pinned_and_then_freed_again() {
        // The shipped shape: a commented example, and the live keys around it.
        let text = "# target_tokens = 12000  <- uncomment to pin a ceiling\n\n[context]\nmax_tool_output = 24576\ntarget_tokens = 40000\n\n[search]\nbackend = \"searxng\"\n";
        let (out, ch) = Config::set_context_keys_in(text, &[("target_tokens", "60000")]);
        assert!(
            ch.iter().any(|c| c.contains("target_tokens: target_tokens = 40000 -> target_tokens = 60000")),
            "{ch:?}"
        );
        let back: Config = toml::from_str(&out).expect("the edited file parses");
        assert_eq!(back.context.target_tokens, Some(60_000), "the pin is the setting");
        assert_eq!(back.search.backend, "searxng", "the other sections are untouched");

        // And the way back to "derive it" is to remove the line, because no number
        // means "from the endpoint".
        let (out2, ch2) = Config::unset_keys_in(&out, Some("context"), &["target_tokens"]);
        let free: Config = toml::from_str(&out2).expect("the edited file parses");
        assert_eq!(free.context.target_tokens, None, "unset means derive it again");
        assert!(ch2.iter().any(|c| c.contains("removed target_tokens = 60000")), "{ch2:?}");
        assert!(
            out2.contains("max_tool_output = 24576") && out2.contains("backend = \"searxng\""),
            "removing one key must not take the section or its neighbours with it:\n{out2}"
        );
        assert!(
            out2.contains("# target_tokens = 12000"),
            "the commented example is documentation, not a setting: it stays"
        );
    }

    #[test]
    fn unsetting_a_key_that_is_somewhere_else_or_nowhere_says_so() {
        let text = "[context]\nmax_tool_output = 1\n\n[other]\ntarget_tokens = 7\n";
        let (out, ch) = Config::unset_keys_in(text, Some("context"), &["target_tokens"]);
        assert_eq!(ch, vec!["target_tokens was not set".to_string()], "{ch:?}");
        assert_eq!(out, text, "nothing to remove means nothing changes");
        assert!(
            out.contains("[other]\ntarget_tokens = 7"),
            "a same-named key in another table is not ours to touch:\n{out}"
        );
    }

    #[test]
    fn a_removed_key_survives_the_round_trip_on_disk() {
        let dir = std::env::temp_dir().join(format!("cw-unset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "mode = \"ask\"\n\n[context]\ntarget_tokens = 40000\nmax_tool_output = 2\n",
        )
        .unwrap();
        let ch = Config::unset_context_keys(&path, &["target_tokens"]).expect("removal ok");
        assert!(ch.iter().any(|c| c.contains("removed")), "{ch:?}");
        let (c, _) = Config::load(Some(&path)).expect("the file still opens");
        assert_eq!(c.context.target_tokens, None);
        assert_eq!(c.mode.as_deref(), Some("ask"), "the rest of the file is intact");
        std::fs::remove_dir_all(&dir).ok();
    }


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

    /// The trap this writer exists to avoid: in TOML a bare key belongs to
    /// whichever `[table]` is open, so `mode = "auto"` written after `[provider]`
    /// parses as `provider.mode`, is rejected as an unknown field, and the
    /// setting the user just made does nothing.
    #[test]
    fn a_top_level_key_lands_before_the_first_table() {
        let text = "workspace = \"~/cwoffee-workspace\"\nmode = \"ask\"\n\n[provider]\nbase_url = \"http://127.0.0.1:8090/v1\"\n";
        let (out, changed) = Config::set_keys_in(text, None, &[("mode", &Config::toml_str("auto"))]);
        assert!(out.contains("mode = \"auto\""), "{out}");
        assert!(!out.contains("mode = \"ask\""), "{out}");
        let at_mode = out.find("mode =").unwrap();
        let at_table = out.find("[provider]").unwrap();
        assert!(at_mode < at_table, "a top-level key written after a table is that table's key: {out}");
        assert_eq!(changed.len(), 1);
        // The file still parses, and the value is the one the user chose.
        let cfg: Config = toml::from_str(&out).expect("the rewritten file must parse");
        assert_eq!(cfg.mode.as_deref(), Some("auto"));
    }

    /// A file that starts with a table has no top-level region at all. The key
    /// still has to go above every table, not inside the first one.
    #[test]
    fn a_top_level_key_goes_above_every_table_when_there_is_no_top_level() {
        let text = "[provider]\nbase_url = \"http://127.0.0.1:8090/v1\"\n";
        let (out, _) = Config::set_keys_in(text, None, &[("mode", &Config::toml_str("session"))]);
        let at_mode = out.find("mode =").unwrap();
        let at_table = out.find("[provider]").unwrap();
        assert!(at_mode < at_table, "{out}");
        let cfg: Config = toml::from_str(&out).expect("the rewritten file must parse");
        assert_eq!(cfg.mode.as_deref(), Some("session"));
        assert_eq!(cfg.provider.base_url, "http://127.0.0.1:8090/v1", "{out}");
    }

    /// A commented-out top-level key is the shipped example's shape. Adding a second
    /// line would leave two answers to one question, one of them inactive.
    #[test]
    fn a_commented_top_level_key_is_replaced_not_added_to() {
        let text = "mode = \"ask\"\n# reasoning = \"low\"\n\n[provider]\n";
        let (out, _) = Config::set_keys_in(text, None, &[("reasoning", &Config::toml_str("xhigh"))]);
        assert_eq!(out.matches("reasoning").count(), 1, "{out}");
        assert!(out.contains("reasoning = \"xhigh\""), "{out}");
        let cfg: Config = toml::from_str(&out).expect("the rewritten file must parse");
        assert_eq!(cfg.reasoning_checked().unwrap(), Some(crate::provider::Reasoning::XHigh));
    }

    /// A file whose first line is the key being written. The old region arithmetic
    /// started looking one line in, missed it, and appended a second copy - which is
    /// a duplicate key, i.e. a file the next start cannot parse.
    #[test]
    fn a_top_level_key_on_the_very_first_line_is_replaced_not_duplicated() {
        let text = "mode = \"ask\"\n\n[provider]\nbase_url = \"http://127.0.0.1:8090/v1\"\n";
        let (out, changed) = Config::set_keys_in(text, None, &[("mode", &Config::toml_str("auto"))]);
        assert_eq!(out.matches("mode = ").count(), 1, "{out}");
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert!(changed[0].contains("->"), "should read as a replacement: {changed:?}");
        let cfg: Config = toml::from_str(&out).expect("one key, once: {out}");
        assert_eq!(cfg.mode.as_deref(), Some("auto"));
    }

    /// Placement is not the same as validity, and the difference cost a real start:
    /// the writer put `model` and `reasoning` at the top level, perfectly above the
    /// first table, in a parser that declares neither - so the file the app had just
    /// written made the **next** boot refuse to open it. Every assertion above passes
    /// on that file.
    ///
    /// So `set_keys` reads its own output back before replacing anything, and this
    /// pins both halves: the refusal, and the file left exactly as it was.
    #[test]
    fn a_written_config_the_next_boot_cannot_read_is_refused_and_the_file_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("cwoffee_cfg_guard_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let original =
            "mode = \"ask\"\n\n[provider]\nbase_url = \"http://127.0.0.1:8090/v1\"\nmodel = \"old-model\"\n";
        std::fs::write(&path, original).unwrap();

        // `model` is not a top-level key: it belongs to [provider]. Writing it at the
        // top level is the mistake that broke the start, and it is now a refused write
        // rather than an app that will not boot.
        let err = Config::set_top_keys(&path, &[("model", &Config::toml_str("new-model"))])
            .expect_err("a key the parser does not declare must not be written");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, original, "a refused write must leave the file alone: {err}");
        assert!(err.contains("would not parse"), "{err}");

        // The same value in the place the reader declares works, and replaces rather
        // than adds: two `model` lines would be two answers to one question.
        Config::set_provider_keys(&path, &[("model", &Config::toml_str("new-model"))]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).expect("the rewritten file must parse");
        assert_eq!(cfg.provider.model, "new-model", "{text}");
        assert_eq!(text.matches("model = ").count(), 1, "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A thinking level is persisted by the host, so the parser has to declare it and
    /// the reader has to name the key when the word is wrong.
    #[test]
    fn a_thinking_level_is_read_back_and_a_strange_word_names_the_key() {
        let c: Config = toml::from_str("reasoning = \"xhigh\"\n")
            .expect("the key the writer produces must parse");
        assert_eq!(c.reasoning_checked().unwrap(), Some(crate::provider::Reasoning::XHigh));

        let c: Config = toml::from_str("reasoning = \"thinking\"\n").unwrap();
        let err = c.reasoning_checked().expect_err("a word that is not a level is a typo");
        assert!(err.contains("reasoning"), "{err}");
        assert!(err.contains("xhigh"), "the message should list the choices: {err}");

        // Absent means the shell's own default, not an invented one.
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.reasoning_checked().unwrap(), None);
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

    /// `ask_user` is the one tool that waits on a person, and the right wait depends
    /// on who that person is. Absent means three minutes; a written value is
    /// range-checked and names itself when out of range.
    #[test]
    fn question_timeout_is_a_knob_and_a_bad_value_names_itself() {
        use std::time::Duration;
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.question_timeout(), Duration::from_secs(180), "absent means three minutes");
        assert_eq!(c.question_timeout_checked().unwrap(), Duration::from_secs(180));

        let c: Config = toml::from_str("question_timeout_secs = 900").expect("parses");
        assert_eq!(c.question_timeout_checked().unwrap(), Duration::from_secs(900));

        let c: Config = toml::from_str("question_timeout_secs = 5").expect("parses");
        let err = c.question_timeout_checked().unwrap_err();
        assert!(err.contains("question_timeout_secs"), "{err}");
        assert!(err.contains("15..=3600"), "{err}");
        let c: Config = toml::from_str("question_timeout_secs = 99999").expect("parses");
        assert!(c.question_timeout_checked().is_err(), "a turn parked for a day is a typo");
    }

    /// The default is the proxy, and a written mode has to reach the layer that
    /// builds the tool. Without the second half of this test a `[sandbox] network`
    /// key could parse, validate, and then be dropped on the floor.
    #[test]
    fn egress_defaults_to_the_proxy_and_a_written_mode_reaches_the_tool_layer() {
        use crate::tools::net::{default_allow, NetMode};
        let c: Config = toml::from_str("").unwrap();
        let ec = c.exec_cfg();
        assert_eq!(ec.net.mode, NetMode::Proxy, "the default must be the proxy");
        assert_eq!(ec.net.allow, default_allow(), "no allow key means the built-in list");
        assert_eq!(ec.net.max_bytes_per_call, 512 << 20);

        let off: Config = toml::from_str("[sandbox]\nnetwork = \"off\"\n").unwrap();
        assert_eq!(off.exec_cfg().net.mode, NetMode::Off);

        let open: Config = toml::from_str(
            "[sandbox]\nnetwork = \"open\"\nallow = [\"*.gitlab.com\"]\nmax_mb_per_call = 40\n",
        )
        .unwrap();
        let ec = open.exec_cfg();
        assert_eq!(ec.net.mode, NetMode::Open);
        assert_eq!(ec.net.allow, vec!["*.gitlab.com".to_string()]);
        assert_eq!(ec.net.max_bytes_per_call, 40 << 20);
        assert_eq!(ec.timeout_secs, default_exec_timeout(), "one key must not reset another");
    }

    /// A mode word that does not parse must stop the app. Falling back to the
    /// default would be the worst outcome for a security-shaped key: the user
    /// writes something, gets the default, and has no reason to doubt it.
    #[test]
    fn a_network_mode_that_does_not_parse_names_itself() {
        let c: Config = toml::from_str("[sandbox]\nnetwork = \"disabled\"\n").unwrap();
        let err = c.net_checked().unwrap_err();
        assert!(err.contains("disabled"), "{err}");
        assert!(err.contains("off, proxy, open"), "{err}");
    }

    #[test]
    fn an_allow_entry_that_is_not_a_hostname_names_itself() {
        let c: Config =
            toml::from_str("[sandbox]\nallow = [\"https://pypi.org/simple\"]\n").unwrap();
        let err = c.net_checked().unwrap_err();
        assert!(err.contains("https://pypi.org/simple"), "{err}");
        assert!(err.contains("hostname"), "{err}");
        let ok: Config =
            toml::from_str("[sandbox]\nallow = [\"pypi.org\",\"*.gitlab.com\",\"*\"]\n")
                .unwrap();
        assert!(ok.net_checked().is_ok(), "{:?}", ok.net_checked());
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
        // The example states the egress default in words people can edit. Stating a
        // value that is not the default would mean two defaults.
        assert_eq!(
            example.sandbox.network,
            Config::default().sandbox.network,
            "the example must state the egress default, not a second one"
        );
        assert!(example.sandbox.allow.is_none(), "the example leaves the built-in allowlist in place");
        assert_eq!(example.sandbox.max_mb_per_call, Config::default().sandbox.max_mb_per_call);
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
