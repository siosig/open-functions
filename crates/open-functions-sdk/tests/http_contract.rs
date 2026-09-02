//! Contract tests for `open-functions-sdk`'s HTTP routing (T011), per
//! `specs/001-cloud-functions-local/contracts/function-contract.md` "HTTP functions" and
//! "Execution ID and logging".
//!
//! Panicking via `unwrap`/`expect` on setup failures is the desired behavior in tests
//! (it fails the test with a clear message), so the crate-wide `unwrap_used`/
//! `expect_used` lints are relaxed here, matching `crates/open-functions/tests/config.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use open_functions_sdk::{Error, Functions, HttpRequest, HttpResponse};
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt as _;

/// Env vars (`FUNCTION_TARGET`, `FUNCTION_SIGNATURE_TYPE`) are process-global, so tests
/// that touch them must not run concurrently.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

fn set_target(target: &str) {
    // SAFETY: serialized by ENV_MUTEX; no other thread reads/writes env vars concurrently.
    unsafe { std::env::set_var("FUNCTION_TARGET", target) };
}

fn clear_env() {
    // SAFETY: serialized by ENV_MUTEX.
    unsafe {
        std::env::remove_var("FUNCTION_TARGET");
        std::env::remove_var("FUNCTION_SIGNATURE_TYPE");
    }
}

async fn echo_handler(req: HttpRequest) -> HttpResponse {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or_default().to_string();
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("x-echo-path", path)
        .header("x-echo-query", query)
        .body(Body::from("ok"))
        .unwrap_or_default()
}

// The `ENV_MUTEX` guard is deliberately held across `.await` points to serialize this
// test's env-var access against every other test in this file for the whole request
// lifecycle; the lock is only ever used here (never nested with another lock), so
// there's no deadlock risk, just clippy's generic "don't block an executor thread"
// caution.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn robots_txt_and_favicon_are_404_regardless_of_target() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    set_target("hello");

    let app = Functions::new()
        .http("hello", echo_handler)
        .router()
        .expect("router should resolve");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/robots.txt")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/favicon.ico")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    clear_env();
}

// See the comment on `robots_txt_and_favicon_are_404_regardless_of_target` above.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn registered_handler_receives_full_request_unmodified() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    set_target("hello");

    let app = Functions::new()
        .http("hello", echo_handler)
        .router()
        .expect("router should resolve");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/some/path?x=1")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("x-echo-path").expect("header"),
        "/some/path"
    );
    assert_eq!(resp.headers().get("x-echo-query").expect("header"), "x=1");

    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .expect("collect body")
        .to_bytes();
    assert_eq!(&body[..], b"ok");

    clear_env();
}

#[tokio::test]
async fn unregistered_target_is_missing_target_error() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    set_target("does-not-exist");

    let functions = Functions::new().http("hello", echo_handler);
    let err = functions.router().expect_err("should fail to resolve");

    match err {
        Error::MissingTarget { target } => {
            assert_eq!(target.as_deref(), Some("does-not-exist"));
        }
        other => panic!("expected MissingTarget, got {other:?}"),
    }

    clear_env();
}

#[tokio::test]
async fn signature_mismatch_when_cloudevent_requested_for_http_target() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    set_target("hello");
    // SAFETY: serialized by ENV_MUTEX.
    unsafe { std::env::set_var("FUNCTION_SIGNATURE_TYPE", "cloudevent") };

    let functions = Functions::new().http("hello", echo_handler);
    let err = functions.router().expect_err("should fail to resolve");

    match err {
        Error::SignatureMismatch {
            target,
            configured,
            actual,
        } => {
            assert_eq!(target, "hello");
            assert_eq!(configured, "cloudevent");
            assert_eq!(actual, "http");
        }
        other => panic!("expected SignatureMismatch, got {other:?}"),
    }

    clear_env();
}

/// A minimal in-test `tracing_subscriber::Layer` that records the `execution_id` field
/// of the current span for every event it observes, so this test can assert the
/// `Function-Execution-Id` header actually reaches the tracing span used for structured
/// logging (function-contract.md "Execution ID and logging"), without depending on `logging::init`
/// (which installs a *process global* subscriber open-functions-sdk itself must own) or adding a
/// new `tracing-test` dependency.
mod capture {
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::registry::LookupSpan;

    pub struct ExecutionIdField(pub String);

    pub struct CapturingLayer(pub Arc<Mutex<Vec<String>>>);

    impl<S> Layer<S> for CapturingLayer
    where
        S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: Context<'_, S>,
        ) {
            let Some(span) = ctx.span(id) else {
                return;
            };
            struct Visitor(Option<String>);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "execution_id" {
                        self.0 = Some(format!("{value:?}"));
                    }
                }
            }
            let mut visitor = Visitor(None);
            attrs.record(&mut visitor);
            if let Some(execution_id) = visitor.0 {
                span.extensions_mut().insert(ExecutionIdField(execution_id));
            }
        }

        fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
            if let Some(scope) = ctx.event_scope(event) {
                for span in scope.from_root() {
                    if let Some(id) = span.extensions().get::<ExecutionIdField>() {
                        self.0
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(id.0.clone());
                    }
                }
            }
        }
    }
}

// `flavor = "current_thread"` keeps the whole test (including polling the router's
// future) on one OS thread, which `tracing::subscriber::set_default`'s thread-local
// guard requires — a multi-thread executor could otherwise poll the future on a worker
// thread that never saw `set_default`.
// See the comment on `robots_txt_and_favicon_are_404_regardless_of_target` above.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "current_thread")]
async fn execution_id_header_reaches_tracing_span_and_does_not_break_handling() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    set_target("hello");

    async fn probing_handler(_req: HttpRequest) -> HttpResponse {
        tracing::info!("probe");
        axum::response::Response::builder()
            .status(StatusCode::OK)
            .body(Body::from("ok"))
            .unwrap_or_default()
    }

    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let layer = capture::CapturingLayer(captured.clone());
    let subscriber = tracing_subscriber::registry().with(layer);

    let app = Functions::new()
        .http("hello", probing_handler)
        .router()
        .expect("router should resolve");

    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/some/path")
                .header("Function-Execution-Id", "test-exec-123")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        captured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|id| id == "test-exec-123"),
        "expected execution_id \"test-exec-123\" to have been recorded on the tracing span, got {:?}",
        captured.lock().unwrap_or_else(|e| e.into_inner())
    );

    drop(_subscriber_guard);
    clear_env();
}
