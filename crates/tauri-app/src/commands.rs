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
