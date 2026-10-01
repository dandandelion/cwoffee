//! Tauri commands. Kept thin on purpose: they start engine work and hand back a
//! channel. No business logic here.

use std::sync::atomic::{AtomicU64, Ordering};

use cowry_engine::stream::{fake_turn, FakeScript};
use cowry_engine::EngineEvent;
use tauri::ipc::Channel;

use crate::gpu;

static NEXT_TURN: AtomicU64 = AtomicU64::new(1);

/// Stream a scripted turn to the front end over a typed Channel.
///
/// We use `Channel<EngineEvent>` rather than `emit` because PLAN.md §1.4 forbids
/// an `invoke()` per token: channels are the cheap path, and the front end
/// batches them on rAF.
#[tauri::command]
pub fn start_fake_turn(
    on_event: Channel<EngineEvent>,
    tok_per_sec: Option<f64>,
    words: Option<usize>,
) -> Result<u64, String> {
    let turn_id = NEXT_TURN.fetch_add(1, Ordering::Relaxed);
    let script = FakeScript {
        tok_per_sec: tok_per_sec.unwrap_or(50.0),
        answer_words: words.unwrap_or(90),
        ..Default::default()
    };

    // The scripted streamer sleeps to imitate token pacing, so it must not run on
    // the main thread.
    std::thread::spawn(move || {
        fake_turn(turn_id, &script, |ev| {
            // A send error means the front end went away mid-turn; that is fine.
            let _ = on_event.send(ev);
        });
    });

    Ok(turn_id)
}

/// Report which GPU this process actually ended up on (PLAN.md §12 check).
#[tauri::command]
pub fn gpu_report() -> gpu::GpuReport {
    gpu::report()
}

/// What the front end actually rendered.
///
/// This exists because every machine-checkable gate in M0 passed while the
/// webview was showing a stale vite error overlay. "Is it painted?" is not
/// answerable from process liveness, memory, or sockets — so the UI reports it.
#[derive(serde::Deserialize)]
pub struct RenderReport {
    pub titlebar: bool,
    pub sidebar: bool,
    pub footer: bool,
    pub stream_el: bool,
    pub body_bg: String,
    pub overlay_present: bool,
    pub text_len: usize,
    pub device_pixel_ratio: f64,
}

#[tauri::command]
pub fn render_report(report: RenderReport) {
    let ok = report.titlebar
        && report.footer
        && report.stream_el
        && !report.overlay_present
        && report.text_len > 0;
    eprintln!(
        "[cowry] render {}: titlebar={} sidebar={} footer={} stream={} overlay={} text={} bg={} dpr={:.2}",
        if ok { "OK" } else { "SUSPECT" },
        report.titlebar,
        report.sidebar,
        report.footer,
        report.stream_el,
        report.overlay_present,
        report.text_len,
        report.body_bg,
        report.device_pixel_ratio,
    );
}
