//! Cwoffee engine.
//!
//! Deliberate constraint: **this crate knows nothing about any UI.** It compiles
//! without tauri, gtk, or a webview, and it must stay that way — the TUI and the
//! desktop app are peers, both consuming [`EngineEvent`].
//!
//! M0 scope: the event contract, telemetry, and a scripted streamer.
//! M1 adds providers, the agent loop, the append-only prompt builder, and storage.

pub mod agent;
pub mod approval;
pub mod config;
pub mod event;
pub mod provider;
pub mod prompt;
pub mod sse;
pub mod legacy;
pub mod host;
pub mod store;
pub mod telemetry;
pub mod stream;
pub mod tools;

pub use agent::{Agent, AgentConfig, EngineSink};
pub use approval::{ApprovalAnswer, ApprovalGate, ApprovalMode, ApprovalRequest};
pub use config::Config;
pub use event::{EngineEvent, ExecStream, FinishReason};
pub use provider::{
    ChatRequest, Delta, DeltaSink, Message, Provider, ProviderError, Reasoning, Role, ToolCall,
    ToolDef,
};
pub use prompt::PromptBuilder;
pub use host::{default_env_block, HostCmd, HostConfig, HostState, SessionHost};
pub use store::{SearchHit, Session, Store, StoreError, StoredMessage, Totals};
pub use telemetry::{LiveWindow, Rolling, TelemetrySnapshot, TurnTelemetry};
pub use tools::{ExecChunk, Registry, Risk, Tool, ToolCtx, ToolResult};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
