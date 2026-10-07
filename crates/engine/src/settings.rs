//! The search provider, as a thing the app can show and change.
//!
//! This lives in the engine rather than in the desktop shell because there are two
//! shells with the same panel: the window and the browser page on the phone. A
//! provider chosen in one has to be the provider the other sees, and the way to
//! guarantee that is one function each of them calls, writing one file.
//!
//! Two rules the shape of this module enforces:
//!
//! * **The file is written first, then the live tool follows.** The other order
//!   leaves the running app and `config.toml` disagreeing about which search is in
//!   use, which is the kind of state where a person "fixed" the setting and it came
//!   back after a restart.
//! * **The status line comes from the tool**, not from the config. A card that
//!   prints what the file says while the engine quietly fell back to DuckDuckGo is
//!   a card that lies.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;
use crate::host::SessionHost;
use crate::tools::searxng::{self, AutoSearXNG};
use crate::tools::web::SearchBackend;

/// What the search panel shows, and what it can change.
#[derive(Debug, Clone, Serialize)]
pub struct SearchView {
    /// `duckduckgo` | `searxng` | `auto` — the same vocabulary as the config file.
    pub backend: String,
    pub searxng_url: String,
    pub auto_port: u16,
    pub auto_idle_secs: u64,
    pub auto_dir: String,
    /// The live tool's own words about what it is doing right now.
    pub describing: String,
    /// `podman` / `docker` / `None`: whether "Cwoffee runs one" is possible here.
    pub runtime: Option<String>,
    /// Present when the managed backend is selected: is the container up, and one
    /// sentence about why not when it is not.
    pub auto: Option<searxng::AutoStatus>,
    /// An environment variable that outranks the file at next start. Said out loud,
    /// because otherwise the panel looks like it is not remembering.
    pub env_override: Option<String>,
    /// Where a change was persisted, when one was made.
    pub saved_to: Option<String>,
}

/// The three spellings the panel offers, and nothing else. A typo here is an error,
/// not a quiet fall back to the provider the panel exists to move away from.
pub fn parse_backend(
    name: &str,
    url: &str,
    cfg: &Config,
) -> Result<SearchBackend, String> {
    match name.trim() {
        "duckduckgo" => Ok(SearchBackend::DuckDuckGo),
        "searxng" => {
            let u = url.trim();
            if u.is_empty() {
                return Err("SearXNG needs a URL, for example http://127.0.0.1:8080/search".into());
            }
            if !u.starts_with("http://") && !u.starts_with("https://") {
                return Err(format!("{u} is not an http(s) URL"));
            }
            Ok(SearchBackend::SearXNG { base: u.to_string() })
        }
        "auto" => Ok(SearchBackend::Auto(AutoSearXNG::new(
            cfg.search.auto_port,
            cfg.search.auto_idle_secs,
            cfg.search_auto_dir(),
        ))),
        other => Err(format!(
            "unknown search provider {other:?}; pick duckduckgo, searxng, or auto"
        )),
    }
}

fn load_cfg(path: &Path) -> Config {
    Config::load(Some(path)).map(|(c, _)| c).unwrap_or_default()
}

/// Read the panel's state: what the file says, what the tool is actually doing, and
/// whether a container is up.
pub async fn search_view(host: &SessionHost, cfg_path: &Path) -> SearchView {
    let cfg = load_cfg(cfg_path);
    let live = host.search_backend();
    let describing = live
        .as_ref()
        .map(|b| b.describe())
        .unwrap_or_else(|| "no search tool is registered in this build".into());
    let backend = live
        .as_ref()
        .map(|b| b.as_str().to_string())
        // A build without the tool still has to show what the config asks for.
        .unwrap_or_else(|| cfg.search.backend.clone());
    // Ask the *live* instance when the live tool is the managed one: it owns the
    // idle clock, so its answer is about the container that actually started.
    let auto = match live {
        Some(SearchBackend::Auto(a)) => Some(a.status().await),
        _ if cfg.search.backend == "auto" => Some(
            AutoSearXNG::new(cfg.search.auto_port, cfg.search.auto_idle_secs, cfg.search_auto_dir())
                .status()
                .await,
        ),
        _ => None,
    };
    let env_override = std::env::var("CWOFFEE_SEARXNG_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|v| {
            format!(
                "CWOFFEE_SEARXNG_URL={v} is set in the app's environment and will outrank \
                 this file at next start"
            )
        });
    SearchView {
        backend,
        searxng_url: cfg.search.searxng_url.clone().unwrap_or_default(),
        auto_port: cfg.search.auto_port,
        auto_idle_secs: cfg.search.auto_idle_secs,
        auto_dir: cfg.search_auto_dir().display().to_string(),
        describing,
        runtime: searxng::runtime().map(String::from),
        auto,
        env_override,
        saved_to: None,
    }
}

/// Choose the provider. Writes the config file, then points the live tool at what
/// the file now says.
///
/// Switching away from `auto` does **not** stop or remove a container: disabling a
/// feature should not uninstall something the person may have started themselves.
/// The idle reaper still stops it a couple of minutes after the last search.
pub async fn set_search_backend(
    host: &SessionHost,
    cfg_path: &Path,
    backend: &str,
    searxng_url: &str,
) -> Result<SearchView, String> {
    let backend = backend.trim();
    let url = searxng_url.trim();
    // Validate against the current file before touching it, so a rejected choice
    // changes nothing at all.
    let cfg = load_cfg(cfg_path);
    parse_backend(backend, url, &cfg)?;

    let be = Config::toml_str(backend);
    let mut kvs: Vec<(String, String)> = vec![("backend".into(), be.clone())];
    if backend == "searxng" {
        kvs.push(("searxng_url".into(), Config::toml_str(url)));
    }
    let krefs: Vec<(&str, &str)> = kvs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let changed = Config::set_search_keys(cfg_path, &krefs)?;
    for c in &changed {
        eprintln!("[cwoffee] search config: {c} ({})", cfg_path.display());
    }

    // Re-read what was just written: what the running tool gets must be exactly
    // what the next start will read, or "saved" is a claim about a different app.
    let cfg = load_cfg(cfg_path);
    let b = parse_backend(
        &cfg.search.backend,
        &cfg.search.searxng_url.clone().unwrap_or_default(),
        &cfg,
    )?;
    let applied = host
        .set_search_backend(b)
        .ok_or_else(|| "this app has no web_search tool to configure".to_string())?;
    eprintln!("[cwoffee] search: {applied}");
    let mut view = search_view(host, cfg_path).await;
    view.describing = applied;
    view.saved_to = Some(cfg_path.display().to_string());
    Ok(view)
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchProbe {
    pub ms: u128,
    /// What the model would be told when the results did not come from the provider
    /// that was configured.
    pub note: Option<String>,
    pub backend: String,
    pub results: Vec<ProbeHit>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeHit {
    pub title: String,
    pub url: String,
}

/// Run one real search and report what happened. A dropdown alone cannot answer the
/// question the panel exists for: "will search work on this machine".
pub async fn search_probe(host: &SessionHost, query: Option<String>) -> Result<SearchProbe, String> {
    let tool = host
        .search_tool()
        .ok_or_else(|| "this app has no web_search tool".to_string())?;
    let q = query.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "coffee roasting".into());
    let t0 = std::time::Instant::now();
    let backend = tool.backend().describe();
    let (note, res) = tool.search_with(&q).await;
    Ok(match res {
        Ok(results) => SearchProbe {
            ms: t0.elapsed().as_millis(),
            note,
            backend,
            results: results.into_iter().take(5).map(|r| ProbeHit { title: r.title, url: r.url }).collect(),
            error: None,
        },
        Err(e) => SearchProbe {
            ms: t0.elapsed().as_millis(),
            note,
            backend,
            results: Vec::new(),
            error: Some(e),
        },
    })
}

/// Where the panel's file lives when the shell did not load one. Shared so the two
/// shells write the same place.
pub fn default_cfg_path() -> PathBuf {
    Config::default_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn the_three_providers_are_the_only_three() {
        assert!(matches!(parse_backend("duckduckgo", "", &cfg()), Ok(SearchBackend::DuckDuckGo)));
        assert!(matches!(
            parse_backend("searxng", "http://127.0.0.1:8080/search", &cfg()),
            Ok(SearchBackend::SearXNG { .. })
        ));
        assert!(matches!(parse_backend("auto", "", &cfg()), Ok(SearchBackend::Auto(_))));
        // A typo must not become DuckDuckGo: that is the provider being avoided, and
        // a dropdown cannot make this mistake, but a hand-edited config can.
        let e = parse_backend("seaang", "", &cfg()).unwrap_err();
        assert!(e.contains("unknown search provider"), "{e}");
        assert!(e.contains("duckduckgo"), "the error should list what is allowed: {e}");
    }

    #[test]
    fn searxng_without_a_url_is_refused_rather_than_emptied() {
        // `SearXNG { base: "" }` used to reach the tool and fail as a network error
        // against an empty host. Name the missing key instead.
        let e = parse_backend("searxng", "   ", &cfg()).unwrap_err();
        assert!(e.contains("needs a URL"), "{e}");
        let e2 = parse_backend("searxng", "127.0.0.1:8080", &cfg()).unwrap_err();
        assert!(e2.contains("http(s)"), "{e2}");
    }

    #[test]
    fn auto_inherits_the_ports_and_idle_from_the_file() {
        let text = "[search]\nbackend = \"auto\"\nauto_port = 8901\nauto_idle_secs = 45\n";
        let c: Config = toml::from_str(text).expect("parses");
        let b = parse_backend("auto", "", &c).unwrap();
        match b {
            SearchBackend::Auto(a) => {
                assert_eq!(a.port, 8901);
                assert_eq!(a.idle_secs, 45);
                assert_eq!(a.base(), "http://127.0.0.1:8901");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_view_describes_the_same_words_the_tool_uses() {
        // The card and the boot log must not disagree about what search is on.
        let b = parse_backend("auto", "", &cfg()).unwrap();
        assert_eq!(b.describe(), "SearXNG managed by Cwoffee on port 8899");
        assert_eq!(SearchBackend::DuckDuckGo.describe(), "DuckDuckGo (keyless HTML endpoint)");
        assert_eq!(
            parse_backend("searxng", "http://box:8080/search", &cfg())
                .unwrap()
                .describe(),
            "SearXNG at http://box:8080/search"
        );
        assert_eq!(b.as_str(), "auto");
    }
}
