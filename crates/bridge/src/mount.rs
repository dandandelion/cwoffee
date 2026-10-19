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
        Err(e) => return Err(e.into()),
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

pub fn write_dev_provider(
    agent_dir: &Path,
    provider: &str,
    base_url: &str,
    model: &str,
    api_key: &str,
    api: &str,
    window: Option<u64>,
) -> anyhow::Result<PathBuf> {
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
    let mut doc: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };
    let ours = doc
        .get("providers")
        .and_then(|p| p.get(provider))
        .map(|p| p.get("baseUrl").and_then(|b| b.as_str()) == Some(base_url))
        .unwrap_or(true);
    if !ours {
        eprintln!(
            "[cwoffee/pi] {} already defines provider {provider} for another baseUrl ({}). Not overwriting a config we do not own — cwoffee's context window and modality claims will not apply, so a picture may be dropped by pi before the request.",
            path.display(),
            doc["providers"][provider]["baseUrl"]
                .as_str()
                .unwrap_or("(none)")
        );
        return Ok(path);
    }
    if doc.get("providers").is_none() {
        doc["providers"] = serde_json::json!({});
    }
    doc["providers"][provider] = serde_json::json!({
        "baseUrl": base_url, "api": api, "apiKey": api_key,
        "models": [entry]
    });
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provider_entry_we_write_carries_the_window_and_both_modalities() {
        let root = std::env::temp_dir().join(format!("cw-provider-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let p = write_dev_provider(&root, "cwoffee", "http://127.0.0.1:8090/v1", "m1", "k", "openai-completions", Some(262144)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let entry = &doc["providers"]["cwoffee"]["models"][0];
        assert_eq!(entry["contextWindow"].as_u64(), Some(262144), "the probed window goes in");
        assert_eq!(entry["input"].as_array().map(|a| a.len()), Some(2), "text and image, or pi drops pictures");
        assert_eq!(entry["maxTokens"].as_u64(), Some(131_072), "half the window, or pi cuts the reply at 16384");
        // A small window keeps pi's default rather than inventing a smaller cut.
        let small = root.join("small");
        write_dev_provider(&small, "cwoffee", "http://127.0.0.1:8090/v1", "m1", "k", "openai-completions", Some(8192)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&small.join("models.json")).unwrap()).unwrap();
        assert_eq!(doc["providers"]["cwoffee"]["models"][0]["maxTokens"].as_u64(), Some(4_096), "never above the window minus the prompt");
        // And no window means no claim: pi's own default stays in charge.
        let blind = root.join("blind");
        write_dev_provider(&blind, "cwoffee", "http://127.0.0.1:8090/v1", "m1", "k", "openai-completions", None).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&blind.join("models.json")).unwrap()).unwrap();
        assert!(doc["providers"]["cwoffee"]["models"][0].get("maxTokens").is_none(), "no probed window, no invented cap");
        // Same baseUrl: refreshed in place, so a window learned later actually lands.
        write_dev_provider(&root, "cwoffee", "http://127.0.0.1:8090/v1", "m1", "k", "openai-completions", Some(131072)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["providers"]["cwoffee"]["models"][0]["contextWindow"].as_u64(), Some(131072));
        // Someone else's provider of the same name is left alone.
        let other = root.join("other");
        write_dev_provider(&other, "cwoffee", "http://elsewhere/v1", "x", "k", "openai-completions", None).unwrap();
        std::fs::create_dir_all(&other).ok();
        std::fs::write(other.join("models.json"), std::fs::read_to_string(&p).unwrap()).unwrap();
        write_dev_provider(&other, "cwoffee", "http://127.0.0.1:8090/v1", "m1", "k", "openai-completions", Some(9)).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&other.join("models.json")).unwrap()).unwrap();
        assert_eq!(doc["providers"]["cwoffee"]["baseUrl"].as_str(), Some("http://127.0.0.1:8090/v1"), "ours was replaced, not theirs");
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
