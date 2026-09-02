//! Contract tests for `cf-rs-sdk`'s CloudEvent routing (T047), per
//! `specs/001-cloud-functions-local/contracts/function-contract.md` "CloudEvents functions"
//! and "Pub/Sub Push → CloudEvent conversion".
//!
//! Panicking via `unwrap`/`expect` on setup failures is the desired behavior in tests
//! (it fails the test with a clear message), so the crate-wide `unwrap_used`/
//! `expect_used` lints are relaxed here, matching `crates/cf-rs-sdk/tests/http_contract.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use cf_rs_sdk::pubsub::MessagePublishedData;
use cf_rs_sdk::{CloudEvent, CloudEventExt, Error, Functions};
use cloudevents::AttributesReader;
use tower::ServiceExt;

/// Env vars (`FUNCTION_TARGET`, `FUNCTION_SIGNATURE_TYPE`) are process-global, so tests
/// that touch them must not run concurrently.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

fn set_target(target: &str) {
    // SAFETY: serialized by ENV_MUTEX; no other thread reads/writes env vars concurrently.
    unsafe { std::env::set_var("FUNCTION_TARGET", target) };
}

fn set_signature(signature: &str) {
    // SAFETY: serialized by ENV_MUTEX.
    unsafe { std::env::set_var("FUNCTION_SIGNATURE_TYPE", signature) };
}

fn clear_env() {
    // SAFETY: serialized by ENV_MUTEX.
    unsafe {
        std::env::remove_var("FUNCTION_TARGET");
        std::env::remove_var("FUNCTION_SIGNATURE_TYPE");
    }
}

/// `message.data` = base64("hello"), a Pub/Sub-shaped body per function-contract.md's
/// "CloudEvents functions" example, using GCP's real camelCase wire field names
/// (`messageId`/`publishTime`/`orderingKey`) — the same spelling
/// `cf-rs-core`'s host-side Pub/Sub → CloudEvent conversion emits (see
/// `crates/cf-rs-core/src/pubsub/convert.rs`) and real Cloud Pub/Sub sends.
const PUBSUB_DATA_JSON: &str = r#"{"message":{"data":"aGVsbG8=","attributes":{"k":"v"},"messageId":"1234567890","publishTime":"2026-09-02T01:02:03.456Z","orderingKey":""},"subscription":"projects/local/subscriptions/cf-rs-on-msg"}"#;

fn assert_decodes_pubsub_data(ev: &CloudEvent) {
    let data: MessagePublishedData = ev.data_as().expect("decode MessagePublishedData");
    assert_eq!(data.message.data, b"hello");
    assert_eq!(data.message.message_id, "1234567890");
    assert_eq!(data.message.attributes.get("k"), Some(&"v".to_string()));
    assert_eq!(
        data.subscription,
        "projects/local/subscriptions/cf-rs-on-msg"
    );
}

// The `ENV_MUTEX` guard is deliberately held across `.await` points to serialize this
// test's env-var access against every other test in this file for the whole request
// lifecycle; the lock is only ever used here (never nested with another lock), so
// there's no deadlock risk, just clippy's generic "don't block an executor thread"
// caution.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn binary_mode_cloudevent_reaches_handler_with_correct_attributes_and_data() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    set_target("on_msg");
    set_signature("cloudevent");

    let app = Functions::new()
        .cloud_event("on_msg", |ev: CloudEvent| async move {
            assert_eq!(ev.id(), "1234567890");
            assert_eq!(
                ev.source().to_string(),
                "//pubsub.googleapis.com/projects/local/topics/orders"
            );
            assert_eq!(ev.ty(), "google.cloud.pubsub.topic.v1.messagePublished");
            assert_decodes_pubsub_data(&ev);
            Ok::<(), std::convert::Infallible>(())
        })
        .router()
        .expect("router should resolve");

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("ce-specversion", "1.0")
                .header("ce-id", "1234567890")
                .header(
                    "ce-source",
                    "//pubsub.googleapis.com/projects/local/topics/orders",
                )
                .header("ce-type", "google.cloud.pubsub.topic.v1.messagePublished")
                .header("ce-time", "2026-09-02T01:02:03.456Z")
                .header("content-type", "application/json")
                .body(Body::from(PUBSUB_DATA_JSON))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(resp.status(), StatusCode::OK);

    clear_env();
}

// See the comment on `binary_mode_cloudevent_reaches_handler_with_correct_attributes_and_data`.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn structured_mode_cloudevent_reaches_handler_and_decodes() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    set_target("on_msg");
    set_signature("cloudevent");

    let envelope = serde_json::json!({
        "specversion": "1.0",
        "id": "1234567890",
        "source": "//pubsub.googleapis.com/projects/local/topics/orders",
        "type": "google.cloud.pubsub.topic.v1.messagePublished",
        "time": "2026-09-02T01:02:03.456Z",
        "datacontenttype": "application/json",
        "data": {
            "message": {
                "data": "aGVsbG8=",
                "attributes": {"k": "v"},
                "messageId": "1234567890",
                "publishTime": "2026-09-02T01:02:03.456Z",
                "orderingKey": ""
            },
            "subscription": "projects/local/subscriptions/cf-rs-on-msg"
        }
    });

    let app = Functions::new()
        .cloud_event("on_msg", |ev: CloudEvent| async move {
            assert_eq!(ev.id(), "1234567890");
            assert_decodes_pubsub_data(&ev);
            Ok::<(), std::convert::Infallible>(())
        })
        .router()
        .expect("router should resolve");

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "application/cloudevents+json")
                .body(Body::from(envelope.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(resp.status(), StatusCode::OK);

    clear_env();
}

// See the comment on `binary_mode_cloudevent_reaches_handler_with_correct_attributes_and_data`.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn handler_ok_yields_200() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    set_target("on_msg");
    set_signature("cloudevent");

    let app = Functions::new()
        .cloud_event("on_msg", |_ev: CloudEvent| async move {
            Ok::<(), std::convert::Infallible>(())
        })
        .router()
        .expect("router should resolve");

    let resp = app.oneshot(binary_request()).await.expect("response");

    assert_eq!(resp.status(), StatusCode::OK);

    clear_env();
}

// See the comment on `binary_mode_cloudevent_reaches_handler_with_correct_attributes_and_data`.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn handler_err_yields_500() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    set_target("on_msg");
    set_signature("cloudevent");

    #[derive(Debug, thiserror::Error)]
    #[error("boom")]
    struct BoomError;

    let app = Functions::new()
        .cloud_event("on_msg", |_ev: CloudEvent| async move {
            Err::<(), _>(BoomError)
        })
        .router()
        .expect("router should resolve");

    let resp = app.oneshot(binary_request()).await.expect("response");

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    clear_env();
}

#[tokio::test]
async fn signature_mismatch_when_http_requested_for_cloudevent_target() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    set_target("on_msg");
    set_signature("http");

    let functions = Functions::new().cloud_event("on_msg", |_ev: CloudEvent| async move {
        Ok::<(), std::convert::Infallible>(())
    });
    let err = functions.router().expect_err("should fail to resolve");

    match err {
        Error::SignatureMismatch {
            target,
            configured,
            actual,
        } => {
            assert_eq!(target, "on_msg");
            assert_eq!(configured, "http");
            assert_eq!(actual, "cloudevent");
        }
        other => panic!("expected SignatureMismatch, got {other:?}"),
    }

    clear_env();
}

fn binary_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/")
        .header("ce-specversion", "1.0")
        .header("ce-id", "1234567890")
        .header(
            "ce-source",
            "//pubsub.googleapis.com/projects/local/topics/orders",
        )
        .header("ce-type", "google.cloud.pubsub.topic.v1.messagePublished")
        .header("ce-time", "2026-09-02T01:02:03.456Z")
        .header("content-type", "application/json")
        .body(Body::from(PUBSUB_DATA_JSON))
        .expect("request")
}
