//! Tool dispatch.
//!
//! Looks up a tool by name and invokes it, converting any harness-level
//! error into a model-visible `is_error` ToolOutput. Real bugs in the
//! tool (e.g. invalid JSON in the spec) bubble up as `Err`.

use crate::tools::{Tool, ToolOutput};

pub fn dispatch(tools: &[Box<dyn Tool>], name: &str, args: &serde_json::Value) -> ToolOutput {
    let Some(tool) = tools.iter().find(|t| t.spec().name == name) else {
        return ToolOutput {
            text: format!("unknown tool `{name}`"),
            is_error: true,
        };
    };

    match tool.invoke(args) {
        Ok(out) => out,
        Err(e) => ToolOutput {
            text: format!("tool error: {e}"),
            is_error: true,
        },
    }
}
