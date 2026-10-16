//! Naming a conversation with a small model that runs on this machine.
//!
//! The store already gives every conversation a truthful title: the first line of its
//! first message, cut at forty-eight characters ("derive_title"). That is honest and
//! useless in the one place it matters — "hey can you look at this" is a real opening
//! line and a terrible sidebar row, and a sidebar of forty rows of them is why nobody
//! uses titles to find things.
//!
//! So we ask a 230 M-parameter on-device model (LiquidAI/LFM2.5-230M) for a phrase,
//! through `scripts/titlegen.py`. Three constraints shape how:
//!
//!   * **Out of process, one shot.** Loading torch and this model peaks around 870 MB of
//!     resident memory on this box for roughly seven seconds, which is more than the
//!     whole desktop tree normally costs. The helper prints one line and exits, and the
//!     memory goes straight back. A resident title server is possible — the helper has a
//!     `--serve` mode — but it would hold that memory to serve one title per conversation,
//!     and only a caller with an idle reaper should decide that out loud.
//!   * **Never on the turn's critical path.** This runs after a turn settles, and the
//!     result arrives as a rename event. A conversation is never made to wait for its
//!     name.
//!   * **Optional, and it says when it did nothing.** If the script, the python, or the
//!     weights are missing, or the model hands back an answer instead of a name, the
//!     automatic title stays and the reason is reported. A wrong title is a claim about
//!     the conversation; a dull true one is not.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::event::EngineEvent;
use crate::store::StoreOps;

/// What `derive_title` in the store would produce for this text. Public because the
/// comparison *is* the eligibility test: a stored title equal to the automatic one means
/// nobody — user or model — has named the conversation yet.
pub fn fallback_title(text: &str) -> String {
    crate::store::derive_title(text)
}

/// Has this conversation been named by a human, or is it still wearing the automatic
/// title? A hand-named row is never overwritten by a model, however good the phrase.
pub fn still_automatic(stored: &str, first_user_text: &str) -> bool {
    let stored = stored.trim();
    if stored.is_empty() || stored == "new session" {
        return true;
    }
    stored == fallback_title(first_user_text)
}

#[derive(Debug, Clone)]
pub struct TitleGenCfg {
    /// Off unless the config asks, because the cost is a ~870 MB spike and this is a
    /// desktop app that people leave open on laptops.
    pub enabled: bool,
    pub python: PathBuf,
    pub script: PathBuf,
    pub model: String,
    pub cache: PathBuf,
    pub threads: usize,
    pub timeout: Duration,
}

impl Default for TitleGenCfg {
    fn default() -> Self {
        let data = data_dir();
        TitleGenCfg {
            enabled: false,
            python: data.join("titlegen").join("venv").join("bin").join("python"),
            script: default_script(),
            model: "LiquidAI/LFM2.5-230M".into(),
            cache: data.join("titlegen").join("models"),
            threads: 4,
            timeout: Duration::from_secs(45),
        }
    }
}

fn data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".local")
                .join("share")
        });
    base.join("cwoffee")
}

/// Where the helper lives. Installed builds put it beside the binary; a checkout runs it
/// from the repository. Both are checked so a dev tree and an installed app agree.
fn default_script() -> PathBuf {
    if let Ok(p) = std::env::current_exe() {
        for up in [
            p.with_file_name("titlegen.py"),
            p.parent()
                .unwrap_or(&p)
                .join("../../../pi-cwoffee/scripts/titlegen.py"),
        ] {
            if up.is_file() {
                // Resolved for display: a status panel that prints
                // `…/target/release/../../../pi-cwoffee/scripts/titlegen.py` makes someone
                // count `..` segments to work out which file the app means.
                return up.canonicalize().unwrap_or(up);
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/titlegen.py")
}

/// What is and is not present, without running anything. A Settings panel needs to be
/// able to say "the weights are not downloaded" instead of showing a switch that does
/// nothing, and a gate needs the same answer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TitleGenStatus {
    pub enabled: bool,
    pub script: String,
    pub script_found: bool,
    pub python: String,
    pub python_found: bool,
    pub weights_cached: bool,
    pub model: String,
    pub cache: String,
    pub threads: usize,
    /// The measured cost, quoted rather than promised: this is what the helper's own
    /// peak resident set was when it was last measured on this class of machine.
    pub peak_rss_mb: u32,
    /// Whether a naming is happening right now. `None` when the caller cannot know —
    /// a gateway reading the config does not own the host's helper. Saying "not running"
    /// because nobody wired the question is the kind of stale placeholder that got this
    /// app into trouble elsewhere.
    pub in_flight: Option<bool>,
}

impl TitleGenCfg {
    pub fn status(&self, in_flight: Option<bool>) -> TitleGenStatus {
        TitleGenStatus {
            enabled: self.enabled,
            script: self.script.display().to_string(),
            script_found: self.script.is_file(),
            python: self.python.display().to_string(),
            python_found: self.python.is_file(),
            weights_cached: weights_cached(&self.cache, &self.model),
            model: self.model.clone(),
            cache: self.cache.display().to_string(),
            threads: self.threads,
            // Measured, not guessed: see the journal entry for the model choice. The
            // number is quoted in the settings panel so the cost is visible next to the
            // switch that turns it on.
            peak_rss_mb: 870,
            in_flight,
        }
    }

    pub fn usable(&self) -> bool {
        self.enabled && self.script.is_file() && self.python.is_file() && weights_cached(&self.cache, &self.model)
    }

    /// Ask for a name. The helper owns the model, the prompt and the quality check; this
    /// owns the timeout, the process, and the decision that a refusal keeps the old name.
    ///
    /// Blocking on purpose — callers run this through `spawn_blocking`.
    pub fn name_blocking(&self, text: &str) -> Result<String, String> {
        if text.trim().is_empty() {
            return Err("nothing to name: the conversation has no first message".into());
        }
        // Say which piece is missing, in the order a person would go looking for it. A
        // single "titlegen failed" would send them to the model when the venv is what is
        // absent.
        if !self.script.is_file() {
            return Err(format!("the title helper script is not there: {}", self.script.display()));
        }
        if !self.python.is_file() {
            return Err(format!(
                "no python for the title helper at {} — create the venv, or point titlegen_python at one with                  torch and transformers",
                self.python.display()
            ));
        }
        if !weights_cached(&self.cache, &self.model) {
            return Err(format!(
                "the weights for {} are not cached in {} — fetch them with: hf download {} --cache-dir <that directory>",
                self.model,
                self.cache.display(),
                self.model
            ));
        }
        let mut cmd = std::process::Command::new(&self.python);
        cmd.arg(&self.script)
            .arg("--threads")
            .arg(self.threads.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.env("CWOFFEE_TITLEGEN_MODEL", &self.model)
            .env("CWOFFEE_TITLEGEN_CACHE", &self.cache)
            .env("OMP_NUM_THREADS", self.threads.to_string())
            .env("HF_HUB_OFFLINE", "1");
        if let Some(parent) = self.script.parent() {
            cmd.current_dir(parent);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start the title helper ({}): {e}", self.python.display()))?;
        // The pid is ours: we started it, and it is the only process this call is
        // allowed to signal. Killing by name is forbidden in this codebase after an
        // incident; this kills by handle.
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            if let Err(e) = stdin.write_all(text.as_bytes()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not hand the message to the title helper: {e}"));
            }
        }
        let deadline = std::time::Instant::now() + self.timeout;
        let _status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if std::time::Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!(
                            "the title helper did not answer in {}s (a loaded model plus a CPU turn on this \
                             box takes seconds, so something is wrong with the helper, not with the model)",
                            self.timeout.as_secs()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("could not wait for the title helper: {e}"));
                }
            }
        };
        let out = child.wait_with_output().map_err(|e| format!("could not read the helper: {e}"))?;
        let title = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
        let stats = String::from_utf8_lossy(&out.stderr);
        if title.is_empty() {
            // The helper explains itself as JSON on stderr even when it refuses. Quote
            // what it said rather than inventing a reason for it.
            let reason = stats
                .lines()
                .rev()
                .find(|l| l.trim_start().starts_with('{'))
                .and_then(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                .map(|v| {
                    v.get("rejected")
                        .or_else(|| v.get("error"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("no reason given")
                        .to_string()
                })
                .unwrap_or_else(|| "no reason given".to_string());
            let candidate = serde_json::from_str::<serde_json::Value>(
                stats.lines().rev().find(|l| l.trim_start().starts_with('{')).unwrap_or("{}").trim(),
            )
            .ok()
            .and_then(|v| v.get("candidate").and_then(|x| x.as_str()).map(str::to_string))
            .unwrap_or_default();
            let cand = if candidate.is_empty() {
                String::new()
            } else {
                format!(" (it offered {candidate:?})")
            };
            return Err(format!("the title helper refused: {reason}{cand}"));
        }
        if title.chars().count() > 60 || title.contains('\n') {
            return Err(format!("the helper's title is not usable in a row: {title:?}"));
        }
        Ok(title)
    }
}

/// Are the weights in the local cache? Same question the helper asks itself, answered
/// without importing python: a Settings panel should not pay six seconds to say whether
/// a 460 MB download happened.
pub fn weights_cached(cache: &Path, model: &str) -> bool {
    // Hugging Face's on-disk layout: <cache>/models--<org>--<name>/snapshots/<rev>/*.safetensors.
    // Both spellings of the prefix are tried, because `HF_HOME`-style caches sometimes add
    // a `models/` level and a hand-made cache directory sometimes does not.
    let leaf = model.replace('/', "--");
    for root in [
        cache.join(format!("models--{leaf}")),
        cache.join("models").join(format!("models--{leaf}")),
        cache.join("models").join(&leaf),
        cache.join(&leaf),
    ] {
        if !root.is_dir() {
            continue;
        }
        if let Ok(rd) = std::fs::read_dir(&root) {
            if rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().map(|x| x == "safetensors").unwrap_or(false))
            {
                return true;
            }
        }
        if walk_has_safetensors(&root, 3) {
            return true;
        }
    }
    false
}

fn walk_has_safetensors(dir: &Path, depth: usize) -> bool {
    if depth == 0 || !dir.is_dir() {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    for e in rd.filter_map(|x| x.ok()) {
        let p = e.path();
        if p.extension().map(|x| x == "safetensors").unwrap_or(false) {
            return true;
        }
        let hidden = p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(true);
        if p.is_dir() && !hidden {
            if walk_has_safetensors(&p, depth - 1) {
                return true;
            }
        }
    }
    false
}

/// One helper at a time, across the whole process. The reason is not politeness: two
/// concurrent loads would double the spike on a machine that is also running the real
/// model process and the UI, and titles are not urgent enough to compete for that.
#[derive(Clone, Default)]
pub struct TitleGen {
    pub cfg: TitleGenCfg,
    busy: Arc<AtomicBool>,
}

impl TitleGen {
    pub fn new(cfg: TitleGenCfg) -> Self {
        TitleGen { cfg, busy: Arc::new(AtomicBool::new(false)) }
    }

    pub fn in_flight(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Try to claim the single slot. Returns None if one is already running, which is the
    /// correct answer for a second conversation: it keeps the title it already has, and
    /// the next settle will offer it again.
    pub fn claim(&self) -> bool {
        !self.busy.swap(true, Ordering::SeqCst)
    }

    pub fn release(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }
}

/// Offer a conversation to the helper, if it is still wearing the title we made up for
/// it. Called after a turn settles, never before: the conversation has its automatic
/// title the moment its first message is stored, and a name is decoration on top of that.
///
/// Returns true when an attempt was started. Every failure path leaves the automatic
/// title in place and says why — in the log always, and on screen when the feature was
/// switched on by hand, because a feature someone enabled and which silently did nothing
/// is the worst kind of off.
pub fn maybe_name(
    tg: &TitleGen,
    store: &Arc<dyn StoreOps>,
    ev: &tokio::sync::mpsc::UnboundedSender<EngineEvent>,
    session_id: i64,
) -> bool {
    let cfg = &tg.cfg;
    if !cfg.enabled || !tg.claim() {
        return false;
    }
    let release = || tg.release();
    let row = match store.session(session_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            release();
            return false;
        }
        Err(e) => {
            release();
            eprintln!("[cwoffee/titlegen] could not read session {session_id}: {e}");
            return false;
        }
    };
    let first = match store.history(session_id) {
        Ok(h) => h
            .into_iter()
            .find(|m| matches!(m.role, crate::provider::Role::User))
            .map(|m| m.content)
            .unwrap_or_default(),
        Err(e) => {
            release();
            eprintln!("[cwoffee/titlegen] could not read the conversation: {e}");
            return false;
        }
    };
    if first.trim().is_empty() || !still_automatic(&row.title, &first) {
        // Someone named it, or it already has a model's name: never overwrite a choice.
        release();
        return false;
    }
    if !cfg.usable() {
        let st = cfg.status(Some(true));
        let what = if !st.script_found {
            format!("no helper script at {}", st.script)
        } else if !st.python_found {
            format!("no python at {}", st.python)
        } else if !st.weights_cached {
            format!("weights for {} are not cached", st.model)
        } else {
            "the helper is not usable".to_string()
        };
        let keeping = row.title.clone();
        eprintln!("[cwoffee/titlegen] asked for a title and cannot: {what} — keeping {keeping:?}");
        let _ = ev.send(EngineEvent::Note {
            session: Some(session_id),
            message: format!(
                "titlegen is on but cannot run: {what}. The conversation keeps its automatic title;                  the peak cost of the helper is about {} MB of memory for a few seconds per new                  conversation, which is why it is opt-in.",
                st.peak_rss_mb
            ),
        });
        release();
        return false;
    }
    // Owned copies, because the task outlives this call: the host loop cannot be held
    // open while a model loads, and a title is not worth stalling a turn for.
    let tg_run = tg.clone();
    let tg_done = tg.clone();
    let store2 = Arc::clone(store);
    let ev2 = ev.clone();
    let text = first.clone();
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || tg_run.cfg.name_blocking(&text)).await;
        tg_done.release();
        match result {
            Ok(Ok(title)) => match store2.rename_session(session_id, &title) {
                Ok(()) => {
                    let _ = ev2.send(EngineEvent::SessionChanged { session_id, title: title.clone() });
                }
                Err(e) => eprintln!("[cwoffee/titlegen] named it {title:?} but the rename failed: {e}"),
            },
            Ok(Err(why)) => {
                eprintln!("[cwoffee/titlegen] session {session_id} keeps its automatic title: {why}");
                let _ = ev2.send(EngineEvent::Note {
                    session: Some(session_id),
                    message: format!("the local title model did not name this conversation: {why}"),
                });
            }
            Err(e) => eprintln!("[cwoffee/titlegen] the helper task died: {e}"),
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untitled_conversation_is_offered_and_a_named_one_is_not() {
        let first = "hey can you look at my quarterly cashflow spreadsheet please";
        let auto = fallback_title(first);
        assert!(still_automatic("new session", first), "an untouched row is eligible");
        assert!(still_automatic(&auto, first), "the derived title is eligible");
        assert!(!still_automatic("Quarterly cashflow gap", first), "a named row is never touched");
        assert!(still_automatic("", first), "an empty stored title is treated as unnamed");
    }

    #[test]
    fn a_missing_helper_is_reported_as_absent_rather_than_broken() {
        let mut cfg = TitleGenCfg::default();
        cfg.enabled = true;
        cfg.script = PathBuf::from("/definitely/not/here/titlegen.py");
        let st = cfg.status(None);
        assert!(!st.script_found);
        assert!(!cfg.usable());
        // The answer comes from files, not from a subprocess: a Settings panel that has
        // to load a model to report its own state is a panel that freezes.
        let err = cfg.name_blocking("hello there").unwrap_err();
        assert!(err.contains("title helper script is not there"), "{err}");
    }

    #[test]
    fn one_helper_at_a_time_across_the_process() {
        let tg = TitleGen::new(TitleGenCfg::default());
        assert!(tg.claim(), "the first ask wins the slot");
        assert!(!tg.claim(), "a second concurrent ask is turned away, not queued behind a 900 MB load");
        tg.release();
        assert!(tg.claim(), "and the slot comes back");
    }
}
