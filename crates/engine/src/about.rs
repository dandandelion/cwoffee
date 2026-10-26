//! What this build is, and where the person's things live.
//!
//! One struct, filled by whichever transport is answering, so the desktop window and a phone on the
//! gateway read the same list of facts rather than two hand-written strings that drift apart. The
//! fields are deliberately plain strings and numbers: this is a page for a human who is trying to
//! find a file or file a bug report, and every field has to be something the answering process
//! *knows* rather than infers.
//!
//! The reason it exists at all: this app writes a config file, a conversation database, a workspace
//! of produced documents, and (on the pi core) a model-owned thread file in a directory of its own.
//! Those four are the whole of the product's state, they are spread over two or three places under
//! `$HOME`, and until now the only way to learn where any of them was is to read the source or the
//! install script. A support question like "did my history survive the upgrade?" should be answerable
//! from inside the app.

use crate::config::Config;
use std::path::Path;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct About {
    /// The product's own name. Constant, and worth stating on the page rather than assuming: the
    /// window title, the launcher and this page have to agree about what the app is called.
    pub app: String,
    /// The build's version, from the crate. The same number the bundle file names itself after.
    pub version: String,
    /// Which engine is answering: `rust` (the in-process agent) or `pi` (a `pi --mode rpc` child).
    /// Asked of the host rather than read from an environment variable by the page, so the answer
    /// cannot disagree with the thing that is actually running.
    pub core: String,
    /// The model the current conversation would be sent to, as the composer names it.
    pub model: String,
    /// The endpoint that model came from. Empty means none is configured — which is the state a
    /// first run is in, and the About page is one of the places that has to say so plainly.
    pub endpoint: String,
    /// How many endpoints this build knows about. The list itself belongs to the Models page; this
    /// is only the count, so About never becomes a second copy of that table.
    pub endpoints: usize,
    pub conversations: usize,
    pub messages: usize,
    /// The folder new conversations work in, as the composer's chip names it.
    pub workspace: String,
    /// The config file the app read (or would read). Written back to by the settings screens, so
    /// naming it is naming the place a lost setting went to.
    pub config_path: String,
    /// The SQLite record: conversations, messages, attachments, the change log.
    pub db_path: String,
    /// Where the model's own copy of the conversation lives. On the pi core that is a directory of
    /// thread files pi owns; the Rust core keeps its history inside the record above and says so.
    pub thread_store: String,
    /// Which web search is on, in words a person can act on (`duckduckgo`, `searxng`, `off`).
    pub search: String,
    /// Remote access: `off`, or where it listens. The token is never in here — a page that shows a
    /// token to a screenshot is a leaked token.
    pub gateway: String,
    /// How this binary was built, e.g. `linux x86_64 · release`. Bug reports need it; "I downloaded
    /// the AppImage" does not say which of the three bundles arrived.
    pub build: String,
}

/// The parts every host knows: the record's counts, the model on the composer, the endpoint it came
/// from, and the search choice. A transport adds its own paths and build stamp on top of this, which
/// is why the struct has `Default` — the fields a given host cannot know stay empty, and the page
/// skips empty rows instead of inventing them.
pub fn from_config(cfg: &Config, cfg_path: &Path, conversations: usize, messages: usize) -> About {
    // The endpoint picture comes from `provider_status_with`, not from `cfg.provider` directly,
    // because the environment is layered over the file at boot. An About page that printed the file's
    // `[provider]` line while the process answers through `CWOFFEE_URL` would name an address the
    // app is not talking to — the exact confusion that made the settings screen offer to probe
    // somebody's live server during a scripted run.
    let status = crate::config::provider_status_with(cfg_path, cfg);
    About {
        app: "cwoffee".into(),
        version: crate::VERSION.into(),
        core: String::new(), // the host answers this about itself
        model: status.chosen.clone(),
        endpoint: status.chosen_base_url.clone(),
        endpoints: status.providers.len(),
        conversations,
        messages,
        workspace: cfg.workspace.clone().unwrap_or_default(),
        config_path: String::new(),
        db_path: String::new(),
        thread_store: String::new(),
        search: cfg.search.backend.trim().to_string(),
        gateway: String::new(),
        build: build_stamp(),
    }
}

/// `<os> <arch> · <profile>`. Compile-time on purpose: the person reading the page cannot be asked
/// what profile a menu-launched binary was built with.
pub fn build_stamp() -> String {
    format!(
        "{} {} · {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(debug_assertions) { "debug" } else { "release" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counts and the endpoint come from what is actually configured, and an app with no
    /// endpoint configured says so with an empty string rather than naming a port nobody chose.
    #[test]
    fn about_reports_the_endpoint_that_exists_and_not_one_invented() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        let a = from_config(&cfg, Path::new("/nonexistent/cwoffee.toml"), 3, 41);
        assert_eq!(a.app, "cwoffee");
        assert_eq!(a.version, crate::VERSION);
        assert_eq!(a.conversations, 3);
        assert_eq!(a.messages, 41);
        assert_eq!(a.endpoint, "", "an app with no endpoint has no endpoint to name");
        assert_eq!(a.endpoints, 0);
        assert!(a.build.contains("·"), "the build stamp names the profile: {}", a.build);
    }

    #[test]
    fn about_names_the_endpoint_and_model_in_use() {
        let cfg: Config = serde_json::from_str(
            r#"{
                "provider": { "kind": "openai", "base_url": "http://127.0.0.1:8090/v1", "model": "qwen-mini" },
                "providers": {
                  "home": { "kind": "openai", "base_url": "http://127.0.0.1:8090/v1", "models": ["qwen-mini"] },
                  "work": { "kind": "anthropic", "base_url": "https://api.example.com", "models": ["a", "b"] }
                },
                "search": { "backend": "searxng", "searxng_url": "http://127.0.0.1:8899" }
            }"#,
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("cw-about-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[provider]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:8090/v1\"\nmodel = \"qwen-mini\"\n\n[providers.\"home\"]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:8090/v1\"\nmodels = [\"qwen-mini\"]\n\n[providers.\"work\"]\nkind = \"anthropic\"\nbase_url = \"https://api.example.com\"\nmodels = [\"a\", \"b\"]\n\n[search]\nbackend = \"searxng\"\nsearxng_url = \"http://127.0.0.1:8899\"\n",
        )
        .unwrap();
        let a = from_config(&cfg, &path, 12, 340);
        assert_eq!(a.endpoint, "http://127.0.0.1:8090/v1");
        assert_eq!(a.model, "qwen-mini");
        assert_eq!(a.endpoints, 2, "two endpoints are configured, and About counts them");
        assert_eq!(a.search, "searxng");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
