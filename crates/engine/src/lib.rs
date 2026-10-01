//! Cowry engine.
//!
//! Deliberate constraint: **this crate knows nothing about any UI.** It compiles
//! without tauri, gtk, or a webview, and it must stay that way — the TUI and the
//! desktop app are peers, both consuming [`EngineEvent`].
//!
//! M0 scope: the event contract, telemetry, and a scripted streamer.
//! M1 adds providers, the agent loop, the append-only prompt builder, and storage.

pub mod event;
pub mod telemetry;
pub mod stream;

pub use event::{EngineEvent, ExecStream, FinishReason};
pub use telemetry::{LiveWindow, Rolling, TelemetrySnapshot, TurnTelemetry};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
