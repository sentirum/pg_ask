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
//! `Config::new_agent()` allocates a new connection pool — with a tight
//! agent loop hitting the same provider, the previous behaviour
//! re-resolved DNS and re-handshook TLS for every call. The cache makes
//! the pool persistent for the lifetime of the backend.
//!
//! `max_body_bytes` is NOT part of the cache key: it's a Rust-side
//! cap applied after the agent has handed us a reader, so two clients
//! with different caps can safely share the same Agent.
//!
//! ## ureq 3.x migration (Wave 4)
//!
//! v0.5.3 moved this module from ureq 2.x to ureq 3.x. The public
//! `HttpClient` surface is unchanged so every provider and tool
//! compiles untouched. Internally:
//!
//! * `AgentBuilder` → `Agent::config_builder()` / `Config`.
//! * `agent.get(...).call()` now returns
//!   `http::Response<ureq::Body>`; we read the body via
//!   `body_mut().read_to_string()` and feed our own byte cap on top.
//! * Non-2xx responses surface as `Error::StatusCode(code)` instead of
//!   the old `Error::Status(code, resp)` two-tuple. The new variant
//!   doesn't carry the response, so to recover the body / Location we
//!   call `http_status_as_error(false)` on the config and inspect the
//!   `Response` directly. That's a deliberate choice for HTTP-fetch
//!   where the caller needs the redirect target; provider calls keep
//!   the default and map 4xx/5xx onto `AskError::ProviderHttp`.

use crate::infra::errors::{AskError, Result};
use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

use ureq::config::Config;
use ureq::http::Response as HttpResponse;
use ureq::tls::{TlsConfig, TlsProvider};
use ureq::{Agent, Body, Error as UreqError};

#[derive(Clone)]
pub struct HttpClient {
    agent: Agent,
    /// Hard cap on bytes read from any response body. Applies to both
    /// `get_text_with_status` (model-facing) and `post_json` (provider
    /// responses). Set to `usize::MAX` to disable. We use a generous
    /// default (8 MiB) because provider responses can include large
    /// completions; the http_fetch tool layers its own much smaller
    /// cap on top (see MAX_BODY_BYTES there).
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
        // ureq 3.x: build the request, attach headers, send. The
        // typestate builder makes this slightly more verbose than
        // 2.x but the call shape is the same.
        let mut req = self.agent.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        match req.call() {
            Ok(resp) => Ok(extract_status_body_location(resp, self.max_body_bytes)),
            // 3.x: `StatusCode` no longer carries the response. For
            // get-with-status the caller wants the body / Location too,
            // so any non-2xx is opaque under the default policy. The
            // shared agent disables `http_status_as_error` so we get
            // the response in the Ok arm instead — the only Err here
            // is genuine transport / protocol failure.
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
        let mut req = self
            .agent
            .post(url)
            .header("content-type", "application/json");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }

        let resp = match req.send_json(body) {
            Ok(r) => r,
            Err(UreqError::StatusCode(status)) => {
                // ureq 3.x drops the response from StatusCode errors;
                // we surface just the code. Pre-3.x we propagated the
                // body too, which the agent loop sometimes echoed to
                // the model. The new shape is strictly less leaky for
                // provider responses (which may include diagnostic
                // payloads we don't want in tool output).
                return Err(AskError::ProviderHttp {
                    status,
                    body: String::new(),
                });
            }
            Err(e) => return Err(AskError::Transport(e.to_string())),
        };

        let (status, body, _loc) = extract_status_body_location(resp, self.max_body_bytes);
        if !(200..300).contains(&status) {
            return Err(AskError::ProviderHttp { status, body });
        }
        serde_json::from_str::<T>(&body).map_err(|e| AskError::Transport(e.to_string()))
    }
}

/// Common shape: pull status, capped body, and Location header out of
/// an ureq 3.x response. Centralised so the redirect-aware GET and
/// the JSON POST share one parser.
fn extract_status_body_location(
    mut resp: HttpResponse<Body>,
    max_body_bytes: usize,
) -> (u16, String, Option<String>) {
    let status = resp.status().as_u16();
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // ureq 3.x exposes the body via `body_mut()`. We want our own
    // byte cap rather than `read_to_string` because providers can
    // legitimately exceed the default cap; the cap is configurable
    // per-client via `HttpClient::with_options`.
    let reader = resp.body_mut().as_reader();
    let body = read_capped(reader, max_body_bytes);
    (status, body, location)
}

/// Process-level Agent cache. Each Postgres backend is its own process
/// (Postgres forks per connection), so the cache is per-backend in
/// practice. The key is `(connect_ms, total_ms, follow_redirects)`;
/// every distinct combination gets its own pool, but in the typical
/// pg_ask deployment there are at most two entries: one for providers
/// (follow redirects) and one for `http_fetch` (no redirects).
fn shared_agent(connect_timeout_ms: u64, total_timeout_ms: u64, follow_redirects: bool) -> Agent {
    type Key = (u64, u64, bool);
    static POOL: Mutex<Vec<(Key, Agent)>> = Mutex::new(Vec::new());

    let key: Key = (connect_timeout_ms, total_timeout_ms, follow_redirects);
    let mut pool = POOL.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((_, agent)) = pool.iter().find(|(k, _)| *k == key) {
        return agent.clone();
    }

    let config = Config::builder()
        .timeout_connect(Some(Duration::from_millis(connect_timeout_ms)))
        .timeout_global(Some(Duration::from_millis(total_timeout_ms)))
        // 3.x splits "follow N redirects" from "treat 3xx as ok".
        // The http_fetch tool wants the response (with Location) on
        // 3xx; everyone else wants the agent to chase.
        .max_redirects(if follow_redirects { 5 } else { 0 })
        // Surface status as `Ok(response)` for both 2xx and 3xx.
        // Errors are reserved for genuine transport failures. This
        // matches the 2.x behaviour we relied on in http_fetch.
        .http_status_as_error(false)
        .tls_config(
            TlsConfig::builder()
                // Pin rustls explicitly so a `native-tls` cargo
                // feature on a downstream consumer doesn't flip the
                // backend. Matches the 2.x `tls` feature gate.
                .provider(TlsProvider::Rustls)
                .build(),
        )
        .build();
    let agent: Agent = config.into();
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
