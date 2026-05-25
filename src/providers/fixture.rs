//! Deterministic, network-free provider for tests and CI.
//!
//! `provider = 'fixture'` reads a JSON script from disk and replays its
//! turns in order, one per `complete()` call. Nothing crosses the
//! network, nothing needs an API key, and every byte the agent sees is
//! authored by the test. This lets `cargo pgrx test` exercise the full
//! pipeline — agent loop, tool dispatch, sql_guard, SPI, telemetry —
//! against the same code path real users hit, without recording HTTP
//! fixtures or stubbing out internals.
//!
//! ## Wire
//!
//! The model is selected with `model = 'fixture:<scenario>'`. The
//! provider resolves `<scenario>` to
//! `<base_dir>/<scenario>.json`, where `base_dir` is `base_url` if set
//! (treated as a filesystem path), otherwise
//! `$CARGO_MANIFEST_DIR/tests/fixtures` baked in at compile time. The
//! JSON is an array of turns:
//!
//! ```json
//! [
//!   { "tool_calls": [
//!       { "id": "c1",
//!         "name": "sql_query",
//!         "arguments": { "sql": "SELECT count(*) FROM pg_class" } } ],
//!     "text": "let me check" },
//!   { "final": "there are 397 rows in pg_class" }
//! ]
//! ```
//!
//! Each call to `complete()` advances a backend-local cursor. When the
//! script runs out, we surface `Provider("fixture script exhausted")`
//! so a runaway test fails loudly instead of silently looping.
//!
//! ## Why not just mock the trait in Rust tests?
//!
//! Because the bug-hunting value of this provider is exactly the parts
//! the trait *doesn't* cover: how the agent stitches tool results into
//! the next prompt, how `_traces` records the run, how the sql_guard
//! rejects what the model emits, how the SRF streams turn boundaries.
//! All of that lives behind `#[pg_extern]` and only runs inside a real
//! backend. A fixture provider keeps the boundary in one place — the
//! HTTP edge — and tests everything inside it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::providers::wire::{Message, ProviderResponse, ToolCall, ToolSpec};
use crate::providers::Provider;

/// One scripted turn. Either the model is done (`final`) or it wants to
/// call tools before continuing (`tool_calls`, optional `text` reasoning).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FixtureTurn {
    Final {
        #[serde(rename = "final")]
        final_text: String,
    },
    ToolCalls {
        tool_calls: Vec<ToolCall>,
        #[serde(default)]
        text: Option<String>,
    },
}

pub struct FixtureProvider {
    scenario: String,
    base_dir: PathBuf,
}

impl FixtureProvider {
    pub fn new(cfg: &RuntimeConfig) -> Result<Self> {
        let model = cfg.model.as_deref().unwrap_or("");
        let scenario = model.strip_prefix("fixture:").ok_or_else(|| {
            AskError::InvalidConfig {
                key: "model",
                message: format!(
                    "fixture provider expects `model = 'fixture:<scenario>'`, got `{model}`"
                ),
            }
        })?;

        // `base_url` is reused as a filesystem path here. Default points
        // at the repo's own fixtures directory so `cargo test` works
        // out of the box.
        let base_dir = cfg
            .base_url
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
            });

        Ok(Self {
            scenario: scenario.to_string(),
            base_dir,
        })
    }

    fn load(&self) -> Result<Vec<FixtureTurn>> {
        let path = self.base_dir.join(format!("{}.json", self.scenario));
        let bytes = std::fs::read(&path).map_err(|e| {
            AskError::Transport(format!(
                "fixture: cannot read {}: {e}",
                path.display()
            ))
        })?;
        serde_json::from_slice::<Vec<FixtureTurn>>(&bytes).map_err(|e| {
            AskError::Transport(format!(
                "fixture: cannot parse {} as a turn array: {e}",
                path.display()
            ))
        })
    }
}

// Per-scenario call cursor, scoped to the current backend. Two
// concurrent backends each get their own counter; sequential calls
// inside one backend share it.
thread_local! {
    static CURSORS: RefCell<HashMap<String, usize>> = RefCell::new(HashMap::new());
}

/// Test-only hook to rewind a scenario between assertions. Not exposed
/// over SQL on purpose — leaks pg-internal types and is only useful
/// from Rust unit tests living in the same crate.
#[cfg(any(test, feature = "pg_test"))]
pub fn reset_cursor(scenario: &str) {
    CURSORS.with(|c| {
        c.borrow_mut().remove(scenario);
    });
}

impl Provider for FixtureProvider {
    fn complete(
        &self,
        _system: &str,
        _history: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ProviderResponse> {
        let script = self.load()?;

        let idx = CURSORS.with(|c| {
            let mut map = c.borrow_mut();
            let slot = map.entry(self.scenario.clone()).or_insert(0);
            let i = *slot;
            *slot += 1;
            i
        });

        let turn = script.get(idx).cloned().ok_or_else(|| {
            AskError::Transport(format!(
                "fixture: scenario `{}` exhausted at call #{idx} ({} turns available)",
                self.scenario,
                script.len()
            ))
        })?;

        Ok(match turn {
            FixtureTurn::Final { final_text } => ProviderResponse::Final { text: final_text },
            FixtureTurn::ToolCalls { tool_calls, text } => ProviderResponse::ToolCalls {
                text,
                calls: tool_calls,
            },
        })
    }
}
