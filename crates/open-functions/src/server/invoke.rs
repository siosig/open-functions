//! Invoke listener (T042, extended by US2's T054): resolves the request to a
//! function, acquires an instance from its `InstancePool` (starting one if
//! needed), forwards the request via `Forwarder`, and maps failures to the
//! status codes in `contracts/function-contract.md`'s "Status codes" table.
//!
//! `/_cf/push/{name}` (Pub/Sub push delivery, `Resolved::Push`) validates and
//! converts the open-pubusb Push body into a CloudEvent (via `open_functions_core::pubsub`),
//! forwards it to the function instance in CloudEvents binary content mode,
//! and maps the instance's response per function-contract.md's "Pub/Sub
//! Push → CloudEvent conversion" status table (2xx → 204 Ack; failures → the
//! status open-pubusb should treat as Nack).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json};
use cloudevents::AttributesReader;
use open_functions_core::forward::{ForwardFailure, RequestRewriteContext, map_outcome};
use open_functions_core::model::function::FunctionState;
use open_functions_core::pool::AcquireError;
use open_functions_core::pubsub::convert::{
    CloudEventParams, PushConvertError, parse_push_envelope, to_cloud_event,
};
use open_functions_core::registry::service::RegistryService;
use open_functions_core::resolve::{Resolved, Resolver};
use serde_json::json;

use crate::forward::Forwarder;

#[derive(Clone)]
pub struct InvokeState {
    pub resolver: Arc<Resolver>,
    pub registry: Arc<RegistryService>,
    pub forwarder: Arc<Forwarder>,
    /// `pubsub.project`, used to build the CloudEvent `source` field for
    /// Push deliveries. `None` when `pubsub.enabled = false` (Push requests
    /// arriving anyway — e.g. a stale open-pubusb subscription — get a 400).
    pub pubsub_project: Option<String>,
}

pub fn router(state: InvokeState) -> Router {
    Router::new().fallback(handle).with_state(state)
}

async fn handle(
    State(state): State<InvokeState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    mut req: Request,
) -> axum::response::Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let path = req.uri().path().to_string();

    let resolved = state.resolver.resolve(host.as_deref(), &path);

    let function = match resolved {
        Resolved::NoMatch => return not_found("function name required"),
        // Path-prefix matches strip the `/{name}` prefix before forwarding
        // (function-contract.md: the host passes the remaining path with
        // `/<name>` stripped to the function); host-header matches forward
        // the path unchanged.
        Resolved::PathPrefix {
            function,
            rest_path,
        } => {
            if !rewrite_request_path(&mut req, &rest_path) {
                return api_bad_request("invalid request path");
            }
            function
        }
        Resolved::Host { function } => function,
        Resolved::Push { function } => {
            return handle_push(state, function, req).await;
        }
    };

    let record = match state.registry.get(&function) {
        Ok(Some(f)) => f,
        Ok(None) => return not_found(&format!("function {function:?} not found")),
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": err.to_string(), "code": "INTERNAL"})),
            )
                .into_response();
        }
    };

    if record.current_revision.is_none() || record.state != FunctionState::Ready {
        record_invocation(&function, "http", "rejected", 503, None);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "function not ready",
                "code": "UNAVAILABLE",
                "state": record.state,
            })),
        )
            .into_response();
    }

    let Some(pool) = state.registry.pool_for(&function).await else {
        record_invocation(&function, "http", "rejected", 503, None);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "function not ready", "code": "UNAVAILABLE"})),
        )
            .into_response();
    };

    let acquired = match pool.acquire().await {
        Ok(acquired) => acquired,
        Err(AcquireError::Rejected) => {
            record_invocation(&function, "http", "rejected", 429, None);
            return queue_rejected();
        }
        Err(AcquireError::QueueTimeout(_)) => {
            record_invocation(&function, "http", "rejected", 429, None);
            return queue_rejected();
        }
        Err(AcquireError::Draining) => {
            record_invocation(&function, "http", "rejected", 503, None);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONNECTION, "close")],
                Json(json!({"error": "function is being deleted", "code": "UNAVAILABLE"})),
            )
                .into_response();
        }
        Err(AcquireError::Spawn(err)) => {
            record_invocation(&function, "http", "error", 502, None);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": err.to_string(), "code": "UNAVAILABLE"})),
            )
                .into_response();
        }
    };

    let execution_id = uuid::Uuid::new_v4().simple().to_string();
    let ctx = RequestRewriteContext {
        execution_id: execution_id.clone(),
        client_addr: client_addr.ip(),
        proto: "http",
        original_host: host,
    };
    let timeout = Duration::from_secs(u64::from(record.timeout_secs));

    // `open_functions_invocation_duration_seconds`: "forward → response complete"
    // per ops-config.md, so timed from just before the forward call rather
    // than from the top of this handler (which also includes registry/pool
    // lookups already covered by their own concerns).
    let forward_started_at = std::time::Instant::now();
    let outcome = state
        .forwarder
        .forward(acquired.addr, req, &ctx, timeout)
        .await;
    let duration = forward_started_at.elapsed();

    match outcome {
        Ok(resp) => {
            record_invocation(
                &function,
                "http",
                "ok",
                resp.status().as_u16(),
                Some(duration),
            );
            resp
        }
        Err(failure) => {
            if matches!(
                failure,
                ForwardFailure::ConnectionRefused | ForwardFailure::ConnectionReset
            ) {
                pool.report_dead(acquired.addr).await;
            }
            let mapping = map_outcome(failure);
            record_invocation(
                &function,
                "http",
                forward_failure_outcome(failure),
                mapping.status,
                Some(duration),
            );
            let status = StatusCode::from_u16(mapping.status).unwrap_or(StatusCode::BAD_GATEWAY);
            (
                status,
                [(
                    header::HeaderName::from_static("function-execution-id"),
                    execution_id,
                )],
                Json(json!({"error": format!("{failure:?}"), "code": mapping.code})),
            )
                .into_response()
        }
    }
}

/// Maps a [`ForwardFailure`] to the `outcome` label value for
/// `open_functions_invocations_total`/`open_functions_invocation_duration_seconds`
/// (ok/error/timeout/rejected), per ops-config.md's metrics table.
fn forward_failure_outcome(failure: ForwardFailure) -> &'static str {
    match failure {
        ForwardFailure::Timeout => "timeout",
        ForwardFailure::ConnectionRefused | ForwardFailure::ConnectionReset => "error",
        ForwardFailure::QueueRejected => "rejected",
    }
}

/// Records `open_functions_invocations_total{function, kind, outcome}`, and when a
/// forward attempt actually happened, `open_functions_invocation_duration_seconds{function, kind}`
/// and the `open_functions::invoke` structured log line (`status`, `duration_ms`,
/// `outcome`), per ops-config.md's metrics and logging tables.
fn record_invocation(
    function: &str,
    kind: &'static str,
    outcome: &'static str,
    status: u16,
    duration: Option<Duration>,
) {
    metrics::counter!(
        "open_functions_invocations_total",
        "function" => function.to_string(),
        "kind" => kind,
        "outcome" => outcome,
    )
    .increment(1);
    if let Some(duration) = duration {
        metrics::histogram!(
            "open_functions_invocation_duration_seconds",
            "function" => function.to_string(),
            "kind" => kind,
        )
        .record(duration.as_secs_f64());
    }
    let duration_ms = duration.map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);
    tracing::info!(
        target: "open_functions::invoke",
        function,
        kind,
        status,
        duration_ms,
        outcome,
    );
}

/// Handles `POST /_cf/push/{function}` (open-pubusb Pub/Sub Push delivery):
/// validates and converts the body to a CloudEvent, forwards it to the
/// function instance in binary content mode, and maps the outcome to the
/// status open-pubusb should treat as Ack/Nack, per function-contract.md.
async fn handle_push(
    state: InvokeState,
    function: String,
    req: Request,
) -> axum::response::Response {
    let Some(project) = state.pubsub_project.clone() else {
        record_push_received(&function, "invalid");
        return api_bad_request("pubsub.enabled = false; push delivery is not accepted");
    };

    let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            record_push_received(&function, "invalid");
            return api_bad_request("failed to read request body");
        }
    };

    let envelope = match parse_push_envelope(&body_bytes) {
        Ok(envelope) => envelope,
        Err(
            err @ (PushConvertError::InvalidJson
            | PushConvertError::NotAnObject
            | PushConvertError::MissingMessage
            | PushConvertError::InvalidBase64),
        ) => {
            record_push_received(&function, "invalid");
            return api_bad_request(&err.to_string());
        }
    };

    let record = match state.registry.get(&function) {
        Ok(Some(f)) => f,
        Ok(None) => {
            record_push_received(&function, "invalid");
            return not_found(&format!("function {function:?} not found"));
        }
        Err(err) => {
            record_push_received(&function, "invalid");
            return internal_error(&err.to_string());
        }
    };

    let topic = match &record.trigger {
        open_functions_core::model::function::Trigger::Pubsub { topic } => topic.clone(),
        open_functions_core::model::function::Trigger::Http => {
            record_push_received(&function, "invalid");
            return api_bad_request(&format!(
                "function {function:?} does not have a pubsub trigger"
            ));
        }
    };

    if record.current_revision.is_none() || record.state != FunctionState::Ready {
        record_push_received(&function, "nack");
        record_invocation(&function, "event", "rejected", 503, None);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "function not ready", "code": "UNAVAILABLE", "state": record.state})),
        )
            .into_response();
    }
    let Some(pool) = state.registry.pool_for(&function).await else {
        record_push_received(&function, "nack");
        record_invocation(&function, "event", "rejected", 503, None);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "function not ready", "code": "UNAVAILABLE"})),
        )
            .into_response();
    };

    let acquired = match pool.acquire().await {
        Ok(acquired) => acquired,
        Err(AcquireError::Rejected | AcquireError::QueueTimeout(_)) => {
            record_push_received(&function, "nack");
            record_invocation(&function, "event", "rejected", 429, None);
            return status_only(429);
        }
        Err(AcquireError::Draining) => {
            record_push_received(&function, "nack");
            record_invocation(&function, "event", "rejected", 503, None);
            return status_only(503);
        }
        Err(AcquireError::Spawn(_)) => {
            record_push_received(&function, "nack");
            record_invocation(&function, "event", "error", 503, None);
            return status_only(503);
        }
    };

    let event = to_cloud_event(
        &envelope,
        &CloudEventParams {
            project: &project,
            topic: &topic,
        },
    );
    let push_req = match build_binary_mode_request(&event) {
        Ok(req) => req,
        Err(_) => {
            record_push_received(&function, "nack");
            record_invocation(&function, "event", "error", 500, None);
            return internal_error("failed to build CloudEvent request");
        }
    };

    let execution_id = uuid::Uuid::new_v4().simple().to_string();
    let ctx = RequestRewriteContext {
        execution_id,
        client_addr: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        proto: "http",
        original_host: None,
    };
    let timeout = Duration::from_secs(u64::from(record.timeout_secs));

    let forward_started_at = std::time::Instant::now();
    let outcome = state
        .forwarder
        .forward(acquired.addr, push_req, &ctx, timeout)
        .await;
    let duration = Some(forward_started_at.elapsed());

    match outcome {
        Ok(resp) if resp.status().is_success() => {
            record_push_received(&function, "ack");
            record_invocation(&function, "event", "ok", 204, duration);
            status_only(204)
        }
        Ok(resp) => {
            // Transparent per function-contract.md: pass through the
            // instance's own status (Nack -> open-pubusb retries).
            record_push_received(&function, "nack");
            let status = resp.status();
            record_invocation(&function, "event", "ok", status.as_u16(), duration);
            status_only(status.as_u16())
        }
        Err(failure) => {
            if matches!(
                failure,
                ForwardFailure::ConnectionRefused | ForwardFailure::ConnectionReset
            ) {
                pool.report_dead(acquired.addr).await;
            }
            record_push_received(&function, "nack");
            let mapping = map_outcome(failure);
            record_invocation(
                &function,
                "event",
                forward_failure_outcome(failure),
                mapping.status,
                duration,
            );
            status_only(mapping.status)
        }
    }
}

/// Records `open_functions_pubsub_push_received_total{function, result}`
/// (ack/nack/invalid), per ops-config.md's metrics table.
fn record_push_received(function: &str, result: &'static str) {
    metrics::counter!(
        "open_functions_pubsub_push_received_total",
        "function" => function.to_string(),
        "result" => result,
    )
    .increment(1);
}

/// Builds a CloudEvents 1.0 binary-content-mode HTTP request (`ce-*` headers + JSON body = the event's `data`), per function-contract.md's "CloudEvents functions" section.
fn build_binary_mode_request(event: &cloudevents::Event) -> Result<Request, ()> {
    let data_json = event
        .data()
        .cloned()
        .map(serde_json::Value::try_from)
        .transpose()
        .map_err(|_| ())?
        .unwrap_or(serde_json::Value::Null);
    let body = serde_json::to_vec(&data_json).map_err(|_| ())?;

    let mut builder = Request::builder()
        .method("POST")
        .uri("/")
        .header("ce-specversion", "1.0")
        .header("ce-id", event.id())
        .header("ce-source", event.source().to_string())
        .header("ce-type", event.ty())
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(time) = event.time() {
        builder = builder.header("ce-time", time.to_rfc3339());
    }

    builder.body(Body::from(body)).map_err(|_| ())
}

fn status_only(status: u16) -> axum::response::Response {
    StatusCode::from_u16(status)
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
}

fn internal_error(message: &str) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": message, "code": "INTERNAL"})),
    )
        .into_response()
}

/// Replaces `req`'s URI path with `rest_path`, preserving the original query
/// string. `rest_path` always starts with `/` (per `Resolver`'s contract), so
/// the rebuilt `PathAndQuery` is always valid; returning `false` is only a
/// defensive fallback rather than an expected path.
fn rewrite_request_path(req: &mut Request, rest_path: &str) -> bool {
    let query = req.uri().query().map(str::to_string);
    let path_and_query = match &query {
        Some(q) => format!("{rest_path}?{q}"),
        None => rest_path.to_string(),
    };

    let mut parts = req.uri().clone().into_parts();
    let Ok(pq) = http::uri::PathAndQuery::try_from(path_and_query) else {
        return false;
    };
    parts.path_and_query = Some(pq);

    match http::Uri::from_parts(parts) {
        Ok(uri) => {
            *req.uri_mut() = uri;
            true
        }
        Err(_) => false,
    }
}

fn api_bad_request(message: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": message, "code": "INVALID_ARGUMENT"})),
    )
        .into_response()
}

fn not_found(message: &str) -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": message, "code": "NOT_FOUND"})),
    )
        .into_response()
}

fn queue_rejected() -> axum::response::Response {
    let mapping = map_outcome(ForwardFailure::QueueRejected);
    (
        StatusCode::from_u16(mapping.status).unwrap_or(StatusCode::TOO_MANY_REQUESTS),
        [(header::RETRY_AFTER, "1")],
        Json(json!({"error": "too many concurrent requests", "code": mapping.code})),
    )
        .into_response()
}
