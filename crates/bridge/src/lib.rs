//! `cwoffee-bridge` — pi's RPC protocol behind cwoffee's own core traits.
//!
//! Four files, and each one answers a different question:
//!
//! - `process.rs` — how one `pi --mode rpc` child is started, framed, talked to,
//!   and made sure it does not outlive us.
//! - `translate.rs` — how pi's stream records become cwoffee's `EngineEvent`
//!   vocabulary, and why pi's "turn" is not cwoffee's.
//! - `host.rs` — the `HostOps` implementation: addressing, approvals, session
//!   switching, the idle reaper.
//! - `sessions.rs` — which pi session file belongs to which cwoffee conversation.
//!
//! Nothing here is a shim over pi's own semantics: the approvals, the transcript,
//! the addressing rules, and the failure behaviour are cwoffee's, and a front end
//! cannot tell which core it is talking to.

pub mod host;
pub mod mount;
pub mod process;
pub mod sessions;
pub mod translate;

pub use host::{EventRx, PiHost, PiHostConfig};
pub use mount::{mount_extensions, pi_node_modules, write_dev_provider};
pub use process::{PiChild, PiError, PiLaunch};
pub use sessions::SessionMap;
pub use translate::{Translator, UiRequest};
