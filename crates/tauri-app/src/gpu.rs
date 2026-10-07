//! GPU policy — PLAN.md §12.
//!
//! On the dev box the NVIDIA GPU is the **default EGL vendor** (`10_nvidia.json`
//! sorts before `50_mesa.json`) and has ~44 MiB of VRAM free because llama.cpp
//! owns the rest. If WebKitGTK grabs an NVIDIA context we allocate into a
//! nearly-full GPU and get blank webviews that look like our bug.
//!
//! The connected panel (`card1-eDP-1`) is driven by Intel, so pinning the app to
//! Mesa makes app-and-compositor **same-vendor DMABUF**, which avoids the
//! cross-vendor hazard rather than working around it.
//!
//! These must be set **before any GTK/EGL initialisation**, i.e. at the very top
//! of `main()`. Setting them in a shell is not enough, because the packaged app
//! does not inherit your shell environment.

use std::path::Path;

/// Mesa EGL vendor file. Present on this box (verified).
const MESA_EGL_VENDOR: &str = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";

/// Force libglvnd to only ever hand us a Mesa (Intel iGPU) EGL context.
///
/// Idempotent, and respects an explicit override: if the user already set the
/// variable (e.g. to test a fallback from PLAN.md §12), we do not clobber it.
pub fn pin_intel_gpu() {
    if std::env::var_os("__EGL_VENDOR_LIBRARY_FILENAMES").is_none() {
        if Path::new(MESA_EGL_VENDOR).exists() {
            std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", MESA_EGL_VENDOR);
        } else {
            // Non-Fedora layout: try the usual alternative, else leave it alone
            // and let the §12 fallbacks handle it.
            for alt in [
                "/usr/share/glvnd/egl_vendor.d/50_mesa.json",
                "/etc/glvnd/egl_vendor.d/50_mesa.json",
            ] {
                if Path::new(alt).exists() {
                    std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", alt);
                    break;
                }
            }
        }
    }

    // Only relevant on the X11/GLX path, but harmless and cheap.
    if std::env::var_os("__GLX_VENDOR_LIBRARY_NAME").is_none() {
        std::env::set_var("__GLX_VENDOR_LIBRARY_NAME", "mesa");
    }
}

/// Which `/dev/dri/renderD*` nodes this process actually holds open.
///
/// This is the only honest answer to "which GPU did my app get?" — env vars are
/// a request; open file descriptors are a fact. The M0 acceptance check asserts
/// this contains renderD128 (Intel) and **not** renderD129 (NVIDIA).
pub fn self_render_nodes() -> Vec<String> {
    render_nodes_of("self")
}

fn render_nodes_of(pid: &str) -> Vec<String> {
    let fd_dir = format!("/proc/{pid}/fd");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&fd_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(link) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let target = link.to_string_lossy().to_string();
        if let Some(node) = target.strip_prefix("/dev/dri/") {
            if node.starts_with("renderD") && !out.iter().any(|n: &String| n == node) {
                out.push(node.to_string());
            }
        }
    }
    out.sort();
    out
}

/// Which WebKitGTK rendering path the webview is allowed to use.
///
/// The page is byte-for-byte the same code in a browser tab as in this window,
/// and the blink the user reported appeared **only** in this window. That put the
/// artifact under the DOM, in WebKitGTK's compositing / buffer-exchange path, and
/// it is why every in-page instrument (computed styles, animation events, rAF
/// deltas, committed-block counts) stayed clean while the eye saw a flash:
/// nothing in the page stopped being drawn; a frame of it never arrived.
///
/// A/B on this box, same build, same prompt, user as the instrument:
///
///   WEBKIT_DISABLE_DMABUF_RENDERER unset  -> blink, every streaming turn
///   WEBKIT_DISABLE_DMABUF_RENDERER=1      -> gone, completely, same build
///
/// So the zero-copy DMABUF frame exchange is what drops frames here (WebKitGTK
/// 2.54.0, Wayland, Mesa/i915, panel driven by the same i915 mutter composites
/// on). The copy path it replaces costs a frame copy and nothing measurable: PSS
/// and animation frame cadence are in `docs/BENCHMARKS.md` for both modes.
///
/// The other documented hatch, kept because it is the one to reach for if a
/// future artifact is layer-related rather than exchange-related:
///
///   WEBKIT_DISABLE_COMPOSITING_MODE=1  no accelerated compositing at all
///
/// `CWOFFEE_WEBKIT_MODE` selects one so the user does not need a shell wrapper,
/// and the boot log records the choice - "it blinks on my machine" is
/// unanswerable in eight months without it. Setting it back to `default` puts the
/// zero-copy path back for anyone whose machine does not drop frames.
pub fn apply_webkit_mode() -> WebkitMode {
    let requested = std::env::var("CWOFFEE_WEBKIT_MODE").unwrap_or_else(|_| DEFAULT_WEBKIT_MODE.into());
    match webkit_env_plan(&requested) {
        Ok(plan) => {
            for (k, v) in plan {
                // An explicit empty value counts as unset: `FOO= app` is a user
                // turning the flag off, not a user setting it to "".
                let set = std::env::var_os(k).filter(|v| !v.is_empty()).is_some();
                if !set {
                    std::env::set_var(k, v);
                }
            }
            WebkitMode {
                name: normalise_webkit_mode(&requested),
                warning: None,
            }
        }
        Err(msg) => WebkitMode {
            name: DEFAULT_WEBKIT_MODE.into(),
            warning: Some(msg),
        },
    }
}

/// The default, and why it is not `default`: see the doc comment on
/// `apply_webkit_mode`. WebKitGTK's zero-copy renderer drops frames on this
/// class of Mesa/Wayland setup, and a dropped frame in a chat app is a bug the
/// user can see, while the copy it costs us is invisible.
const DEFAULT_WEBKIT_MODE: &str = "no-dmabuf";

fn normalise_webkit_mode(mode: &str) -> String {
    match mode {
        "no-dmabuf" | "no_dmabuf" => "no-dmabuf",
        "no-compositing" | "no_compositing" => "no-compositing",
        _ => "default",
    }
    .to_string()
}

/// Which WebKit env vars a mode asks for. Pure, so the spelling table can be
/// tested without racing the process-wide environment.
fn webkit_env_plan(mode: &str) -> Result<Vec<(&'static str, &'static str)>, String> {
    match mode {
        "default" | "" => Ok(vec![]),
        "no-dmabuf" | "no_dmabuf" => Ok(vec![("WEBKIT_DISABLE_DMABUF_RENDERER", "1")]),
        "no-compositing" | "no_compositing" => Ok(vec![("WEBKIT_DISABLE_COMPOSITING_MODE", "1")]),
        other => Err(format!(
            "unknown CWOFFEE_WEBKIT_MODE {other:?}; expected default | no-dmabuf | no-compositing"
        )),
    }
}

#[derive(Clone, Debug)]
pub struct WebkitMode {
    pub name: String,
    pub warning: Option<String>,
}

/// Report for the UI / CLI.
#[derive(serde::Serialize)]
pub struct GpuReport {
    pub egl_vendor: String,
    pub glx_vendor: String,
    pub render_nodes: Vec<String>,
    /// true if we accidentally hold an NVIDIA node
    pub leaked_nvidia: bool,
}

pub fn report() -> GpuReport {
    let nodes = self_render_nodes();
    GpuReport {
        egl_vendor: std::env::var("__EGL_VENDOR_LIBRARY_FILENAMES")
            .unwrap_or_else(|_| "<unset>".into()),
        glx_vendor: std::env::var("__GLX_VENDOR_LIBRARY_NAME")
            .unwrap_or_else(|_| "<unset>".into()),
        leaked_nvidia: nodes.iter().any(|n| n == "renderD129"),
        render_nodes: nodes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_is_idempotent_and_respects_override() {
        std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", "/custom/mesa.json");
        pin_intel_gpu();
        assert_eq!(
            std::env::var("__EGL_VENDOR_LIBRARY_FILENAMES").unwrap(),
            "/custom/mesa.json"
        );
    }

    #[test]
    fn self_scan_does_not_panic() {
        // we may or may not hold render nodes depending on when this runs
        let _ = render_nodes_of("self");
    }

    #[test]
    fn webkit_mode_table_is_exact_and_refuses_invention() {
        // The shipped default is the copy path, not WebKit's own default: the
        // zero-copy renderer drops frames here. Pinning the string means a
        // future "cleanup" back to WebKit's default has to be a decision.
        assert_eq!(DEFAULT_WEBKIT_MODE, "no-dmabuf");
        assert!(webkit_env_plan("default").unwrap().is_empty());
        assert_eq!(
            webkit_env_plan("no-dmabuf").unwrap(),
            vec![("WEBKIT_DISABLE_DMABUF_RENDERER", "1")]
        );
        assert_eq!(
            webkit_env_plan("no-compositing").unwrap(),
            vec![("WEBKIT_DISABLE_COMPOSITING_MODE", "1")]
        );
        // underscore spellings are accepted, because every other knob is CWOFFEE_*
        assert_eq!(webkit_env_plan("no_dmabuf").unwrap().len(), 1);
        // a typo must not quietly run the broken default path and look like a fix
        assert!(webkit_env_plan("no-comp").is_err());
        assert_eq!(normalise_webkit_mode("no_compositing"), "no-compositing");
        assert_eq!(normalise_webkit_mode("default"), "default");
    }
}
