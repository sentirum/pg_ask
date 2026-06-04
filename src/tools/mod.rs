//! Tool registry.
//!
//! Tools are pure Rust types invoked by the agent loop between provider
//! turns. They run **inside the same PG backend and transaction** as the
//! caller, via SPI. Network-bound tools (web fetch, etc.) come in v0.4.

pub mod describe_table;
pub mod http_fetch;
pub mod recall;
pub mod render;
pub mod sample_table;
pub mod sql_query;
pub mod user_defined;

use crate::infra::config::RuntimeConfig;
use crate::infra::errors::Result;
use crate::infra::http::HttpClient;
use crate::providers::ToolSpec;
use pgrx::prelude::*;
use std::cell::RefCell;
use std::time::Instant;

/// P3 fix: per-backend TTL cache for user-defined tools.
/// Avoids a SPI round-trip to ask._tools on every ask() call.
const TOOL_CACHE_TTL_SECS: u64 = 5;

thread_local! {
    static TOOL_CACHE: RefCell<Option<(std::string::String, Instant, Vec<user_defined::UserDefinedTool>)>> = RefCell::new(None);
}

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
        tools.push(Box::new(recall::RecallTool { cfg: cfg.clone() }));
    }
    if cfg.allow_http {
        // We deliberately do NOT reuse the provider HttpClient here:
        // http_fetch needs redirects(0) + tighter body cap (C5 SSRF
        // fix), which the provider client doesn't want. The `http`
        // parameter remains in the signature for future tools that
        // legitimately want the shared agent.
        let _ = &http;
        tools.push(Box::new(http_fetch::HttpFetchTool::new(
            cfg.http_connect_timeout_ms,
            cfg.http_total_timeout_ms,
            cfg.http_allow_list.clone(),
            cfg.allow_private_hosts,
        )));
    }
    // sample_table is always available — cheap, safe, and often the first
    // thing the model needs when exploring an unfamiliar schema.
    tools.push(Box::new(sample_table::SampleTableTool {
        readonly: cfg.readonly,
        max_rows: cfg.tool_max_rows,
        statement_timeout_ms: cfg.tool_statement_timeout_ms,
        sensitive_columns: cfg.sensitive_columns.clone(),
    }));
    // Append any user-defined tools registered by the caller. HP1
    // (Gemini v0.5.2 review item 1.2): each tool carries the same
    // readonly + statement_timeout snapshot as the built-ins, so the
    // subtxn that runs the body honours them.
    if let Ok(user_tools) = load_user_tools(cfg.readonly, cfg.tool_statement_timeout_ms) {
        for t in user_tools {
            tools.push(Box::new(t));
        }
    }
    tools
}

/// Load user-defined tools from `ask._tools` for the current role.
/// Silently returns an empty vec on SPI failure so a broken _tools table
/// does not crash the agent loop.
///
/// P3 fix: results are cached per-backend for 5 seconds (TTL). The cache
/// key includes the current user so `SET ROLE` doesn't leak cached tools
/// across roles. On cache miss the SPI query runs as before.
///
/// `readonly` and `statement_timeout_ms` are baked into every returned
/// `UserDefinedTool` so HP1 (subtxn + per-call GUCs) has the policy
/// it needs without a second config load per tool invocation. Threaded
/// from the surrounding `RuntimeConfig` snapshot at the caller.
pub fn load_user_tools(
    readonly: bool,
    statement_timeout_ms: u64,
) -> Result<Vec<user_defined::UserDefinedTool>> {
    let current_user: String = Spi::get_one("SELECT current_user")
        .ok().flatten()
        .unwrap_or_default();

    // Check the TTL cache first.
    let cache_hit = TOOL_CACHE.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|(user, ts, tools)| {
                if *user == current_user && ts.elapsed().as_secs() < TOOL_CACHE_TTL_SECS {
                    Some(tools.clone())
                } else {
                    None
                }
            })
    });

    if let Some(cached) = cache_hit {
        return Ok(cached.clone());
    }

    let tools = load_user_tools_from_spi(readonly, statement_timeout_ms)?;

    // Populate cache.
    TOOL_CACHE.with(|c| {
        *c.borrow_mut() = Some((current_user, Instant::now(), tools.clone()));
    });

    Ok(tools)
}

fn load_user_tools_from_spi(
    readonly: bool,
    statement_timeout_ms: u64,
) -> Result<Vec<user_defined::UserDefinedTool>> {
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
                readonly,
                statement_timeout_ms,
            });
        }
        Ok(())
    })?;

    Ok(out)
}
