//! `http_fetch` tool — fetch a public URL and return the body as text.
//!
//! Gated by two knobs:
//!   1. `pg_ask.allow_http` — master switch (default `false`).
//!   2. `pg_ask.http_allow_list` — comma-separated URL prefix allow-list.
//!      Empty means "deny everything".
//!
//! Both checks happen at invoke time (defence-in-depth: the tool is not
//! registered when `allow_http` is off, but we also check inside invoke
//! in case the GUC flips mid-session).
//!
//! The response body is truncated to `MAX_BODY_CHARS` so the model
//! doesn't get DOS'd by a multi-megabyte JSON blob. If the body parses
//! as JSON we pretty-print it so the model sees structure; otherwise we
//! return raw text.

use super::{Tool, ToolOutput};
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;
use crate::providers::ToolSpec;
use serde_json::json;

const MAX_BODY_CHARS: usize = 8_000;

pub struct HttpFetchTool {
    pub http: HttpClient,
    pub allow_list: Vec<String>,
}

impl Tool for HttpFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "http_fetch".to_string(),
            description: "Fetch a URL via HTTP GET and return the response body. \
                Use this for public API endpoints, documentation, or reference data. \
                Only URLs matching the operator allow-list are permitted.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute URL to fetch (https://...)."
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn invoke(&self, args: &serde_json::Value) -> Result<ToolOutput> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AskError::Tool {
                name: "http_fetch".to_string(),
                message: "missing required argument `url`".into(),
            })?;

        if !is_allowed(url, &self.allow_list) {
            return Ok(ToolOutput {
                text: format!("URL not in allow-list: {url}"),
                is_error: true,
            });
        }

        let body = match self.http.get_text(url, &[]) {
            Ok(b) => b,
            Err(AskError::ProviderHttp { status, body: b }) => {
                return Ok(ToolOutput {
                    text: format!("HTTP {status}: {b}"),
                    is_error: true,
                });
            }
            Err(e) => {
                return Ok(ToolOutput {
                    text: format!("fetch failed: {e}"),
                    is_error: true,
                });
            }
        };

        // Try to pretty-print JSON so the model sees structure; fall back
        // to raw text on parse failure.
        let display = if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| body.clone())
        } else {
            body
        };

        let truncated = if display.chars().count() > MAX_BODY_CHARS {
            let cut: String = display.chars().take(MAX_BODY_CHARS).collect();
            format!("{cut}… [truncated]")
        } else {
            display
        };

        Ok(ToolOutput {
            text: truncated,
            is_error: false,
        })
    }
}

fn is_allowed(url: &str, allow_list: &[String]) -> bool {
    if allow_list.is_empty() {
        return false;
    }
    allow_list.iter().any(|prefix| url.starts_with(prefix))
}


