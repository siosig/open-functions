//! Admin listener: `/healthz`, `/readyz`, `/metrics`, Bearer-token middleware
//! guarding `/v1/*`, and the `/v1/functions/*` management API (T041, extended
//! US5's T081: `GET .../logs`, `POST .../:stop`, and `follow` support on both
//! the build log and the function log endpoints). See `contracts/admin-api.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::{Router, middleware};
use futures_util::StreamExt;
use open_functions_core::logs::pipe::LogRecord;
use open_functions_core::model::build::BuildStatus;
use open_functions_core::model::function::{Function, Source, Trigger};
use open_functions_core::registry::service::{
    DeleteError, RegisterError, RegisterRequest, RegistryService, StopError,
};
use open_functions_core::registry::store::StoreError;
use serde::Deserialize;
use serde_json::json;

#[derive(Clone)]
pub struct AdminState {
    /// `admin.token` from config, or `None` if unset (unauthenticated `/v1/*`,
    /// only valid per `crate::config::validate` when `admin.listen` is loopback).
    pub token: Option<String>,
    pub metrics_enabled: bool,
    pub metrics_require_token: bool,
    pub metrics_handle: Arc<metrics_exporter_prometheus::PrometheusHandle>,
    /// Flipped to `true` once `serve` has completed startup (both listeners
    /// bound, storage open). Read by `/readyz`.
    pub ready: Arc<AtomicBool>,
    pub registry: Arc<RegistryService>,
    /// Base URL (scheme://host:port) used to build the `urls.path` field in
    /// function detail responses, e.g. `http://127.0.0.1:8080`.
    pub invoke_base_url: String,
    pub host_suffix: Option<String>,
}

pub fn router(state: AdminState) -> Router {
    let v1 = Router::new()
        .route(
            "/functions",
            get(list_functions).put(register_function_by_path_error),
        )
        .route(
            "/functions/{name}",
            axum::routing::put(register_function)
                .get(get_function)
                .delete(delete_function),
        )
        .route("/functions/{name}/builds/{build_id}", get(get_build))
        .route(
            "/functions/{name}/builds/{build_id}/log",
            get(get_build_log),
        )
        .route("/functions/{name}/logs", get(get_function_logs))
        // admin-api.md specifies `POST /v1/functions/{name}:stop` (Google's
        // custom-method colon convention), but axum 0.8.9 pins `matchit`
        // to `=0.8.4`, whose router rejects a literal suffix sharing a path
        // segment with a named param ("Only one parameter is allowed per
        // path segment") -- a later matchit (0.8.6+) lifts this, but axum
        // itself hard-pins the version, leaving no way to opt in from this
        // crate. `/functions/{name}/stop` is the closest routable
        // equivalent; `AdminClient::stop` and `fn stop` use this same path.
        .route("/functions/{name}/stop", axum::routing::post(stop_function))
        .layer(middleware::from_fn_with_state(state.clone(), require_token));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .nest("/v1", v1)
        .with_state(state)
}

/// `PUT /v1/functions` (no name segment) is not a valid route per
/// admin-api.md; kept only so the router doesn't 404 with an unhelpful
/// "method not allowed" for a common typo — returns the same 400 shape as
/// other validation errors.
async fn register_function_by_path_error() -> axum::response::Response {
    api_error(
        StatusCode::BAD_REQUEST,
        "INVALID_ARGUMENT",
        "function name is required: PUT /v1/functions/{name}",
    )
}

async fn healthz() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn readyz(State(state): State<AdminState>) -> impl IntoResponse {
    if state.ready.load(Ordering::SeqCst) {
        (
            StatusCode::OK,
            Json(json!({"status": "ready", "functions": 0, "bindings_pending": 0})),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready"})),
        )
    }
}

async fn metrics(State(state): State<AdminState>, headers: HeaderMap) -> axum::response::Response {
    if !state.metrics_enabled {
        return (StatusCode::NOT_FOUND, "metrics disabled").into_response();
    }
    if state.metrics_require_token && !token_matches(&state.token, &headers) {
        return unauthorized();
    }
    (StatusCode::OK, state.metrics_handle.render()).into_response()
}

async fn require_token(
    State(state): State<AdminState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    if !token_matches(&state.token, &headers) {
        return unauthorized();
    }
    next.run(request).await
}

/// A `None` token means the admin listener is unauthenticated (only valid on
/// loopback, per `crate::config::validate`), so every request passes.
fn token_matches(token: &Option<String>, headers: &HeaderMap) -> bool {
    let Some(token) = token else {
        return true;
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    provided == Some(token.as_str())
}

fn unauthorized() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "authentication required", "code": "UNAUTHENTICATED"})),
    )
        .into_response()
}

fn api_error(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> axum::response::Response {
    (status, Json(json!({"error": message.into(), "code": code}))).into_response()
}

/// `PUT /v1/functions/{name}` request body, per `contracts/admin-api.md`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeployRequest {
    trigger: TriggerDto,
    source: SourceDto,
    #[serde(default)]
    entry_point: Option<String>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    timeout_secs: Option<u32>,
    #[serde(default)]
    concurrency: Option<u32>,
    #[serde(default)]
    memory_mib: Option<u32>,
    #[serde(default)]
    min_instances: Option<u32>,
    #[serde(default)]
    max_instances: Option<u32>,
    #[serde(default)]
    idle_timeout_secs: Option<u32>,
    #[serde(default)]
    queue_policy: Option<String>,
    #[serde(default)]
    queue_max_wait_secs: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TriggerDto {
    Http,
    Pubsub { topic: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SourceDto {
    Dir {
        path: String,
        #[serde(default)]
        bin: Option<String>,
    },
    Image {
        #[serde(rename = "ref")]
        image_ref: String,
    },
}

async fn register_function(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Json(body): Json<DeployRequest>,
) -> axum::response::Response {
    let trigger = match body.trigger {
        TriggerDto::Http => Trigger::Http,
        TriggerDto::Pubsub { topic } => Trigger::Pubsub { topic },
    };
    let source = match body.source {
        SourceDto::Dir { path, bin } => Source::Dir { path, bin },
        SourceDto::Image { image_ref } => Source::Image { image_ref },
    };
    let queue_policy = match body.queue_policy.as_deref() {
        None => None,
        Some("wait") => Some(open_functions_core::model::function::QueuePolicy::Wait),
        Some("reject") => Some(open_functions_core::model::function::QueuePolicy::Reject),
        Some(other) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                format!("queue_policy must be \"wait\" or \"reject\", got {other:?}"),
            );
        }
    };

    let req = RegisterRequest {
        name,
        trigger,
        source,
        entry_point: body.entry_point,
        env: body.env,
        timeout_secs: body.timeout_secs,
        concurrency: body.concurrency,
        memory_mib: body.memory_mib,
        min_instances: body.min_instances,
        max_instances: body.max_instances,
        idle_timeout_secs: body.idle_timeout_secs,
        queue_policy,
        queue_max_wait_secs: body.queue_max_wait_secs,
    };

    match state.registry.register(req).await {
        Ok(accepted) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "revision": accepted.revision,
                "build_id": accepted.build_id,
            })),
        )
            .into_response(),
        Err(RegisterError::Validation(err)) => {
            api_error(StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", err.to_string())
        }
        Err(RegisterError::SourceNotFound(path)) => api_error(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            format!("source path {path:?} does not exist or is not a directory"),
        ),
        Err(RegisterError::Unsupported(reason)) => api_error(
            StatusCode::PRECONDITION_FAILED,
            "FAILED_PRECONDITION",
            reason,
        ),
        Err(RegisterError::BuildInProgress(name)) => api_error(
            StatusCode::CONFLICT,
            "ABORTED",
            format!("a build for {name:?} is already in progress"),
        ),
        Err(RegisterError::Store(err)) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            err.to_string(),
        ),
    }
}

async fn list_functions(State(state): State<AdminState>) -> axum::response::Response {
    match state.registry.list() {
        Ok(functions) => {
            let summaries: Vec<_> = functions.iter().map(function_summary_json).collect();
            (StatusCode::OK, Json(json!({ "functions": summaries }))).into_response()
        }
        Err(err) => store_error_response(&err),
    }
}

async fn get_function(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> axum::response::Response {
    match state.registry.get(&name) {
        Ok(Some(function)) => {
            let instances = state.registry.instance_count(&name).await;
            let binding = match state.registry.get_binding(&name) {
                Ok(binding) => binding,
                Err(err) => return store_error_response(&err),
            };
            // `None` for an image-mode function's current revision (it never
            // has a `Build` -- see `registry::service::register_image`) or a
            // function that has never successfully deployed a revision;
            // `fn build-log` without `--build` uses this to find the build
            // to show.
            let current_build_id = match function.current_revision {
                Some(rev) => state
                    .registry
                    .get_revision(&name, rev)
                    .ok()
                    .flatten()
                    .and_then(|r| r.build_id),
                None => None,
            };
            (
                StatusCode::OK,
                Json(function_detail_json(
                    &state,
                    &function,
                    instances,
                    binding,
                    current_build_id,
                )),
            )
                .into_response()
        }
        Ok(None) => function_not_found(&name),
        Err(err) => store_error_response(&err),
    }
}

async fn delete_function(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> axum::response::Response {
    match state.registry.delete(&name).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"state": "deleting"}))).into_response(),
        Err(DeleteError::NotFound(name)) => function_not_found(&name),
        Err(DeleteError::Store(err)) => store_error_response(&err),
    }
}

async fn get_build(
    State(state): State<AdminState>,
    Path((_name, build_id)): Path<(String, String)>,
) -> axum::response::Response {
    match state.registry.get_build(&build_id) {
        Ok(Some(build)) => (
            StatusCode::OK,
            Json(json!({
                "id": build.id,
                "function_name": build.function_name,
                "revision": build.revision,
                "mode": build.mode,
                "status": build.status,
                "exit_code": build.exit_code,
                "started_at": build.started_at,
                "finished_at": build.finished_at,
            })),
        )
            .into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "build not found"),
        Err(err) => store_error_response(&err),
    }
}

#[derive(Debug, Deserialize)]
struct FollowQuery {
    #[serde(default)]
    follow: Option<bool>,
}

/// `GET /v1/functions/{name}/builds/{build_id}/log[?follow=true]`. Without
/// `follow`, returns the log file's current contents as a single response
/// (T041's original MVP behavior). With `follow=true` (T081/US5), streams
/// the file's growth in a chunked response until the build is no longer
/// `running`, per admin-api.md.
async fn get_build_log(
    State(state): State<AdminState>,
    Path((_name, build_id)): Path<(String, String)>,
    Query(query): Query<FollowQuery>,
) -> axum::response::Response {
    let build = match state.registry.get_build(&build_id) {
        Ok(Some(build)) => build,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "build not found"),
        Err(err) => return store_error_response(&err),
    };

    if !query.follow.unwrap_or(false) {
        return match tokio::fs::read_to_string(&build.log_path).await {
            Ok(contents) => (StatusCode::OK, contents).into_response(),
            Err(err) => api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
                format!("failed to read build log: {err}"),
            ),
        };
    }

    let registry = Arc::clone(&state.registry);
    let log_path = std::path::PathBuf::from(build.log_path);
    let state_tuple = (registry, build_id, log_path, 0u64, build.status);
    let stream = futures_util::stream::unfold(state_tuple, |state| async move {
        follow_build_log_step(state).await
    });
    let body = axum::body::Body::from_stream(stream.map(Ok::<_, std::io::Error>));
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

/// One step of the `follow=true` build-log stream: reads whatever new bytes
/// have been appended to the log file since `offset`, yielding them if
/// non-empty; if there's nothing new yet and the build has already finished
/// (`status != Running`), ends the stream; otherwise waits briefly and
/// re-checks the build's status before trying again.
async fn follow_build_log_step(
    (registry, build_id, log_path, mut offset, mut status): (
        Arc<RegistryService>,
        String,
        std::path::PathBuf,
        u64,
        BuildStatus,
    ),
) -> Option<(
    Vec<u8>,
    (
        Arc<RegistryService>,
        String,
        std::path::PathBuf,
        u64,
        BuildStatus,
    ),
)> {
    loop {
        let chunk = read_new_bytes(&log_path, &mut offset).await;
        if !chunk.is_empty() {
            return Some((chunk, (registry, build_id, log_path, offset, status)));
        }
        if status != BuildStatus::Running {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Ok(Some(build)) = registry.get_build(&build_id) {
            status = build.status;
        }
    }
}

/// Reads any bytes appended to the file at `path` since `*offset`, advancing
/// `*offset` past them. A file that doesn't exist yet (the builder hasn't
/// created it) or a transient read error is treated as "nothing new yet"
/// rather than a stream-ending error, since `follow_build_log_step`'s caller
/// only stops on the build's own recorded status.
async fn read_new_bytes(path: &std::path::Path, offset: &mut u64) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return Vec::new();
    };
    if file.seek(std::io::SeekFrom::Start(*offset)).await.is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    match file.read_to_end(&mut buf).await {
        Ok(n) => {
            *offset += n as u64;
            buf
        }
        Err(_) => Vec::new(),
    }
}

#[derive(Debug, Deserialize)]
struct LogsQuery {
    #[serde(default)]
    tail: Option<usize>,
    #[serde(default)]
    follow: Option<bool>,
}

/// `GET /v1/functions/{name}/logs?tail=100&follow=false` (T081/US5): the
/// function's recent log lines from its ring buffer (T079), as
/// `application/x-ndjson` -- one JSON-encoded [`LogRecord`] per line.
/// `follow=true` keeps the connection open and streams new lines as they
/// arrive (a lagged follower, per `tokio::sync::broadcast`'s semantics,
/// silently skips ahead rather than erroring the stream).
async fn get_function_logs(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Query(query): Query<LogsQuery>,
) -> axum::response::Response {
    match state.registry.get(&name) {
        Ok(None) => return function_not_found(&name),
        Err(err) => return store_error_response(&err),
        Ok(Some(_)) => {}
    }

    let buffer = state.registry.log_buffer(&name);
    let initial = buffer.tail(query.tail.unwrap_or(100));

    if !query.follow.unwrap_or(false) {
        let mut body = Vec::new();
        for record in &initial {
            append_ndjson_line(&mut body, record);
        }
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/x-ndjson")],
            body,
        )
            .into_response();
    }

    let rx = buffer.subscribe();
    let live = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(record) => return Some((record, rx)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let body_stream = futures_util::stream::iter(initial)
        .chain(live)
        .map(|record| {
            let mut line = Vec::new();
            append_ndjson_line(&mut line, &record);
            Ok::<_, std::io::Error>(line)
        });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        axum::body::Body::from_stream(body_stream),
    )
        .into_response()
}

fn append_ndjson_line(buf: &mut Vec<u8>, record: &LogRecord) {
    if let Ok(mut line) = serde_json::to_vec(record) {
        buf.append(&mut line);
        buf.push(b'\n');
    }
}

/// `POST /v1/functions/{name}:stop` (T081/US5): forces every running
/// instance of `name` to stop (scale to zero), without touching the
/// function's registration, bindings, or artifacts.
async fn stop_function(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> axum::response::Response {
    match state.registry.stop_instances(&name).await {
        Ok(()) => (StatusCode::OK, Json(json!({"stopped": true}))).into_response(),
        Err(StopError::NotFound(name)) => function_not_found(&name),
        Err(StopError::Store(err)) => store_error_response(&err),
    }
}

fn function_not_found(name: &str) -> axum::response::Response {
    api_error(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        format!("function {name:?} not found"),
    )
}

fn store_error_response(err: &StoreError) -> axum::response::Response {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        err.to_string(),
    )
}

fn function_summary_json(f: &Function) -> serde_json::Value {
    json!({
        "name": f.name,
        "trigger": f.trigger,
        "source_kind": match &f.source {
            Source::Dir { .. } => "dir",
            Source::Image { .. } => "image",
        },
        "state": f.state,
        "current_revision": f.current_revision,
        "updated_at": f.updated_at,
    })
}

fn function_detail_json(
    state: &AdminState,
    f: &Function,
    instances: usize,
    binding: Option<open_functions_core::model::TriggerBinding>,
    current_build_id: Option<String>,
) -> serde_json::Value {
    let path_url = format!("{}/{}", state.invoke_base_url.trim_end_matches('/'), f.name);
    let host_url = state.host_suffix.as_ref().map(|suffix| {
        // Reuse the invoke base URL's scheme://port, swap in the host-based name.
        let scheme_and_port = state
            .invoke_base_url
            .split_once("://")
            .map(|(_, rest)| rest.rsplit_once(':').map(|(_, p)| p).unwrap_or(""))
            .unwrap_or("");
        if scheme_and_port.is_empty() {
            format!("http://{}.{suffix}", f.name)
        } else {
            format!("http://{}.{suffix}:{scheme_and_port}", f.name)
        }
    });

    json!({
        "name": f.name,
        "trigger": f.trigger,
        "source": f.source,
        "entry_point": f.entry_point,
        "env": f.env,
        "timeout_secs": f.timeout_secs,
        "concurrency": f.concurrency,
        "memory_mib": f.memory_mib,
        "min_instances": f.min_instances,
        "max_instances": f.max_instances,
        "idle_timeout_secs": f.idle_timeout_secs,
        "queue_policy": f.queue_policy,
        "queue_max_wait_secs": f.queue_max_wait_secs,
        "state": f.state,
        "current_revision": f.current_revision,
        "current_build_id": current_build_id,
        "last_error": f.last_error,
        "urls": {
            "path": path_url,
            "host": host_url,
        },
        "instances_running": instances,
        "binding": binding,
        "created_at": f.created_at,
        "updated_at": f.updated_at,
    })
}
