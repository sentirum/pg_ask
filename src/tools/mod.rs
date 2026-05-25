//! Tool registry.
//!
//! Tools are pure Rust types invoked by the agent loop between provider
//! turns. They run **inside the same PG backend and transaction** as the
//! caller, via SPI. Network-bound tools (web fetch, etc.) come in v0.4.

pub mod describe_table;
pub mod sql_query;

use crate::infra::config::RuntimeConfig;
use crate::infra::errors::Result;
use crate::providers::ToolSpec;

/// Result of executing a single tool call. Errors that the model should
/// be allowed to see and recover from are reported as `is_error = true`
/// rather than `Err(...)`. `Err` is reserved for harness-level failures
/// (e.g. a bug in the tool itself).
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
/// The runtime config drives every per-tool knob (readonly, row cap,
/// statement timeout) so tools never read globals themselves.
///
/// `include_describe_table` is set by the agent when the schema render had
/// to fall back to compact mode — in that case the model needs a way to
/// pull column detail on demand. When the full schema fit in the prompt,
/// the extra tool is omitted to keep the function-call menu tight.
pub fn default_toolset(cfg: &RuntimeConfig, include_describe_table: bool) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(sql_query::SqlQueryTool {
        readonly: cfg.readonly,
        max_rows: cfg.tool_max_rows,
        statement_timeout_ms: cfg.tool_statement_timeout_ms,
    })];
    if include_describe_table {
        tools.push(Box::new(describe_table::DescribeTableTool));
    }
    tools
}
