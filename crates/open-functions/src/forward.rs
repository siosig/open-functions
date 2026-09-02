//! Network-facing request forwarder: proxies one HTTP request to a function
//! instance's address, using `open_functions_core::forward`'s pure header-rewrite and
//! failure-mapping logic. Implements T039, built around T029's completed
//! building blocks (see `open_functions_core::forward` for the contract this follows).

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use open_functions_core::forward::{
    ForwardFailure, RequestRewriteContext, rewrite_response_headers,
};

#[derive(Clone)]
pub struct Forwarder {
    client: Client<HttpConnector, Body>,
}

impl Default for Forwarder {
    fn default() -> Self {
        Self::new()
    }
}

impl Forwarder {
    pub fn new() -> Self {
        Self {
            client: Client::builder(TokioExecutor::new()).build_http(),
        }
    }

    /// Forwards `req` to the instance at `addr`, rewriting request headers per
    /// `ctx`, waiting at most `timeout` for a complete response. On success,
    /// the response headers are rewritten (hop-by-hop stripped,
    /// `Function-Execution-Id` set) before being returned; the caller forwards
    /// status/body/headers to the original client unchanged from there.
    ///
    /// `req`'s URI must already carry the path/query to send to the instance
    /// (the caller — the invoke handler — is responsible for the path
    /// rewriting implied by path-prefix vs. host-header resolution; this
    /// function only redirects the *authority* to `addr`).
    pub async fn forward(
        &self,
        addr: SocketAddr,
        mut req: axum::extract::Request,
        ctx: &RequestRewriteContext,
        timeout: Duration,
    ) -> Result<Response, ForwardFailure> {
        let received_at = std::time::Instant::now();
        open_functions_core::forward::rewrite_request_headers(req.headers_mut(), ctx);

        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        let new_uri = match http::Uri::builder()
            .scheme("http")
            .authority(addr.to_string())
            .path_and_query(path_and_query)
            .build()
        {
            Ok(uri) => uri,
            // A well-formed incoming request's path/query, re-targeted at a
            // known-valid socket address, cannot produce an invalid URI in
            // practice; treat the theoretical failure as "instance
            // unreachable" rather than panicking.
            Err(_) => return Err(ForwardFailure::ConnectionRefused),
        };
        *req.uri_mut() = new_uri;

        // `open_functions_forward_overhead_seconds`: host-added latency only — the
        // time spent here before the instance call starts, plus the time
        // spent here after the instance's response arrives — deliberately
        // excluding the instance's own processing time in between (that's
        // what `open_functions_invocation_duration_seconds` measures as a whole).
        let pre_call_overhead = received_at.elapsed();

        let outcome = tokio::time::timeout(timeout, self.client.request(req)).await;
        let response_received_at = std::time::Instant::now();

        let result = match outcome {
            Err(_) => Err(ForwardFailure::Timeout),
            Ok(Err(err)) => Err(classify_client_error(&err)),
            Ok(Ok(resp)) => Ok(resp),
        };

        let final_result = match result {
            Ok(resp) => {
                let (mut parts, body) = resp.into_parts();
                rewrite_response_headers(&mut parts.headers, &ctx.execution_id);
                Ok(Response::from_parts(parts, Body::new(body)))
            }
            Err(failure) => Err(failure),
        };

        let post_call_overhead = response_received_at.elapsed();
        metrics::histogram!("open_functions_forward_overhead_seconds")
            .record((pre_call_overhead + post_call_overhead).as_secs_f64());

        final_result
    }
}

/// Classifies a `hyper-util` legacy client error into a [`ForwardFailure`].
/// Connection-establishment failures (instance not yet listening, or dead)
/// map to `ConnectionRefused`; failures after a connection was established
/// (the instance crashed mid-response) map to `ConnectionReset`.
fn classify_client_error(err: &hyper_util::client::legacy::Error) -> ForwardFailure {
    if err.is_connect() {
        ForwardFailure::ConnectionRefused
    } else {
        ForwardFailure::ConnectionReset
    }
}
