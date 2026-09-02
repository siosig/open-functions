//! Reverse-proxy forwarding from the invoke listener to a function instance.
//!
//! This module holds the pure, testable logic used by the request forwarder:
//! hop-by-hop header stripping, request/response header rewriting, and mapping
//! non-2xx forwarding failures to the status code / error code pairs from
//! `specs/001-cloud-functions-local/contracts/function-contract.md`'s
//! "Status codes" table. It deliberately has no `axum` dependency (`cf-rs-core`
//! does not depend on axum) and operates only on the `http` crate's types, which
//! axum re-exports and interoperates with. The actual network I/O (via
//! `hyper-util`) is implemented by the `Forwarder` in the `cf-rs` binary crate,
//! built around these functions.

use std::net::IpAddr;

use http::{HeaderMap, HeaderName, HeaderValue};

#[cfg(test)]
mod tests;

/// The static hop-by-hop headers that must never be forwarded, per RFC 7230 §6.1
/// and function-contract.md's "HTTP functions" section.
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "proxy-authenticate",
    "proxy-authorization",
    "upgrade",
];

/// Removes hop-by-hop headers from `headers` in place.
///
/// Removes the static list in [`HOP_BY_HOP_HEADERS`] (case-insensitively, as
/// `HeaderMap` always is), and additionally removes any header named in a
/// `Connection` header's value, per RFC 7230 §6.1 (e.g. `Connection: X-Custom`
/// means `X-Custom` is also stripped).
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // Collect the header names listed in any `Connection` header value(s)
    // before removing `Connection` itself below.
    let mut dynamic: Vec<HeaderName> = Vec::new();
    for value in headers.get_all(http::header::CONNECTION).iter() {
        if let Ok(s) = value.to_str() {
            for token in s.split(',') {
                let token = token.trim();
                if token.is_empty() {
                    continue;
                }
                if let Ok(name) = HeaderName::try_from(token) {
                    dynamic.push(name);
                }
            }
        }
    }

    for name in HOP_BY_HOP_HEADERS {
        headers.remove(*name);
    }
    for name in dynamic {
        headers.remove(name);
    }
}

/// Sets `headers[name]` to `value`, overwriting any existing value(s).
/// Silently no-ops if `value` is not a legal header value (should not happen
/// for the callers in this module, which only pass execution ids, IP address
/// strings, and configured protocol/host strings).
fn set_header(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(name, v);
    }
}

/// Context needed to rewrite request headers when forwarding a client request
/// to a function instance.
pub struct RequestRewriteContext {
    /// The `Function-Execution-Id` the host generated for this call. Always
    /// wins over any client-supplied value — the host is authoritative per
    /// function-contract.md.
    pub execution_id: String,
    /// The client's address, appended to (or used to set) `X-Forwarded-For`.
    pub client_addr: IpAddr,
    /// "http" or "https", forwarded as `X-Forwarded-Proto`. cf-rs itself has
    /// no TLS (per spec Assumptions), so this will always be "http" in
    /// practice, but is kept as a parameter for correctness/testability
    /// rather than hardcoded.
    pub proto: &'static str,
    /// The `Host` the client sent, forwarded as `X-Forwarded-Host` when present.
    pub original_host: Option<String>,
}

/// Rewrites `headers` in place for forwarding a client request to a function
/// instance:
///
/// - strips hop-by-hop headers (see [`strip_hop_by_hop`]);
/// - sets `Function-Execution-Id`, overwriting any client-supplied value;
/// - appends to (or sets) `X-Forwarded-For`;
/// - sets `X-Forwarded-Proto`;
/// - sets `X-Forwarded-Host` from `ctx.original_host`, if present.
///
/// Does not touch `traceparent`: it is not in the hop-by-hop strip list, so a
/// client-supplied value passes through unchanged simply by this function
/// never referencing it.
pub fn rewrite_request_headers(headers: &mut HeaderMap, ctx: &RequestRewriteContext) {
    strip_hop_by_hop(headers);

    set_header(
        headers,
        HeaderName::from_static("function-execution-id"),
        &ctx.execution_id,
    );

    let xff_name = HeaderName::from_static("x-forwarded-for");
    let appended = match headers.get(&xff_name).and_then(|v| v.to_str().ok()) {
        Some(existing) if !existing.is_empty() => format!("{existing}, {}", ctx.client_addr),
        _ => ctx.client_addr.to_string(),
    };
    set_header(headers, xff_name, &appended);

    set_header(
        headers,
        HeaderName::from_static("x-forwarded-proto"),
        ctx.proto,
    );

    if let Some(host) = &ctx.original_host {
        set_header(headers, HeaderName::from_static("x-forwarded-host"), host);
    }
}

/// Rewrites `headers` in place for a response on its way back to the client:
/// strips hop-by-hop headers and sets `Function-Execution-Id` (overwriting any
/// value if somehow already present) to the same value the request carried.
/// Per function-contract.md, "the response passes status/headers/body through
/// unchanged, with Function-Execution-Id added" — status and body are
/// otherwise passed through untouched by this function.
pub fn rewrite_response_headers(headers: &mut HeaderMap, execution_id: &str) {
    strip_hop_by_hop(headers);
    set_header(
        headers,
        HeaderName::from_static("function-execution-id"),
        execution_id,
    );
}

/// The ways a forwarded call can fail to produce an instance response.
///
/// A successful response (any status code returned by the instance) is
/// forwarded as-is by the caller and is *not* represented here — callers
/// should only reach for [`map_outcome`] once they already know the call
/// failed to reach or complete against an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardFailure {
    /// No response arrived before the configured timeout elapsed.
    Timeout,
    /// The instance never accepted the TCP connection (still starting, or dead).
    ConnectionRefused,
    /// The connection was accepted but dropped mid-response (instance crashed
    /// while handling the request).
    ConnectionReset,
    /// The call was rejected before dispatch due to the concurrency/queue limit.
    QueueRejected,
}

/// The status code and machine-readable error code to send the client for a
/// given [`ForwardFailure`], matching function-contract.md's "Status codes"
/// table (timeout → 504, connection refused → 502, connection reset
/// mid-response → 500, queue rejected → 429).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ErrorMapping {
    pub status: u16,
    pub code: &'static str,
}

/// Maps a [`ForwardFailure`] to the status/code pair to send the client.
///
/// function-contract.md's status table gives the HTTP status for each case
/// but, unlike admin-api.md's error-format table, does not spell out a
/// machine-readable `code` string for any of them. To keep `code` consistent
/// with admin-api.md's existing values (`NOT_FOUND`, `UNAVAILABLE`, etc., which
/// mirror Google's canonical API error codes / `google.rpc.Code`), this
/// function picks the standard Google API error code for each HTTP status per
/// https://cloud.google.com/apis/design/errors#error_codes:
/// 504 → `DEADLINE_EXCEEDED`, 502 → `UNAVAILABLE` (same family as admin-api.md's
/// existing 503 `UNAVAILABLE` — both describe an instance that cannot be
/// reached), 500 → `INTERNAL`, 429 → `RESOURCE_EXHAUSTED`. This is a judgment
/// call where the contract is silent, not a value pulled from the spec.
pub fn map_outcome(outcome: ForwardFailure) -> ErrorMapping {
    match outcome {
        ForwardFailure::Timeout => ErrorMapping {
            status: 504,
            code: "DEADLINE_EXCEEDED",
        },
        ForwardFailure::ConnectionRefused => ErrorMapping {
            status: 502,
            code: "UNAVAILABLE",
        },
        ForwardFailure::ConnectionReset => ErrorMapping {
            status: 500,
            code: "INTERNAL",
        },
        ForwardFailure::QueueRejected => ErrorMapping {
            status: 429,
            code: "RESOURCE_EXHAUSTED",
        },
    }
}
