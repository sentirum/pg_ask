//! Tool registry.
//!
//! Tools are pure Rust types invoked by the agent loop between provider
//! turns. They run **inside the same PG backend and transaction** as the
//! caller, via SPI. Network-bound tools (web fetch, etc.) come in v0.4.

pub mod describe_table;
pub mod http_fetch;
pub mod recall;
pub mod sample_table;
pub mod sql_query;
pub mod user_defined;

use crate::infra::config::RuntimeConfig;
use crate::infra::errors::Result;
use crate::infra::http::HttpClient;
use crate::providers::ToolSpec;
use pgrx::prelude::*;

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
/// statement timeout, http allow-list, sensitive_columns) so tools never
/// read globals themselves.
///
/// `include_describe_table` is set by the agent when the schema render had
/// to fall back to compact mode — in that case the model needs a way to
/// pull column detail on demand. When the full schema fit in the prompt,
/// the extra tool is omitted to keep the function-call menu tight.
///
/// `include_memory` is set when the memory layer is enabled AND functional
/// (pgvector installed, embedding config present). The agent decides this
/// at the top of every call so a session that disables memory mid-flight
/// does the right thing on the next turn.
pub fn default_toolset(
    cfg: &RuntimeConfig,
    include_describe_table: bool,
    include_memory: bool,
    http: HttpClient,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(sql_query::SqlQueryTool {
        readonly: cfg.readonly,
        max_rows: cfg.tool_max_rows,
        statement_timeout_ms: cfg.tool_statement_timeout_ms,
        sensitive_columns: cfg.sensitive_columns.clone(),
    })];
    if include_describe_table {
        tools.push(Box::new(describe_table::DescribeTableTool));
    }
    if include_memory {
        tools.push(Box::new(recall::RecallTool));
    }
    if cfg.allow_http {
        tools.push(Box::new(http_fetch::HttpFetchTool {
            http,
            allow_list: cfg.http_allow_list.clone(),
        }));
    }
    // sample_table is always available — cheap, safe, and often the first
    // thing the model needs when exploring an unfamiliar schema.
    tools.push(Box::new(sample_table::SampleTableTool {
        readonly: cfg.readonly,
        max_rows: cfg.tool_max_rows,
        statement_timeout_ms: cfg.tool_statement_timeout_ms,
        sensitive_columns: cfg.sensitive_columns.clone(),
    }));
    // Append any user-defined tools registered by the caller.
    if let Ok(user_tools) = load_user_tools() {
        for t in user_tools {
            tools.push(Box::new(t));
        }
    }
    tools
}

/// Load user-defined tools from `ask._tools` for the current role.
/// Silently returns an empty vec on SPI failure so a broken _tools table
/// does not crash the agent loop.
pub fn load_user_tools() -> Result<Vec<user_defined::UserDefinedTool>> {
    let mut out: Vec<user_defined::UserDefinedTool> = Vec::new();

    Spi::connect(|client| -> Result<()> {
        let rows = client.select(
            "SELECT name, spec, body FROM ask._tools WHERE owner = current_user",
            None,
            &[],
        )?;
        for row in rows {
            let name: String = row
                .get_datum_by_ordinal(1)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();
            let spec_text: String = row
                .get_datum_by_ordinal(2)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();
            let body: String = row
                .get_datum_by_ordinal(3)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();

            if name.is_empty() || body.is_empty() {
                continue;
            }

            let spec_json: serde_json::Value =
                serde_json::from_str(&spec_text).unwrap_or(serde_json::json!({}));
            let description = spec_json
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = spec_json
                .get("input_schema")
                .cloned()
                .unwrap_or(serde_json::json!({"type":"object"}));

            out.push(user_defined::UserDefinedTool {
                name: name.clone(),
                body,
                spec: ToolSpec {
                    name,
                    description,
                    input_schema,
                },
            });
        }
        Ok(())
    })?;

    Ok(out)
}
