//! Agent orchestration.
//!
//! The agent layer is pure orchestration over the [`Provider`] and [`Tool`]
//! traits. It does not call SPI directly, does not speak HTTP, does not
//! build SQL strings. Swapping providers, registering new tools, and
//! changing prompts all happen *outside* this module.

mod dispatch;
mod prompt;
mod run;
pub mod stream;

// Note: `run` / `run_with_history` are retained as convenience entry
// points for non-API callers (background workers, tests). The public
// `ask.*` SQL surface goes through `run_with_cfg` so the runtime
// snapshot is loaded exactly once per call (P1, v0.5.2 review).
#[allow(unused_imports)]
pub use run::{run, run_with_history};
pub use run::{run_with_cfg, AgentMode};
#[allow(unused_imports)] // consumed by telemetry writer once it lands
pub use run::AgentOutcome;
#[allow(unused_imports)]
pub use stream::run_stream;
pub use stream::run_stream_with_cfg;
