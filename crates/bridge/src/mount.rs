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
    if let Some(w) = window {
        entry["contextWindow"] = serde_json::json!(w);
    }
    let models = serde_json::json!({
        "providers": { provider: {
            "baseUrl": base_url, "api": api, "apiKey": api_key,
            "models": [entry]
        }}
    });
    let path = agent_dir.join("models.json");
    std::fs::write(&path, serde_json::to_string_pretty(&models)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
