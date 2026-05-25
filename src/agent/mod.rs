//! Agent orchestration.
//!
//! The agent layer is pure orchestration over the [`Provider`] and [`Tool`]
//! traits. It does not call SPI directly, does not speak HTTP, does not
//! build SQL strings. Swapping providers, registering new tools, and
//! changing prompts all happen *outside* this module.

mod dispatch;
mod prompt;
mod run;

pub use run::{run, run_with_history, AgentMode};
#[allow(unused_imports)] // consumed by telemetry writer once it lands
pub use run::AgentOutcome;
