//! Tool registry.
//!
//! Tools are pure Rust closures invoked by the agent loop between provider
//! turns. They run **inside the same PG backend and transaction** as the
//! caller, via SPI. Network-bound tools (web fetch, etc.) come later.

use crate::error::Result;
use crate::providers::ToolSpec;

pub mod sql_query;

/// Result of executing a single tool call.
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

/// A registered tool the agent can invoke.
pub trait Tool {
    fn spec(&self) -> ToolSpec;
    fn invoke(&self, args: &serde_json::Value) -> Result<ToolOutput>;
}

/// Standard toolset for the `ask` entry point.
///
/// The readonly flag is honoured per-tool — `sql_query` refuses writes when set.
pub fn default_toolset(readonly: bool) -> Vec<Box<dyn Tool>> {
    vec![Box::new(sql_query::SqlQueryTool { readonly })]
}
