//! `recall` tool — exposes the memory layer to the agent.
//!
//! Surfaced to the model only when `pg_ask.memory_enabled = on` AND
//! pgvector is installed (checked at toolset-construction time). The
//! model can then call `recall` between turns to retrieve previously-
//! stored context: user preferences, prior conclusions, glossaries, etc.
//!
//! The tool name is intentionally short and verb-shaped (`recall`, not
//! `memory_search`) — short tool names route better through every
//! provider's function-calling pipeline.

use super::{Tool, ToolOutput};
use crate::infra::config::RuntimeConfig;
use crate::infra::errors::Result;
use crate::memory;
use crate::providers::ToolSpec;
use serde_json::json;

/// How many hits to fetch when the model doesn't supply a `limit`. Five
/// covers typical "remind me what we said about X" use cases without
/// blowing the prompt budget.
const DEFAULT_LIMIT: usize = 5;
/// Hard ceiling regardless of what the model asks for.
const MAX_LIMIT: usize = 25;

/// Recall tool. Holds a snapshot of the runtime config so each
/// `invoke()` call doesn't re-read the GUCs + `_config` table. P1
/// (v0.5.2 review).
pub struct RecallTool {
    pub cfg: RuntimeConfig,
}

impl Tool for RecallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "recall".to_string(),
            description: "Search the user's long-term memory for previously stored \
                notes, preferences, or facts relevant to a query. Use this to look \
                up context that may not be in the current conversation — user \
                preferences, definitions, prior conclusions. Returns up to 25 hits \
                ranked by hybrid (vector + full-text) similarity."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query":     { "type": "string",
                                   "description": "Free-text query." },
                    "namespace": { "type": "string",
                                   "description": "Optional namespace (default 'default')." },
                    "limit":     { "type": "integer",
                                   "minimum": 1, "maximum": 25,
                                   "description": "Max hits to return (default 5)." }
                },
                "required": ["query"]
            }),
        }
    }

    fn invoke(&self, args: &serde_json::Value) -> Result<ToolOutput> {
        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.trim(),
            _ => return Ok(err("missing required argument `query`")),
        };
        let namespace = args
            .get("namespace")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);

        let hits = match memory::recall_with_cfg(&self.cfg, query, namespace, limit) {
            Ok(h) => h,
            Err(e) => return Ok(err(&format!("recall failed: {e}"))),
        };

        if hits.is_empty() {
            return Ok(ok(format!("(no hits for `{query}`)")));
        }

        let mut out = String::new();
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "[{i}] score={:.3}  id={}\n  {}\n",
                h.similarity,
                h.id,
                truncate(&h.content, 400),
            ));
        }
        Ok(ok(out))
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}

fn ok(text: String) -> ToolOutput {
    ToolOutput {
        text,
        is_error: false,
    }
}
fn err(msg: &str) -> ToolOutput {
    ToolOutput {
        text: msg.to_string(),
        is_error: true,
    }
}
