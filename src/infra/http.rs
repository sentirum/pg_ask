//! Shared HTTP client.
//!
//! All provider implementations get a [`HttpClient`] handle and use it for
//! their network calls. Centralising this gives us a single place to enforce
//! connect / total timeouts (critical when running inside a Postgres backend:
//! a hung HTTP call holds locks and stops cooperative cancellation from
//! kicking in until we return).
//!
//! Built per request (cheap — `ureq::Agent` is an `Arc` internally) so timeout
//! changes via GUC take effect immediately.

use crate::infra::errors::{AskError, Result};
use std::time::Duration;

#[derive(Clone)]
pub struct HttpClient {
    agent: ureq::Agent,
}

impl HttpClient {
    pub fn new(connect_timeout_ms: u64, total_timeout_ms: u64) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(connect_timeout_ms))
            .timeout(Duration::from_millis(total_timeout_ms))
            .build();
        Self { agent }
    }

    /// POST JSON, expect JSON back. Maps ureq error variants onto our
    /// [`AskError`] taxonomy so providers can stay terse.
    pub fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &serde_json::Value,
    ) -> Result<T> {
        let mut req = self.agent.post(url).set("content-type", "application/json");
        for (k, v) in headers {
            req = req.set(k, v);
        }

        let resp = req.send_json(body);
        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(status, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(AskError::ProviderHttp { status, body });
            }
            Err(e) => return Err(AskError::Transport(e.to_string())),
        };

        resp.into_json::<T>()
            .map_err(|e| AskError::Transport(e.to_string()))
    }
}
