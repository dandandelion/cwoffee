//! Cowry desktop host.
//!
//! Thin by design. Everything interesting lives in `crates/engine`.

mod commands;
mod gpu;

fn main() {
    // MUST be the first thing that happens: the GPU policy has to be in place
    // before GTK/EGL is initialised, or libglvnd hands us an NVIDIA context on a
    // GPU that is already ~97% full from inference. See PLAN.md §12.
    gpu::pin_intel_gpu();

    let report = gpu::report();
    eprintln!(
        "[cowry] EGL vendor pinned to: {}\n[cowry] GLX vendor: {}",
        report.egl_vendor, report.glx_vendor
    );

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::start_fake_turn,
            commands::gpu_report
        ])
        .run(tauri::generate_context!())
        .expect("error while running cowry");
}
