//! Shared HTTP client.
//!
//! All provider implementations get a [`HttpClient`] handle and use it for
//! their network calls. Centralising this gives us a single place to enforce
//! connect / total timeouts (critical when running inside a Postgres backend:
//! a hung HTTP call holds locks and stops cooperative cancellation from
//! kicking in until we return).
//!
//! P4 (v0.5.2 review): we keep a process-level cache of `ureq::Agent`s
//! keyed on (connect_timeout, total_timeout, redirects). `ureq::Agent`
//! is internally an `Arc` so cloning is free, but each fresh
//! `AgentBuilder::build()` allocates a new connection pool — with a
//! tight agent loop hitting the same provider, the previous behaviour
//! re-resolved DNS and re-handshook TLS for every call. The cache
//! makes the pool persistent for the lifetime of the backend.
//!
//! `max_body_bytes` is NOT part of the cache key: it's a Rust-side
//! cap applied after the agent has handed us a reader, so two clients
//! with different caps can safely share the same Agent.

use crate::infra::errors::{AskError, Result};
use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone)]
pub struct HttpClient {
    agent: ureq::Agent,
    /// Hard cap on bytes read from any response body. Applies to both
    /// `get_text` (model-facing) and `post_json` (provider responses).
    /// Set to `usize::MAX` to disable. We use a generous default (8 MiB)
    /// because provider responses can include large completions; the
    /// http_fetch tool layers its own much smaller cap on top (see
    /// MAX_BODY_BYTES there).
    max_body_bytes: usize,
}

impl HttpClient {
    pub fn new(connect_timeout_ms: u64, total_timeout_ms: u64) -> Self {
        Self::with_options(connect_timeout_ms, total_timeout_ms, 8 * 1024 * 1024, true)
    }

    /// Build with explicit body cap and redirect policy. Used by the
    /// `http_fetch` tool (C5 SSRF fix): allowing ureq's default redirect
    /// behaviour means a whitelisted endpoint can 302 to an attacker
    /// host that we never re-validate. The tool turns redirects off and
    /// surfaces 3xx responses to the model, which can then issue a
    /// fresh fetch against the new URL — which goes through the
    /// allow-list check again.
    pub fn with_options(
        connect_timeout_ms: u64,
        total_timeout_ms: u64,
        max_body_bytes: usize,
        follow_redirects: bool,
    ) -> Self {
        let agent = shared_agent(connect_timeout_ms, total_timeout_ms, follow_redirects);
        Self {
            agent,
            max_body_bytes,
        }
    }

    /// GET a URL and return (status, body, optional Location header).
    /// Used by `http_fetch` so 3xx responses can be surfaced to the
    /// model rather than auto-followed (C5 SSRF).
    ///
    /// We don't have a `get_text` convenience anymore — every caller we
    /// have today wants the status code, and a status-less helper made
    /// it too easy to misuse for non-2xx responses (the old code
    /// silently swallowed redirects).
    #[allow(dead_code)] // construction tested via http_fetch unit/pg_test
    pub fn get_text_with_status(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<(u16, String, Option<String>)> {
        let mut req = self.agent.get(url);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        match req.call() {
            Ok(r) => {
                let status = r.status();
                let location = r.header("location").map(|s| s.to_string());
                let body = read_capped(r.into_reader(), self.max_body_bytes);
                Ok((status, body, location))
            }
            Err(ureq::Error::Status(status, r)) => {
                // `redirects(0)` raises Status(3xx, _) instead of
                // chasing; pluck the Location header so the caller can
                // re-validate it against the allow-list.
                let location = r.header("location").map(|s| s.to_string());
                let body = read_capped(r.into_reader(), self.max_body_bytes);
                Ok((status, body, location))
            }
            Err(e) => Err(AskError::Transport(e.to_string())),
        }
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
                let body = read_capped(r.into_reader(), self.max_body_bytes);
                return Err(AskError::ProviderHttp { status, body });
            }
            Err(e) => return Err(AskError::Transport(e.to_string())),
        };

        // We cap the body before parsing to JSON: a malicious or
        // misbehaving provider that returns gigabytes of garbage would
        // otherwise hang the backend in `into_string`. Real responses
        // are well under the 8 MiB default.
        let body = read_capped(resp.into_reader(), self.max_body_bytes);
        serde_json::from_str::<T>(&body).map_err(|e| AskError::Transport(e.to_string()))
    }
}

/// Process-level Agent cache. Each Postgres backend is its own process
/// (Postgres forks per connection), so the cache is per-backend in
/// practice. The key is `(connect_ms, total_ms, follow_redirects)`;
/// every distinct combination gets its own pool, but in the typical
/// pg_ask deployment there are at most two entries: one for providers
/// (follow redirects) and one for `http_fetch` (no redirects).
fn shared_agent(
    connect_timeout_ms: u64,
    total_timeout_ms: u64,
    follow_redirects: bool,
) -> ureq::Agent {
    type Key = (u64, u64, bool);
    static POOL: Mutex<Vec<(Key, ureq::Agent)>> = Mutex::new(Vec::new());

    let key: Key = (connect_timeout_ms, total_timeout_ms, follow_redirects);
    let mut pool = POOL.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((_, agent)) = pool.iter().find(|(k, _)| *k == key) {
        return agent.clone();
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(connect_timeout_ms))
        .timeout(Duration::from_millis(total_timeout_ms))
        .redirects(if follow_redirects { 5 } else { 0 })
        .build();
    pool.push((key, agent.clone()));
    agent
}

/// Read up to `max` bytes from `r`, decode as UTF-8 lossily, and return.
/// Avoids `Read::take` because we'd lose the ability to distinguish a
/// reader that returned exactly `max` bytes (legitimate) from one that
/// hit the cap; in practice we don't surface that distinction yet but
/// the explicit byte-by-byte loop keeps the door open.
fn read_capped<R: Read>(mut r: R, max: usize) -> String {
    let mut buf: Vec<u8> = Vec::with_capacity(4096.min(max));
    let mut chunk = [0u8; 4096];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = max.saturating_sub(buf.len());
                if room == 0 {
                    break;
                }
                let take = n.min(room);
                buf.extend_from_slice(&chunk[..take]);
                if take < n {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}
