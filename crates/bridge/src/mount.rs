//! Getting extensions and a provider onto disk where pi will find them.
//!
//! pi loads extensions and providers from its config home (`--agent-dir` here), and
//! an extension written against `@earendil-works/pi-coding-agent` only resolves if
//! that package is reachable from the file. Both facts are easy to get subtly wrong,
//! so they live in one place the probe and the gateway share instead of twice in
//! whichever binary remembered them.

use std::path::{Path, PathBuf};

/// node_modules of the installed pi, found by walking up from its real binary.
///
/// The binary is usually a symlink into a versioned install, so it is canonicalised
/// first: walking up from `~/.local/bin/pi` finds nothing, while walking up from
/// `.../lib/node_modules/@earendil-works/pi-coding-agent/...` finds the tree that
/// extension imports need.
pub fn pi_node_modules(pi_bin: &Path) -> Option<PathBuf> {
    let bin = find_bin(pi_bin).and_then(|p| std::fs::canonicalize(p).ok())?;
    let mut dir = bin.parent()?;
    for _ in 0..8 {
        let marker = dir.join("node_modules").join("@earendil-works").join("pi-coding-agent");
        if marker.exists() {
            return Some(dir.join("node_modules"));
        }
        dir = dir.parent()?;
    }
    None
}

/// A bare `--pi pi` means "whichever pi this shell would run", and canonicalising the
/// literal string `pi` finds nothing. Same PATH rules as a shell: first match wins.
fn find_bin(name_or_path: &Path) -> Option<PathBuf> {
    if name_or_path.components().count() > 1 {
        return Some(name_or_path.to_path_buf());
    }
    let name = name_or_path.to_str()?;
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let cand = PathBuf::from(dir).join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Copy each extension into `<agent_dir>/extensions/<name>` and link pi's own
/// `node_modules` beside them, returning the paths to pass as `-e`.
///
/// Copying rather than pointing at the source keeps the isolated agent dir honest:
/// pi sees exactly the extensions we chose, and a stray file in a checkout cannot
/// join a running session. The symlink is what lets those imports resolve without
/// installing the world into the agent dir.
pub fn mount_extensions(
    agent_dir: &Path,
    pi_bin: &Path,
    srcs: &[PathBuf],
) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if srcs.is_empty() {
        return Ok(out);
    }
    let dir = agent_dir.join("extensions");
    std::fs::create_dir_all(&dir)?;
    for src in srcs {
        let name = src
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "extension.ts".into());
        let dst = dir.join(&name);
        std::fs::copy(src, &dst)?;
        out.push(dst);
    }
    let link = agent_dir.join("node_modules");
    if !link.exists() {
        match pi_node_modules(pi_bin) {
            Some(mods) => {
                std::os::unix::fs::symlink(&mods, &link)?;
                eprintln!("[bridge] linked {} for extension imports", mods.display());
            }
            None => eprintln!(
                "[bridge] no node_modules found next to {}; extensions may fail to load",
                pi_bin.display()
            ),
        }
    }
    Ok(out)
}

/// cwoffee's `kind` word to pi's `api` word. The two projects name the same thing
/// differently, and guessing wrong here means a request pi cannot route — so the
/// mapping is written down rather than inferred. `llama.cpp` speaks the OpenAI wire,
/// which is why cwoffee's own provider for it is an OpenAI one with a different base.
pub fn pi_api_for_kind(kind: &str) -> &'static str {
    match kind.trim().to_ascii_lowercase().as_str() {
        "anthropic" => "anthropic-messages",
        _ => "openai-completions",
    }
}

/// Write a single-provider `models.json` pointing at `base_url`.
///
/// **Development and smoke only.** The gateway needs *some* provider file to point
/// pi at cwoffee's own mock endpoint, which is what makes a pi-core gateway testable
/// with cwoffee's existing fixtures. Real packaging — where the key comes from the
/// user's config, is never written into a world-readable file, and probably uses pi's
/// own auth store instead — is S4.1's decision, and pretending this solved it would
/// be worse than admitting it is a fixture.
/// The same ceiling the Rust core would compute: the pin from config if there is one,
/// otherwise the engine's share of the window the endpoint states, otherwise the shared
/// fallback. Both cores draw one meter from one formula, or a dual-core comparison
/// turns into an argument about arithmetic.
pub fn context_budget(pinned: Option<usize>, window: Option<usize>) -> usize {
    pinned
        .or_else(|| window.map(cwoffee_engine::host::auto_budget))
        .unwrap_or(cwoffee_engine::host::FALLBACK_BUDGET)
}

/// Tell pi where the ceiling is, so its own folding and cwoffee's meter are one number
/// stated twice.
///
/// pi folds when `contextTokens > contextWindow - compaction.reserveTokens` (its default
/// reserve is 16 384, chosen for coding sessions). cwoffee's meter fills against a
/// budget. Writing `reserveTokens = window - budget` makes the fold happen where the app
/// says the conversation is full, instead of pi folding on a default nobody chose while
/// the meter still shows a third of the window free.
///
/// It goes in as a **per-model override** (`compaction.modelOverrides["provider/model"]`),
/// not globally: a reserve computed from one model's window must not follow a different
/// model around. Everything else in the file is left exactly as it was — the agent dir is
/// cwoffee's own, but a person may have put settings there, and a settings panel that
/// rewrites the file wholesale is a panel that deletes somebody's configuration.
///
/// `None` is returned when the window is not known. Then pi keeps its defaults, and the
/// caller says so rather than claiming a threshold it never wrote.
pub fn mount_settings(
    agent_dir: &Path,
    provider: &str,
    model: &str,
    budget: usize,
    window: usize,
) -> anyhow::Result<Option<String>> {
    if window == 0 || budget >= window {
        return Err(anyhow::anyhow!(
            "a ceiling of {budget} tokens leaves nothing of a {window}-token window for the answer"
        ));
    }
    std::fs::create_dir_all(agent_dir)?;
    let path = agent_dir.join("settings.json");
    let mut doc: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|e| {
                anyhow::anyhow!(
                    "{} does not parse ({e}); refusing to overwrite a settings file we did not write",
                    path.display()
                )
            })?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        // Named, and with the promise that nothing changed: a bare `Permission denied` is
        // technically true and useless, because the reader cannot tell which of pi's files
        // cwoffee walked away from, or whether it left a half-written one.
        Err(e) => {
            return Err(anyhow::anyhow!(
                "cannot read {}: {e} — nothing was written",
                path.display()
            ))
        }
    };
    if !doc.is_object() {
        return Err(anyhow::anyhow!(
            "{} is not a JSON object; refusing to overwrite it",
            path.display()
        ));
    }
    let reserve = window - budget;
    let key = format!("{provider}/{model}");
    let comp = doc
        .as_object_mut()
        .expect("checked above")
        .entry("compaction")
        .or_insert_with(|| serde_json::json!({}));
    if !comp.is_object() {
        return Err(anyhow::anyhow!(
            "{} already has a `compaction` key that is not an object; refusing to guess what it meant",
            path.display()
        ));
    }
    let over = comp
        .as_object_mut()
        .expect("checked above")
        .entry("modelOverrides")
        .or_insert_with(|| serde_json::json!({}));
    if !over.is_object() {
        return Err(anyhow::anyhow!(
            "{} already has a `compaction.modelOverrides` key that is not an object; refusing to guess",
            path.display()
        ));
    }
    let entry = over
        .as_object_mut()
        .expect("checked above")
        .entry(key.clone())
        .or_insert_with(|| serde_json::json!({}));
    if !entry.is_object() {
        return Err(anyhow::anyhow!(
            "{} already has an override for {key} that is not an object; refusing to guess",
            path.display()
        ));
    }
    entry["reserveTokens"] = serde_json::json!(reserve);
    let text = serde_json::to_string_pretty(&doc)?;
    // Write-then-rename, as with the config file: a settings file truncated by a crash
    // half-way through is a file pi cannot open on the next start.
    let tmp = agent_dir.join("settings.json.tmp");
    std::fs::write(&tmp, format!("{text}\n"))?;
    std::fs::rename(&tmp, &path)?;
    Ok(Some(format!(
        "pi will fold above {budget} of {window} tokens ({} compaction.modelOverrides[\"{key}\"].reserveTokens = {reserve})",
        path.display()
    )))
}

/// Ask the endpoint what window this model has, through the same probe the Rust core
/// uses. `None` means the endpoint never said, which the UI reports rather than guesses.
pub async fn probe_window(provider: &dyn cwoffee_engine::provider::Provider, model: &str) -> Option<u64> {
    let infos = provider.list_models_info().await.ok()?;
    infos
        .into_iter()
        .find(|m| m.id == model)
        .and_then(|m| m.context_tokens)
}

/// Mount every endpoint the model screen registered, so a model picked on the screen is a
/// model pi can actually reach.
///
/// Without this, `set_model("box/qwen3-4b")` on the pi core names a model pi has never heard
/// of: the screen would offer a choice the core cannot honour, and the failure would surface as
/// a provider error far from the click that caused it. The chosen endpoint's own entry is
/// written by `write_dev_provider`; this covers the rest, and only the rest.
///
/// A removed endpoint row leaves its pi entry behind. That is deliberate bookkeeping-shyness:
/// cwoffee's model list comes from cwoffee's config, so a leftover entry is unreachable from
/// the app, and deleting files behind a row the user already deleted is the sort of surprise
/// that costs somebody their hand-written provider.
pub fn mount_provider_rows(
    agent_dir: &Path,
    cfg_path: &Path,
) -> anyhow::Result<Vec<String>> {
    let rows = cwoffee_engine::config::provider_rows_in(cfg_path);
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let known: Vec<(String, cwoffee_engine::config::ModelRow)> =
        cwoffee_engine::config::model_rows_in(cfg_path);
    std::fs::create_dir_all(agent_dir)?;
    let path = agent_dir.join("models.json");
    // "Could not read it" and "there is nothing there" are different answers, and the old
    // arm treated both as the second: a `models.json` that existed but could not be read
    // (permissions, a hung mount point) was parsed as `{}` and then written back whole,
    // which would have erased the endpoint registrations cwoffee does not own. Absence is
    // the only state that starts from an empty document; anything else is a refusal.
    let mut doc: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "cannot read {}: {e} — nothing was written",
                path.display()
            ))
        }
    };
    if !doc.is_object() {
        return Err(anyhow::anyhow!("{} is not a JSON object", path.display()));
    }
    let providers = doc
        .as_object_mut()
        .expect("object")
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}));
    let mut mounted = Vec::new();
    for (name, row) in &rows {
        let api = match row.kind.as_str() {
            "anthropic" => "anthropic-messages",
            _ => "openai-completions",
        };
        let mut models = Vec::new();
        for id in &row.models {
            // cwoffee ids are `provider/model`; pi's entry id is the bare model under its
            // provider, so the two spellings have to agree at the seam.
            let bare = id.split_once('/').map(|(_, m)| m).unwrap_or(id.as_str());
            if bare.is_empty() {
                continue;
            }
            let facts = known.iter().find(|(mid, _)| mid == id).map(|(_, r)| r.clone());
            let mut entry = serde_json::json!({ "id": bare });
            let vision = facts.as_ref().and_then(|f| f.supports_vision);
            entry["input"] = serde_json::json!(match vision {
                Some(false) => vec!["text"],
                // Unknown or yes: cwoffee accepts image attachments, and pi drops a picture
                // quietly if the entry says text-only. A model that truly cannot see images
                // answers the request with an error the user can read, which is better than a
                // picture that disappears.
                _ => vec!["text", "image"],
            });
            if let Some(w) = facts.as_ref().and_then(|f| f.context_window) {
                entry["contextWindow"] = serde_json::json!(w);
                let cap = (w / 2).max(1);
                entry["maxTokens"] = serde_json::json!(cap);
            }
            models.push(entry);
        }
        if models.is_empty() {
            continue;
        }
        let entry = serde_json::json!({
            "baseUrl": row.base_url,
            "api": api,
            // A provider entry with no `apiKey` is not registered by pi at all — probed
            // against pi 1.2.0: a row mounted without a key never appears in
            // `get_available_models`, and `set_model` answers "Model not found" for a model
            // the settings screen had just offered. That is why every endpoint added on the
            // screen was useless for inference. A keyless local server gets the same
            // placeholder cwoffee's own provider entry has always carried ("unused"), which
            // llama.cpp ignores and pi accepts. Omitting the field, which this used to do to
            // avoid an empty Authorization header, is what hid the endpoint.
            "apiKey": if row.api_key.trim().is_empty() { "unused".to_string() } else { row.api_key.clone() },
            "models": models,
        });
        providers
            .as_object_mut()
            .expect("object")
            .insert(name.clone(), entry);
        mounted.push(format!("{name} ({} model(s))", models.len()));
    }
    let body = serde_json::to_string_pretty(&doc)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(mounted)
}

/// The thinking-level fields for one provider kind, or nothing when cwoffee cannot say
/// which dialect that endpoint speaks. See `write_dev_provider` for why the fields exist.
fn reasoning_decl(kind: &str, efforts: &[String]) -> Option<serde_json::Value> {
    let known: Vec<String> = efforts.iter().map(|e| e.trim().to_lowercase()).collect();
    // No probe data: fall back to the values the dialect itself documents, which is what the
    // rust core assumes for the same kind. Stated as the dialect's values, not as a claim
    // about this particular server - a level the server rejects shows up as an endpoint error
    // the user can read, which beats a dial that silently does nothing.
    let known = if !known.is_empty() {
        known
    } else {
        match kind {
            "llama.cpp" | "llamacpp" | "local" => vec!["low".into(), "medium".into(), "xhigh".into()],
            "openai" => vec!["low".into(), "medium".into(), "high".into()],
            _ => return None,
        }
    };
    let takes = |v: &str| known.iter().any(|e| e == v);
    // pi's own levels are off, minimal, low, medium, high, xhigh, max. `xhigh` and `max` are
    // only offered at all when the map names them, so the map is written per level rather
    // than left to pi's defaults.
    let map = serde_json::json!({
        "minimal": if takes("minimal") { serde_json::json!("minimal") } else if takes("low") { serde_json::json!("low") } else { serde_json::Value::Null },
        "low": if takes("low") { serde_json::json!("low") } else if takes("minimal") { serde_json::json!("minimal") } else { serde_json::Value::Null },
        "medium": if takes("medium") { serde_json::json!("medium") } else { serde_json::Value::Null },
        "high": if takes("high") { serde_json::json!("high") } else if takes("xhigh") { serde_json::json!("xhigh") } else { serde_json::Value::Null },
        "xhigh": if takes("xhigh") { serde_json::json!("xhigh") } else { serde_json::Value::Null },
        "max": if takes("max") { serde_json::json!("max") } else { serde_json::Value::Null },
    });
    match kind {
        // A llama.cpp server feeds the template, so the knob is a template variable: exactly
        // the pair the rust core sends, and for the same reason - `enable_thinking` alone
        // leaves the effort at the template's own default, which makes the menu a no-op.
        "llama.cpp" | "llamacpp" | "local" => Some(serde_json::json!({
            "reasoning": true,
            "thinkingLevelMap": map,
            "compat": {
                "thinkingFormat": "chat-template",
                "chatTemplateKwargs": {
                    "enable_thinking": { "$var": "thinking.enabled" },
                    "reasoning_effort": { "$var": "thinking.effort", "omitWhenOff": true },
                },
            },
        })),
        // A vanilla OpenAI-shaped endpoint takes a top-level `reasoning_effort`.
        "openai" => Some(serde_json::json!({
            "reasoning": true,
            "thinkingLevelMap": map,
            "compat": { "thinkingFormat": "openai", "supportsReasoningEffort": true },
        })),
        _ => None,
    }
}

/// What `write_dev_provider` actually did to pi's `models.json`.
///
/// This exists because the boot line used to print "provider cwoffee -> <url> (written
/// to …)" unconditionally, and in the case that mattered nothing had been written: the
/// file still held a provider cwoffee pointing at an endpoint from an earlier run (the
/// scripted one, on a port nothing was listening on), the app still asked pi for the
/// model it had configured under that provider, pi could not find it, and every turn
/// ended with an empty answer and no error anywhere. A log line that asserts a write is
/// worse than no log line when it is wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum MountOutcome {
    /// The entry now says what this app says it should: absent and created, or already
    /// pointing at this endpoint and refreshed with the window and modalities just probed.
    Written,
    /// The entry pointed at another endpoint and could not answer for the model this app
    /// is about to ask pi for, so it was replaced. `previous` names the baseUrl that was
    /// there, because "why did my pi config change" deserves an address.
    Replaced { previous: String },
    /// A provider entry that *can* answer for this model, on a baseUrl cwoffee did not
    /// write, was left alone — and that baseUrl is where the requests will really go.
    Foreign { base_url: String },
}

/// Write a single-provider `models.json` pointing at `base_url`.
///
/// The ownership rule, in one sentence: **an entry that cannot answer for the model this
/// app is about to request is replaced; an entry that can is left exactly as it is.**
/// That is the whole guard. "Same provider name, different baseUrl, therefore not ours"
/// was the old rule, and it stranded a stale scripted endpoint in the file forever: the
/// app then held a model name pi had never heard of, and the conversation died with zero
/// characters on screen and no error to read. Keeping a working hand-written entry is
/// still the right call, and when that happens the caller says which baseUrl the requests
/// will actually travel to.
///
/// **Development and smoke only** in origin: the gateway needs *some* provider file to
/// point pi at cwoffee's own mock endpoint, which is what makes a pi-core gateway
/// testable with cwoffee's existing fixtures. Real packaging is S4.1's decision.
pub fn write_dev_provider(
    agent_dir: &Path,
    provider: &str,
    base_url: &str,
    model: &str,
    api_key: &str,
    api: &str,
    kind: &str,
    window: Option<u64>,
    efforts: &[String],
) -> anyhow::Result<(PathBuf, MountOutcome)> {
    std::fs::create_dir_all(agent_dir)?;
    // The context window goes in when we know it. Without it pi reports no window for
    // this model, cwoffee's metrics panel has no denominator, and the user sees "no
    // window reported" where a percentage should be. A number nobody probed is worse
    // than nothing: it would draw a full-looking meter out of a guess.
    let mut entry = serde_json::json!({ "id": model });
    // cwoffee accepts image attachments and hands them to pi on the prompt. If the entry
    // says nothing about modalities, pi classifies the model as text-only and drops the
    // picture before the request — quietly, with no error anywhere. Declaring text+image
    // matches what the app already promises the user; an endpoint that cannot see images
    // then says so in the response, which is a failure the user can actually read.
    entry["input"] = serde_json::json!(["text", "image"]);
    // Thinking levels, declared rather than assumed. pi-ai's `getSupportedThinkingLevels`
    // returns `["off"]` for a model entry that does not say `reasoning: true`, and
    // `clampThinkingLevel` then turns whatever the app asked for into `off` before the request
    // is built - so on a pi core the composer's dial was accepted, reported back as applied,
    // and never reached the wire. That is the whole of "changing reasoning effort does nothing".
    //
    // Which fields to write is a claim about the endpoint, so it comes out of cwoffee's own
    // config: the provider kind picks the wire dialect (the same choice `Config::build_provider`
    // makes for the rust core, where `Flavor::LlamaCpp` sends `chat_template_kwargs` and
    // `Flavor::OpenAi` sends a top-level `reasoning_effort`), and the probed `reasoning_efforts`
    // list picks which levels exist. A level the endpoint never offered is written as `null`,
    // which is how pi is told not to offer it either - better than sending a value the server
    // answers with a 400.
    if let Some(decl) = reasoning_decl(kind, efforts) {
        for (k, v) in decl.as_object().expect("reasoning_decl builds an object") {
            entry[k] = v.clone();
        }
    } else {
        eprintln!(
            "[cwoffee/pi] no thinking levels declared for {model} (provider kind {kind:?} is not one \
             cwoffee knows how to speak): pi will clamp every reasoning request to off for this model"
        );
    }
    if let Some(w) = window {
        entry["contextWindow"] = serde_json::json!(w);
        // pi's provider composer defaults a model's max output tokens to 16384 when the
        // entry says nothing (`definition.maxTokens ?? 16384`). For a long single answer —
        // an SVG of a pelican on a bicycle, a table of a quarter's expenses — that shows up
        // as a reply that stops in the middle with no error anywhere. Declare the cap from
        // the window the endpoint stated: half of it, never below pi's own default, and with
        // room left for the prompt. No window probed, no number invented: pi keeps its default.
        let half = (w / 2).max(16_384).min(w.saturating_sub(4_096).max(1));
        entry["maxTokens"] = serde_json::json!(half);
    }
    let path = agent_dir.join("models.json");
    // Merge, and overwrite only the section this app owns. Skipping the whole file when it
    // already exists — which this used to do — left whatever entry was there first: no
    // `input`, and pi then dropped image attachments before the request, quietly. And
    // clobbering a `models.json` that belongs to somebody else's pi setup (same provider
    // name, different baseUrl) would edit a config we do not own.
    // The same three answers, kept separate: absent (start fresh), unreadable (refuse),
    // unparseable (refuse). The old pair of catch-alls turned either into "start fresh",
    // and this function then *writes* the document — so a `models.json` that merely could
    // not be read for a moment would be replaced with cwoffee's entry alone, taking every
    // other provider in the file with it.
    let mut doc: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!(
                "{} does not parse ({e}); refusing to overwrite a models file we did not write",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "cannot read {}: {e} — nothing was written",
                path.display()
            ))
        }
    };
    let existing = doc.get("providers").and_then(|p| p.get(provider));
    let previous = existing
        .and_then(|p| p.get("baseUrl"))
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_string();
    let same_base = existing.is_some() && previous == base_url;
    // Can this entry answer for the model the app is about to ask pi for? That single
    // question replaces "is this file ours": if the answer is no, the app cannot use the
    // entry either way, so leaving it behind is not caution — it is a guaranteed failure
    // with no error message.
    let answers_for_model = existing
        .and_then(|p| p.get("models"))
        .and_then(|m| m.as_array())
        .map(|list| {
            list.iter()
                .any(|m| m.get("id").and_then(|i| i.as_str()) == Some(model))
        })
        .unwrap_or(false);
    let outcome = if existing.is_some() && !same_base && answers_for_model {
        eprintln!(
            "[cwoffee/pi] provider {provider} in {} points at {previous}, not at the endpoint in \
             cwoffee's config ({base_url}) — but it does list {model}, so it is somebody's entry and it \
             stays. Requests for {model} will go to {previous}, not to {base_url}.",
            path.display()
        );
        MountOutcome::Foreign { base_url: previous }
    } else if existing.is_none() || same_base {
        MountOutcome::Written
    } else {
        eprintln!(
            "[cwoffee/pi] provider {provider} in {} pointed at {previous}, which does not list {model}. \
             Replacing it: cwoffee is about to ask pi for {provider}/{model}, and an entry without that \
             model is a turn that ends with nothing on screen and no error to read.",
            path.display()
        );
        MountOutcome::Replaced { previous }
    };
    if matches!(outcome, MountOutcome::Foreign { .. }) {
        return Ok((path, outcome));
    }
    if doc.get("providers").is_none() {
        doc["providers"] = serde_json::json!({});
    }
    doc["providers"][provider] = serde_json::json!({
        "baseUrl": base_url, "api": api, "apiKey": api_key,
        "models": [entry]
    });
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok((path, outcome))
}

/// Which provider/model pair pi has to be told about for a cwoffee model id.
///
/// The two spellings differ, and confusing them is why a model added on the settings
/// screen could not be used for inference. cwoffee files a registered endpoint's models
/// under a row (`[providers."openrouter"]` with `models = ["claude-x"]`) and the screen
/// offers the model by the name in that row; pi keys the same thing by its provider entry
/// name. Sending the launch provider (`cwoffee`) for every model — which is what this
/// used to do — asks pi for `cwoffee/claude-x`, a model pi has never heard of, so the
/// button that says "answer with this" answers with an error.
///
/// `Err` names the ambiguity instead of picking one: two registered endpoints that both
/// list the same model id mean nothing says which server the person meant, and guessing
/// picks a bill or a model they did not choose.
pub fn pi_target(cfg_path: &Path, own_provider: &str, id: &str) -> anyhow::Result<(String, String)> {
    let id = id.trim();
    if id.is_empty() {
        return Err(anyhow::anyhow!("no model was named"));
    }
    let rows = cwoffee_engine::config::provider_rows_in(cfg_path);
    // An explicit `row/model` spelling wins: it is what `mount_provider_rows` writes into
    // pi's file, and it is unambiguous by construction.
    if let Some((row, bare)) = id.split_once('/') {
        if rows.iter().any(|(n, _)| n == row) {
            return Ok((row.to_string(), bare.to_string()));
        }
        // The launch provider's own prefix is not a row: `[provider]` has no name in the
        // config file, so a spelling like `cwoffee/gpt` can only mean "the endpoint cwoffee
        // owns the entry for". Leaving the prefix on the model id would hand pi a model name
        // with a slash in it, and pi would answer "Model not found" for a model that is
        // sitting in its own file.
        if row == own_provider && !bare.is_empty() {
            return Ok((own_provider.to_string(), bare.to_string()));
        }
    }
    let hits: Vec<String> = rows
        .iter()
        .filter(|(_, r)| r.models.iter().any(|m| m.trim() == id))
        .map(|(n, _)| n.clone())
        .collect();
    match hits.len() {
        0 => Ok((own_provider.to_string(), id.to_string())),
        1 => Ok((hits[0].clone(), id.to_string())),
        _ => Err(anyhow::anyhow!(
            "{id} is registered on {} of cwoffee's endpoints ({}), so nothing says which server to \
             answer with. Remove one of those endpoint rows, or register the model under one endpoint.",
            hits.len(),
            hits.join(", ")
        )),
    }
}

/// Is this provider/model pair already in pi's `models.json`?
///
/// The mount of registered endpoints happens at boot, which is a snapshot: an endpoint
/// added while the window is open is in cwoffee's config and not in pi's file, and
/// "answer with this model" then fails over a model pi has never seen.
pub fn provider_has_model(agent_dir: &Path, provider: &str, model: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(agent_dir.join("models.json")) else {
        return false;
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    doc.get("providers")
        .and_then(|p| p.get(provider))
        .and_then(|p| p.get("models"))
        .and_then(|m| m.as_array())
        .map(|list| {
            list.iter()
                .any(|m| m.get("id").and_then(|i| i.as_str()) == Some(model))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The entry pi is handed has to be one pi will answer a thinking level with.
    ///
    /// pi-ai's `getSupportedThinkingLevels` returns `["off"]` for a model entry that does not
    /// say `reasoning: true`, and `clampThinkingLevel` then maps whatever the app asked for onto
    /// that - before the request is built. An entry filed without these fields is therefore a
    /// composer dial that reports success and changes nothing on the wire, which is exactly the
    /// report we got: "changing reasoning effort doesn't take effect".
    ///
    /// The fields are pinned against `tests/fixtures/pi-model-entry.json`, and
    /// `scripts/pi-reasoning-gate.mjs` feeds that same fixture to pi's own functions. One
    /// fixture, both halves: this test grades what cwoffee writes, the gate grades what pi makes
    /// of it.
    #[test]
    fn the_pi_entry_declares_the_thinking_levels_the_rust_core_would_send() {
        let root = std::env::temp_dir().join(format!("cw-reasoning-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let efforts: Vec<String> = ["low", "medium", "xhigh"].iter().map(|s| s.to_string()).collect();
        let (p, _) = write_dev_provider(
            &root,
            "cwoffee",
            "http://127.0.0.1:8090/v1",
            "m1",
            "k",
            "openai-completions",
            "llama.cpp",
            Some(262144),
            &efforts,
        )
        .unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let entry = &doc["providers"]["cwoffee"]["models"][0];
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/pi-model-entry.json")).unwrap();
        for key in ["reasoning", "thinkingLevelMap", "compat"] {
            assert_eq!(entry[key], fixture[key], "{key} differs from the fixture pi is graded against");
        }
        // `high` is folded onto `xhigh` because that is what the template on this endpoint
        // accepts - the same folding the rust core does in `llama_effort`, so a conversation
        // gets the same thinking whichever core is behind the window.
        assert_eq!(entry["thinkingLevelMap"]["high"], "xhigh", "high folds to the level the template takes");
        // A level the endpoint never offered is written as null, which is pi's own "do not offer
        // this": sending a value the server 400s on is worse than a dial with fewer stops.
        assert!(entry["thinkingLevelMap"]["max"].is_null(), "no max claimed for a template that has none");

        // No probe data is not the same claim as no thinking: the dialect's documented values
        // go in, because the failure being fixed is a dial that silently does nothing.
        let blind = root.join("blind");
        write_dev_provider(
            &blind,
            "cwoffee",
            "http://127.0.0.1:8090/v1",
            "m1",
            "k",
            "openai-completions",
            "llama.cpp",
            None,
            &[],
        )
        .unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(blind.join("models.json")).unwrap()).unwrap();
        let entry = &doc["providers"]["cwoffee"]["models"][0];
        assert_eq!(entry["reasoning"], serde_json::json!(true), "an unprobed llama.cpp endpoint still gets a dial that reaches the wire");
        assert!(
            entry["compat"]["chatTemplateKwargs"]["reasoning_effort"].is_object(),
            "enable_thinking alone leaves the effort at the template's own default"
        );

        // A dialect cwoffee does not speak gets no claim at all - and the boot line says pi will
        // clamp, rather than leaving it to be discovered as a broken dial.
        let other = root.join("other");
        write_dev_provider(
            &other,
            "cwoffee",
            "http://127.0.0.1:8090/v1",
            "m1",
            "k",
            "anthropic-messages",
            "anthropic",
            None,
            &[],
        )
        .unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(other.join("models.json")).unwrap()).unwrap();
        let entry = &doc["providers"]["cwoffee"]["models"][0];
        assert!(entry.get("reasoning").is_none(), "a dialect cwoffee cannot speak is not a dialect to guess about");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_provider_entry_we_write_carries_the_window_and_both_modalities() {
        let root = std::env::temp_dir().join(format!("cw-provider-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (p, wrote) = write_dev_provider(&root, "cwoffee", "http://127.0.0.1:8090/v1", "m1", "k", "openai-completions", "llama.cpp", Some(262144), &[]).unwrap();
        assert_eq!(wrote, MountOutcome::Written, "a fresh file is a write");
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let entry = &doc["providers"]["cwoffee"]["models"][0];
        assert_eq!(entry["contextWindow"].as_u64(), Some(262144), "the probed window goes in");
        assert_eq!(entry["input"].as_array().map(|a| a.len()), Some(2), "text and image, or pi drops pictures");
        assert_eq!(entry["maxTokens"].as_u64(), Some(131_072), "half the window, or pi cuts the reply at 16384");
        // A small window keeps pi's default rather than inventing a smaller cut.
        let small = root.join("small");
        write_dev_provider(&small, "cwoffee", "http://127.0.0.1:8090/v1", "m1", "k", "openai-completions", "llama.cpp", Some(8192), &[]).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&small.join("models.json")).unwrap()).unwrap();
        assert_eq!(doc["providers"]["cwoffee"]["models"][0]["maxTokens"].as_u64(), Some(4_096), "never above the window minus the prompt");
        // And no window means no claim: pi's own default stays in charge.
        let blind = root.join("blind");
        write_dev_provider(&blind, "cwoffee", "http://127.0.0.1:8090/v1", "m1", "k", "openai-completions", "llama.cpp", None, &[]).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&blind.join("models.json")).unwrap()).unwrap();
        assert!(doc["providers"]["cwoffee"]["models"][0].get("maxTokens").is_none(), "no probed window, no invented cap");
        // Same baseUrl: refreshed in place, so a window learned later actually lands.
        let (again_path, wrote) = write_dev_provider(&root, "cwoffee", "http://127.0.0.1:8090/v1", "m1", "k", "openai-completions", "llama.cpp", Some(131072), &[]).unwrap();
        assert_eq!(again_path, p, "the same file, not a second one");
        assert_eq!(wrote, MountOutcome::Written, "same baseUrl is a refresh, not a takeover");
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["providers"]["cwoffee"]["models"][0]["contextWindow"].as_u64(), Some(131072));
        // An entry under cwoffee's own provider name that cannot answer for the model this
        // app is about to request gets replaced, whatever baseUrl it points at. This is the
        // installed-app bug: a previous run had left provider `cwoffee` pointing at the
        // scripted endpoint on a dead port, listing only `cwoffee-mock`. The old rule saw a
        // different baseUrl and refused to touch "a config we do not own"; the app went on
        // asking pi for `cwoffee/qwen3.8-flash-next-iq3_s`, pi had no such model, and every
        // turn ended with nothing on screen and no error to read.
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("models.json"),
            r#"{"providers":{"cwoffee":{"api":"openai-completions","apiKey":"unused","baseUrl":"http://127.0.0.1:8193/v1","models":[{"id":"cwoffee-mock"}]},"someone-else":{"baseUrl":"https://their.server/v1","models":[{"id":"keepme"}]}}}"#,
        )
        .unwrap();
        let (p2, wrote) = write_dev_provider(&other, "cwoffee", "http://127.0.0.1:8090/v1", "qwen3.8-flash-next-iq3_s", "k", "openai-completions", "llama.cpp", Some(262144), &[]).unwrap();
        assert_eq!(
            wrote,
            MountOutcome::Replaced { previous: "http://127.0.0.1:8193/v1".into() },
            "a stale entry that cannot answer for the model is replaced, and the old address is named"
        );
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p2).unwrap()).unwrap();
        assert_eq!(doc["providers"]["cwoffee"]["baseUrl"].as_str(), Some("http://127.0.0.1:8090/v1"), "ours was rewritten to the endpoint in use");
        assert_eq!(doc["providers"]["cwoffee"]["models"][0]["id"].as_str(), Some("qwen3.8-flash-next-iq3_s"), "the model pi is about to be asked for is in the entry");
        assert_eq!(doc["providers"]["someone-else"]["baseUrl"].as_str(), Some("https://their.server/v1"), "a different provider in the same file is not ours to touch");
        // The other direction, which is the reason the guard exists at all: an entry that
        // *can* answer for this model on a baseUrl cwoffee did not write is somebody's
        // configuration, so it stays — and the caller is told where the requests go.
        let mine = root.join("mine");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(
            mine.join("models.json"),
            r#"{"providers":{"cwoffee":{"api":"openai-completions","baseUrl":"https://hand.written/v1","models":[{"id":"qwen3.8-flash-next-iq3_s"}]}}}"#,
        )
        .unwrap();
        let before = std::fs::read_to_string(mine.join("models.json")).unwrap();
        let (p3, wrote) = write_dev_provider(&mine, "cwoffee", "http://127.0.0.1:8090/v1", "qwen3.8-flash-next-iq3_s", "k", "openai-completions", "llama.cpp", Some(262144), &[]).unwrap();
        assert_eq!(
            wrote,
            MountOutcome::Foreign { base_url: "https://hand.written/v1".into() },
            "an entry that lists the model is left alone"
        );
        assert_eq!(std::fs::read_to_string(&p3).unwrap(), before, "left alone means not one byte changed");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The fact the whole mount depends on, probed against pi 1.2.0 rather than assumed:
    /// a provider entry **without an `apiKey` is not registered at all**. It does not appear
    /// in `get_available_models`, and `set_model` for a model under it answers "Model not
    /// found" — for a model the settings screen had just offered, on an endpoint the person
    /// had just added. Every mounted provider therefore carries a key: the row's if it has
    /// one, the same placeholder cwoffee's own provider entry has always used otherwise.
    #[test]
    fn a_mounted_endpoint_is_a_provider_pi_will_actually_register() {
        let root = std::env::temp_dir().join(format!("cw-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg = root.join("config.toml");
        std::fs::write(
            &cfg,
            "[providers.\"lan\"]\nkind = \"llama.cpp\"\nbase_url = \"http://192.168.0.122:8080/v1\"\napi_key = \"\"\nmodels = [\"lan/qwen3-4b\"]\n\n[providers.\"paid\"]\nkind = \"openai\"\nbase_url = \"https://paid/v1\"\napi_key = \"sk-real\"\nmodels = [\"paid/big\"]\n",
        )
        .unwrap();
        mount_provider_rows(&root, &cfg).unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("models.json")).unwrap()).unwrap();
        for name in ["lan", "paid"] {
            let key = doc["providers"][name]["apiKey"].as_str().unwrap_or("");
            assert!(!key.is_empty(), "{name}: an entry with no apiKey is invisible to pi");
        }
        assert_eq!(doc["providers"]["lan"]["apiKey"].as_str(), Some("unused"), "the placeholder, not an empty header");
        assert_eq!(doc["providers"]["paid"]["apiKey"].as_str(), Some("sk-real"), "a real key is the person's own, untouched");
        // And pi's own rule, from the same probe: `set_model` needs the provider/model pair.
        // An id that only carries cwoffee's spelling cannot be looked up.
        assert!(provider_has_model(&root, "lan", "qwen3-4b"), "pi knows it under its row, not under cwoffee");
        assert!(!provider_has_model(&root, "cwoffee", "lan/qwen3-4b"), "nor under a provider that was never mounted");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A model id is not a provider. cwoffee offers a registered endpoint's model by the
    /// name in its row; pi needs the pair. Getting this wrong is "added models cannot be
    /// used for inference": the app asked pi for `cwoffee/<model>`, a model pi had never
    /// heard of, and the model picker's answer was an error about a model nobody named.
    #[test]
    fn a_model_id_becomes_the_provider_pi_keys_it_under() {
        let root = std::env::temp_dir().join(format!("cw-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg = root.join("config.toml");
        std::fs::write(
            &cfg,
            "[provider]\nkind = \"llama.cpp\"\nbase_url = \"http://127.0.0.1:8090/v1\"\nmodel = \"qwen3.8-flash-next-iq3_s\"\n\n[providers.\"box\"]\nkind = \"llama.cpp\"\nbase_url = \"http://192.168.0.122:8080/v1\"\napi_key = \"\"\nmodels = [\"box/qwen3-4b\"]\n\n[providers.\"127.0.0.1-8090-v1\"]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:8090/v1\"\napi_key = \"\"\nmodels = [\"qwen3.8-flash-next-iq3_s\"]\n",
        )
        .unwrap();
        // The prefixed spelling, which is what a probed endpoint writes into `[models.]`.
        assert_eq!(pi_target(&cfg, "cwoffee", "box/qwen3-4b").unwrap(), ("box".into(), "qwen3-4b".into()));
        // The bare spelling, filed under a row that lists it (this user's config shape).
        assert_eq!(
            pi_target(&cfg, "cwoffee", "qwen3.8-flash-next-iq3_s").unwrap(),
            ("127.0.0.1-8090-v1".into(), "qwen3.8-flash-next-iq3_s".into())
        );
        // A model on no row at all is the endpoint in `[provider]`, which is the one cwoffee
        // owns the entry for.
        assert_eq!(pi_target(&cfg, "cwoffee", "unlisted-model").unwrap(), ("cwoffee".into(), "unlisted-model".into()));
        // A slash that names no row is not a provider: inventing one produces a model pi has
        // never seen, which is the failure this whole function exists to stop.
        assert_eq!(pi_target(&cfg, "cwoffee", "nosuchrow/m1").unwrap(), ("cwoffee".into(), "nosuchrow/m1".into()));
        // The launch provider's own prefix does mean its provider, because it is the one name
        // that is not a row in the file: `[provider]` is unnamed.
        assert_eq!(pi_target(&cfg, "cwoffee", "cwoffee/m1").unwrap(), ("cwoffee".into(), "m1".into()));
        // Two endpoints offering the same model: refuse, and name both.
        std::fs::write(
            &cfg,
            "[providers.\"a\"]\nkind = \"openai\"\nbase_url = \"http://a/v1\"\nmodels = [\"same\"]\n\n[providers.\"b\"]\nkind = \"openai\"\nbase_url = \"http://b/v1\"\nmodels = [\"same\"]\n",
        )
        .unwrap();
        let err = pi_target(&cfg, "cwoffee", "same").unwrap_err().to_string();
        assert!(err.contains("a") && err.contains("b") && err.contains("same"), "not actionable: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The mount of registered endpoints runs at boot, so an endpoint added while the
    /// window is open is in cwoffee's config and not yet in pi's file. The model switch
    /// has to be able to notice that, or "answer with this model" fails over a model pi
    /// has never seen.
    #[test]
    fn a_model_added_after_boot_is_not_in_pis_file_until_it_is_mounted() {
        let root = std::env::temp_dir().join(format!("cw-hasmodel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg = root.join("config.toml");
        std::fs::write(
            &cfg,
            "[providers.\"box\"]\nkind = \"llama.cpp\"\nbase_url = \"http://192.168.0.122:8080/v1\"\napi_key = \"\"\nmodels = [\"box/qwen3-4b\"]\n",
        )
        .unwrap();
        assert!(!provider_has_model(&root, "box", "qwen3-4b"), "a missing file has no models in it");
        mount_provider_rows(&root, &cfg).unwrap();
        assert!(provider_has_model(&root, "box", "qwen3-4b"), "mounted, so pi can be told about it");
        assert!(!provider_has_model(&root, "box", "other-model"), "not the same thing as claiming everything");
        assert!(!provider_has_model(&root, "cwoffee", "qwen3-4b"), "under its own provider, not cwoffee's");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The model screen registers endpoints; pi has to be able to reach them or "use this
    /// model" is a button that cannot work.
    #[test]
    fn every_registered_endpoint_becomes_a_provider_pi_can_reach() {
        let root = std::env::temp_dir().join(format!("cw-rows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg = root.join("config.toml");
        std::fs::write(
            &cfg,
            "[provider]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"here/m1\"\n\n[providers.\"box\"]\nkind = \"llama.cpp\"\nbase_url = \"http://192.168.0.122:8080/v1\"\napi_key = \"\"\nmodels = [\"box/qwen3-4b\", \"box/vision-7b\"]\n\n[models.\"box/qwen3-4b\"]\ncontext_window = 131072\nsupports_vision = false\nsource = \"probed\"\n\n[models.\"box/vision-7b\"]\ncontext_window = 8192\nsupports_vision = true\n",
        )
        .unwrap();
        // The chosen endpoint first, the way boot does it.
        let (_p, _w) = write_dev_provider(&root, "here", "http://127.0.0.1:9/v1", "m1", "k", "openai-completions", "llama.cpp", Some(262144), &[]).unwrap();
        let mounted = mount_provider_rows(&root, &cfg).unwrap();
        assert_eq!(mounted, vec!["box (2 model(s))".to_string()], "{mounted:?}");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("models.json")).unwrap()).unwrap();
        // Both endpoints are in the file, and the chosen one is still there on its own baseUrl.
        assert_eq!(doc["providers"]["here"]["baseUrl"].as_str(), Some("http://127.0.0.1:9/v1"));
        let boxp = &doc["providers"]["box"];
        assert_eq!(boxp["baseUrl"].as_str(), Some("http://192.168.0.122:8080/v1"));
        assert_eq!(boxp["api"].as_str(), Some("openai-completions"), "a llama.cpp server speaks the openai wire");
        assert_eq!(boxp["apiKey"].as_str(), Some("unused"), "a keyless local endpoint still needs an apiKey, or pi will not register the provider at all");
        let small = &boxp["models"][0];
        assert_eq!(small["id"].as_str(), Some("qwen3-4b"), "pi's id is bare under its provider");
        assert_eq!(small["contextWindow"].as_u64(), Some(131_072), "the stored window is what pi is told");
        assert_eq!(small["input"].as_array().map(|a| a.len()), Some(1), "a probed text-only model is not shown taking images");
        let eyes = &boxp["models"][1];
        assert_eq!(eyes["input"].as_array().map(|a| a.len()), Some(2));
        // Rerunning boot is idempotent: the same row mounts once, not twice.
        let again = mount_provider_rows(&root, &cfg).unwrap();
        assert_eq!(again, mounted);
        let doc2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("models.json")).unwrap()).unwrap();
        assert_eq!(doc2["providers"]["box"]["models"].as_array().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_mounted_extension_lands_inside_the_agent_dir_and_keeps_its_name() {
        let root = std::env::temp_dir().join(format!("cw-mount-{}", std::process::id()));
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("tools.ts"), "export default 1;").unwrap();
        let agent = root.join("agent");
        let got = mount_extensions(&agent, Path::new("/nonexistent/pi"), &[src.join("tools.ts")]).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].starts_with(&agent), "must be inside the isolated home");
        assert!(got[0].ends_with("extensions/tools.ts"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_extensions_mounts_nothing_rather_than_an_empty_directory_surprise() {
        let root = std::env::temp_dir().join(format!("cw-mount-none-{}", std::process::id()));
        let got = mount_extensions(&root, Path::new("/nonexistent/pi"), &[]).unwrap();
        assert!(got.is_empty());
        assert!(!root.join("extensions").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_bare_name_resolves_through_path_the_way_a_shell_does() {
        // The default --pi value is the literal "pi". Finding it is what makes the
        // node_modules lookup work instead of quietly skipping the symlink.
        assert!(find_bin(Path::new("sh")).is_some());
        assert!(find_bin(Path::new("definitely-not-a-binary-xyz")).is_none());
        assert_eq!(find_bin(Path::new("/bin/sh")).unwrap(), PathBuf::from("/bin/sh"));
    }

    /// "Cannot read it" is not "there is nothing there". The arm that conflated them read a
    /// `models.json` blocked by permissions as `{}` and then wrote that back — erasing the
    /// endpoint registrations in a file cwoffee does not own. The refusal names the file and
    /// leaves the bytes alone.
    #[test]
    fn a_file_that_cannot_be_read_is_not_treated_as_absent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("unreadable");
        let agent = dir.join("pi-agent");
        std::fs::create_dir_all(&agent).unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(
            &cfg,
            "[providers.theirs]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:8090/v1\"\nmodels = [\"theirs/x\"]\n",
        )
        .unwrap();

        for name in ["models.json", "settings.json"] {
            let path = agent.join(name);
            let mine = format!("{{\"kept\": \"for {name}\"}}");
            // A previous failed run can leave a 0o000 file of the same name behind, and
            // writing to that is the very error this test is about — clear it first so the
            // failure under test is the one being provoked.
            let _ = std::fs::remove_file(&path);
            std::fs::write(&path, &mine).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read_to_string(&path).is_ok() {
                // Running as root: permission bits do not bind, so this case would assert
                // nothing. Say so rather than report a pass that proved it.
                eprintln!("[cwoffee/test] skipping {name}: running as root, 0o000 does not bind");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
                continue;
            }
            let e = match name {
                "models.json" => mount_provider_rows(&agent, &cfg).unwrap_err().to_string(),
                _ => mount_settings(&agent, "p", "m", 1000, 4000).unwrap_err().to_string(),
            };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(e.contains(name), "{e}");
            assert!(e.contains("nothing was written"), "{e}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                mine,
                "{name} was written over after a failed read"
            );
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cw-mount-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn pi_is_told_where_to_fold_without_losing_anybody_elses_settings() {
        let dir = scratch("settings");
        // Somebody else's file: a switch, and a compaction setting that is not ours.
        std::fs::write(
            dir.join("settings.json"),
            "{\"showCacheMissNotices\": true, \"compaction\": {\"enabled\": true, \"keepRecentTokens\": 30000}}\n",
        )
        .unwrap();
        let sentence = mount_settings(&dir, "cwoffee", "m1", 131_072, 262_144)
            .unwrap()
            .expect("a window was given, so a threshold can be written");
        assert!(sentence.contains("fold above 131072 of 262144 tokens"), "{sentence}");
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!(doc["showCacheMissNotices"], serde_json::json!(true), "unrelated keys survive");
        assert_eq!(doc["compaction"]["enabled"], serde_json::json!(true));
        assert_eq!(doc["compaction"]["keepRecentTokens"], serde_json::json!(30_000));
        assert_eq!(
            doc["compaction"]["modelOverrides"]["cwoffee/m1"]["reserveTokens"],
            serde_json::json!(131_072),
            "reserve = window - budget: the fold lands where cwoffee's meter says the room is gone"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_settings_file_we_cannot_read_is_left_alone() {
        let dir = scratch("unparseable");
        let text = "{ this is not json, and it is somebody's file\n";
        std::fs::write(dir.join("settings.json"), text).unwrap();
        let e = mount_settings(&dir, "cwoffee", "m1", 8_000, 100_000).unwrap_err().to_string();
        assert!(e.contains("refusing to overwrite"), "{e}");
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.json")).unwrap(),
            text,
            "a refusal changes nothing, including the bytes on disk"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_ceiling_that_leaves_nothing_for_the_answer_is_not_written() {
        let dir = scratch("no-room");
        let e = mount_settings(&dir, "cwoffee", "m1", 100_000, 100_000).unwrap_err().to_string();
        assert!(e.contains("leaves nothing"), "{e}");
        assert!(!dir.join("settings.json").exists(), "no file, no claim");
        std::fs::remove_dir_all(&dir).ok();
    }
}
