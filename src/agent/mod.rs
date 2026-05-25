//! Agent orchestration.
//!
//! The agent layer is pure orchestration over the [`Provider`] and [`Tool`]
//! traits. It does not call SPI directly, does not speak HTTP, does not
//! build SQL strings. Swapping providers, registering new tools, and
//! changing prompts all happen *outside* this module.

mod dispatch;
mod prompt;
mod run;

#[allow(unused_imports)]
pub use run::{run, AgentMode, AgentOutcome};
