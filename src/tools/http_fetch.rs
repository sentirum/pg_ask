//! `http_fetch` tool — fetch a public URL and return the body as text.
//!
//! ## Security model (C5 in the v0.5.2 review)
//!
//! Three independent layers gate every fetch:
//!
//! 1. **Master switch** — `pg_ask.allow_http` GUC (default `false`).
//!    When off, the tool is never even registered with the agent, so
//!    the model can't see it.
//! 2. **Allow-list** — `pg_ask.http_allow_list` is a comma-separated
//!    set of entries. Pre-v0.5.2 entries were treated as raw URL
//!    prefixes and matched with `str::starts_with`, which meant
//!    `https://api.openai.com.evil.com/x` matched an allow entry of
//!    `https://api.openai.com`. We now parse both the request URL and
//!    each allow entry with the `url` crate and compare **host** (plus
//!    optional path prefix). See `EntryMatcher` below.
//! 3. **No redirect following** — ureq is built with `redirects(0)` so
//!    the underlying agent never silently chases a 3xx into an
//!    attacker-controlled host. We surface the 3xx + Location header
//!    back to the model, which can issue a fresh fetch against the new
//!    URL — that fresh fetch goes through the allow-list check from
//!    the top.
//!
//! ## Private-network defence
//!
//! Even with a strict allow-list, an operator could entry the wrong
//! thing and expose a private endpoint. We additionally:
//!
//! * Reject `file://`, `ftp://`, anything that isn't HTTP(S).
//! * Reject literal-IP hosts in private / loopback / link-local /
//!   reserved CIDR ranges, regardless of allow-list (the operator can
//!   override by setting `pg_ask.allow_private_hosts = on` — useful
//!   for self-hosted setups talking to internal services).
//!
//! ## Body cap (H7 bonus)
//!
//! Body is read with a hard byte cap inside `HttpClient`. We additionally
//! cap the *displayed* body to `MAX_BODY_CHARS` to keep the model's
//! context window predictable.

use super::{Tool, ToolOutput};
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;
use crate::providers::ToolSpec;
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use url::Url;

/// Char cap on the body shown to the model. The byte cap lives in
/// `HttpClient::max_body_bytes`; this is the second-line readability cap.
const MAX_BODY_CHARS: usize = 8_000;

/// Hard byte cap fed to the HttpClient builder. 1 MiB is plenty for any
/// reasonable JSON/text response and bounds memory if a server lies
/// about its content length.
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub struct HttpFetchTool {
    /// Pre-built client with `redirects(0)` and body cap. Don't reuse
    /// the shared HttpClient — the provider one chases redirects.
    pub http: HttpClient,
    pub allow_list: Vec<String>,
    /// When true, skip the private/loopback IP guard. For self-hosted
    /// scenarios where the model legitimately needs to call an internal
    /// service. Defaults to false.
    pub allow_private_hosts: bool,
}

impl HttpFetchTool {
    /// Build the tool with a hardened HttpClient (no redirects, capped
    /// body). Connect / total timeouts come from the surrounding config
    /// just like the provider client.
    pub fn new(
        connect_timeout_ms: u64,
        total_timeout_ms: u64,
        allow_list: Vec<String>,
        allow_private_hosts: bool,
    ) -> Self {
        Self {
            http: HttpClient::with_options(
                connect_timeout_ms,
                total_timeout_ms,
                MAX_BODY_BYTES,
                false, // no redirects — surface to model instead
            ),
            allow_list,
            allow_private_hosts,
        }
    }
}

impl Tool for HttpFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "http_fetch".to_string(),
            description: "Fetch a URL via HTTP GET and return the response body. \
                Use this for public API endpoints, documentation, or reference data. \
                Only URLs matching the operator allow-list are permitted. Redirects \
                are not followed automatically — if a 3xx is returned, re-issue \
                the fetch against the Location header (which is also checked)."
                .to_string(),
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

        if let Err(msg) = validate_url(url, &self.allow_list, self.allow_private_hosts) {
            return Ok(ToolOutput {
                text: format!("URL rejected: {msg}"),
                is_error: true,
            });
        }

        let (status, body, location) = match self.http.get_text_with_status(url, &[]) {
            Ok(t) => t,
            Err(AskError::ProviderHttp { status, body }) => {
                return Ok(ToolOutput {
                    text: format!("HTTP {status}: {body}"),
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

        // Surface 3xx to the model rather than chasing automatically.
        if (300..400).contains(&status) {
            let loc = location.unwrap_or_else(|| "<no Location header>".to_string());
            return Ok(ToolOutput {
                text: format!(
                    "HTTP {status} redirect to: {loc}\n\
                     (Redirects are not auto-followed. If you trust this destination, \
                     call http_fetch again with the new URL — it will be re-validated \
                     against the allow-list.)"
                ),
                is_error: false,
            });
        }

        if !(200..300).contains(&status) {
            return Ok(ToolOutput {
                text: format!("HTTP {status}: {body}"),
                is_error: true,
            });
        }

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

/// Validate a URL against scheme / host / allow-list / private-net rules.
/// Returns Ok(()) iff the URL is safe to fetch under the current config.
pub(crate) fn validate_url(
    raw: &str,
    allow_list: &[String],
    allow_private_hosts: bool,
) -> std::result::Result<(), String> {
    let parsed = Url::parse(raw).map_err(|e| format!("invalid URL: {e}"))?;

    // Scheme: only http / https. ureq doesn't speak file:// or ftp:// but
    // we reject explicitly so the error message is meaningful and so we
    // don't depend on a transport behaviour we can't see.
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("scheme `{other}` is not allowed (http/https only)")),
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // Private-IP guard. Only kicks in for literal-IP hosts; DNS names
    // are resolved by ureq at request time and a malicious DNS response
    // could still steer to a private IP — the allow-list is the
    // primary defence there.
    if !allow_private_hosts {
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_disallowed_ip(&ip) {
                return Err(format!(
                    "literal-IP host `{ip}` is in a private / loopback / reserved range; \
                     set `pg_ask.allow_private_hosts = on` to permit"
                ));
            }
        }
    }

    if allow_list.is_empty() {
        return Err("allow-list is empty (set `pg_ask.http_allow_list`)".into());
    }

    let matched = allow_list.iter().any(|entry| {
        match EntryMatcher::parse(entry) {
            Ok(m) => m.matches(&parsed),
            // Malformed allow-list entries silently fail to match — we
            // don't want one bad entry to also reject otherwise-good ones.
            // Operators can spot the issue via `ask.preview` or
            // server logs; we don't error here because the GUC is
            // operator-controlled.
            Err(_) => false,
        }
    });
    if !matched {
        return Err(format!("`{raw}` does not match any allow-list entry"));
    }

    Ok(())
}

/// Parsed allow-list entry. Supports two shapes:
///
/// * Bare host: `api.openai.com` — matches any path on that host
///   (https or http).
/// * Full URL: `https://api.example.com/v1` — matches host **and**
///   requires the request path to start with the entry's path prefix.
///
/// The previous prefix-string match is gone: `api.openai.com.evil.com`
/// no longer matches an entry of `https://api.openai.com` because we
/// compare hosts whole, not as substrings.
#[derive(Debug, PartialEq)]
struct EntryMatcher {
    host: String,
    /// Only set when the entry includes a path. None means "any path".
    path_prefix: Option<String>,
    /// Only enforced when the entry includes a scheme. None means
    /// "either http or https accepted".
    scheme: Option<String>,
}

impl EntryMatcher {
    fn parse(entry: &str) -> std::result::Result<Self, String> {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return Err("empty allow-list entry".into());
        }
        // Bare host: no scheme.
        if !trimmed.contains("://") {
            // Strip any accidental path/query so we don't reuse it as the host.
            let host_only = trimmed.split(['/', '?', '#']).next().unwrap_or(trimmed);
            return Ok(EntryMatcher {
                host: host_only.to_ascii_lowercase(),
                path_prefix: None,
                scheme: None,
            });
        }
        let parsed = Url::parse(trimmed).map_err(|e| format!("bad allow-list entry: {e}"))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| "allow-list entry has no host".to_string())?
            .to_ascii_lowercase();
        let path_prefix = if parsed.path().is_empty() || parsed.path() == "/" {
            None
        } else {
            Some(parsed.path().to_string())
        };
        Ok(EntryMatcher {
            host,
            path_prefix,
            scheme: Some(parsed.scheme().to_string()),
        })
    }

    fn matches(&self, request: &Url) -> bool {
        if let Some(scheme) = &self.scheme {
            if !scheme.eq_ignore_ascii_case(request.scheme()) {
                return false;
            }
        }
        let req_host = match request.host_str() {
            Some(h) => h.to_ascii_lowercase(),
            None => return false,
        };
        if req_host != self.host {
            return false;
        }
        if let Some(prefix) = &self.path_prefix {
            if !request.path().starts_with(prefix.as_str()) {
                return false;
            }
        }
        true
    }
}

/// Whether an IP address belongs to a range we refuse to fetch from
/// without `allow_private_hosts = on`.
///
/// Covers: loopback, link-local, private (RFC1918), shared address
/// space, broadcast/multicast/reserved, and the AWS metadata IP
/// `169.254.169.254` (which is link-local anyway, but called out for
/// clarity).
fn is_disallowed_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_disallowed(v4),
        IpAddr::V6(v6) => v6_is_disallowed(v6),
    }
}

fn v4_is_disallowed(ip: &Ipv4Addr) -> bool {
    if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || ip.is_unspecified()
    {
        return true;
    }
    // Carrier-grade NAT (RFC 6598) — not flagged by std::is_private.
    let [a, b, ..] = ip.octets();
    if a == 100 && (64..=127).contains(&b) {
        return true;
    }
    false
}

fn v6_is_disallowed(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_multicast() || ip.is_unspecified() {
        return true;
    }
    let segs = ip.segments();
    // Unique local (fc00::/7).
    if (segs[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // Link-local (fe80::/10).
    if (segs[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // IPv4-mapped — unwrap and run the v4 guard.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_is_disallowed(&v4);
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn allow_any_public(url: &str, list: &[&str]) -> std::result::Result<(), String> {
        let owned: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        validate_url(url, &owned, false)
    }

    #[test]
    fn rejects_evil_host_suffix_pretending_to_be_allowed() {
        // The pre-v0.5.2 bug: `starts_with("https://api.openai.com")`
        // accepted `https://api.openai.com.evil.com/x`. Host-based
        // matching rejects it cleanly.
        let r = allow_any_public(
            "https://api.openai.com.evil.com/v1/chat",
            &["https://api.openai.com"],
        );
        assert!(r.is_err(), "should reject; got {r:?}");
    }

    #[test]
    fn allows_exact_host_match() {
        let r = allow_any_public(
            "https://api.openai.com/v1/chat",
            &["https://api.openai.com"],
        );
        assert!(r.is_ok(), "should allow; got {r:?}");
    }

    #[test]
    fn bare_host_entry_matches_either_scheme() {
        assert!(allow_any_public("https://example.com/x", &["example.com"]).is_ok());
        assert!(allow_any_public("http://example.com/x", &["example.com"]).is_ok());
    }

    #[test]
    fn full_url_entry_enforces_scheme() {
        // Entry pins https; request is http \u2192 reject.
        let r = allow_any_public("http://example.com/x", &["https://example.com"]);
        assert!(r.is_err(), "should require https; got {r:?}");
    }

    #[test]
    fn path_prefix_is_enforced_when_specified() {
        let r1 = allow_any_public(
            "https://api.example.com/v1/things",
            &["https://api.example.com/v1"],
        );
        assert!(r1.is_ok());
        let r2 = allow_any_public(
            "https://api.example.com/v2/things",
            &["https://api.example.com/v1"],
        );
        assert!(r2.is_err(), "v2 should not match a /v1 prefix; got {r2:?}");
    }

    #[test]
    fn rejects_non_http_schemes() {
        let r = allow_any_public("file:///etc/passwd", &["example.com"]);
        assert!(r.is_err(), "file:// must be rejected; got {r:?}");
        let r = allow_any_public("ftp://example.com/x", &["example.com"]);
        assert!(r.is_err(), "ftp:// must be rejected; got {r:?}");
    }

    #[test]
    fn empty_allow_list_denies_everything() {
        let r = validate_url("https://example.com/x", &[], false);
        assert!(r.is_err(), "empty allow-list must deny");
    }

    #[test]
    fn rejects_loopback_ip() {
        let r = allow_any_public("http://127.0.0.1/x", &["127.0.0.1"]);
        assert!(r.is_err(), "loopback must be rejected; got {r:?}");
        let r = allow_any_public("http://[::1]/x", &["::1"]);
        assert!(r.is_err(), "v6 loopback must be rejected; got {r:?}");
    }

    #[test]
    fn rejects_aws_metadata_ip() {
        // 169.254.169.254 — link-local, classic SSRF target.
        let r = allow_any_public(
            "http://169.254.169.254/latest/meta-data/",
            &["169.254.169.254"],
        );
        assert!(r.is_err(), "AWS metadata IP must be rejected; got {r:?}");
    }

    #[test]
    fn rejects_rfc1918_ranges() {
        for ip in ["10.0.0.1", "172.16.0.1", "192.168.1.1"] {
            let url = format!("http://{ip}/x");
            let r = allow_any_public(&url, &[ip]);
            assert!(r.is_err(), "{ip} should be rejected; got {r:?}");
        }
    }

    #[test]
    fn rejects_cgnat_range() {
        // RFC 6598 carrier-grade NAT, 100.64.0.0/10.
        let r = allow_any_public("http://100.64.0.1/x", &["100.64.0.1"]);
        assert!(r.is_err(), "CGNAT must be rejected; got {r:?}");
    }

    #[test]
    fn allow_private_hosts_overrides_ip_guard() {
        // Self-hosted scenario: operator opted in.
        let r = validate_url("http://10.0.0.5:8080/x", &["10.0.0.5".to_string()], true);
        assert!(
            r.is_ok(),
            "private host should be allowed when opted in; got {r:?}"
        );
    }

    #[test]
    fn host_matching_is_case_insensitive() {
        let r = allow_any_public("https://API.Example.COM/x", &["api.example.com"]);
        assert!(r.is_ok(), "host match must ignore case; got {r:?}");
    }

    #[test]
    fn entry_matcher_parses_bare_host() {
        let m = EntryMatcher::parse("api.openai.com").unwrap();
        assert_eq!(m.host, "api.openai.com");
        assert_eq!(m.path_prefix, None);
        assert_eq!(m.scheme, None);
    }

    #[test]
    fn entry_matcher_parses_full_url_with_path() {
        let m = EntryMatcher::parse("https://api.example.com/v1").unwrap();
        assert_eq!(m.host, "api.example.com");
        assert_eq!(m.path_prefix.as_deref(), Some("/v1"));
        assert_eq!(m.scheme.as_deref(), Some("https"));
    }

    #[test]
    fn entry_matcher_treats_root_path_as_no_prefix() {
        let m = EntryMatcher::parse("https://example.com/").unwrap();
        assert_eq!(m.path_prefix, None);
    }
}
