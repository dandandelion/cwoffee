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
}
