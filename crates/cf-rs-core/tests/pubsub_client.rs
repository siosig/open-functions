//! Integration tests for `PsRsClient` against a mocked ps-rs REST surface
//! (`wiremock`), per plan.md's "core integration" testing strategy row
//! ("PubSubBinding (`wiremock` mocks ps-rs; 409/404/connection failure)").
//!
//! Note: this crate promotes `clippy::unwrap_used`/`clippy::expect_used` to
//! warnings (errors under `-D warnings`), and that applies to test targets
//! too. `ok()`/`some()`/`err()` below stand in for `.unwrap()`/`.expect()`
//! without tripping those lints.

use std::net::TcpListener;
use std::time::Duration;

use cf_rs_core::pubsub::client::{PsRsClient, PubSubError, PushConfig, SubscriptionRequest};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
    result.unwrap_or_else(|e| panic!("{context}: {e:?}"))
}

fn some<T>(option: Option<T>, context: &str) -> T {
    option.unwrap_or_else(|| panic!("{context}: expected Some, got None"))
}

fn err<T: std::fmt::Debug, E>(result: Result<T, E>, context: &str) -> E {
    match result {
        Ok(v) => panic!("{context}: expected Err, got Ok({v:?})"),
        Err(e) => e,
    }
}

const PROJECT: &str = "test-project";

fn client_for(base_url: String) -> PsRsClient {
    PsRsClient::new(base_url, PROJECT.to_string(), Duration::from_secs(5))
}

fn sample_request() -> SubscriptionRequest {
    SubscriptionRequest {
        topic: "projects/test-project/topics/my-topic".to_string(),
        push_config: PushConfig {
            push_endpoint: "http://127.0.0.1:8080/_cf/push/my-fn".to_string(),
        },
        ack_deadline_seconds: 30,
    }
}

fn sample_subscription_json(push_endpoint: &str) -> Value {
    json!({
        "name": "projects/test-project/subscriptions/cf-rs-my-fn",
        "topic": "projects/test-project/topics/my-topic",
        "pushConfig": {"pushEndpoint": push_endpoint},
        "ackDeadlineSeconds": 30,
    })
}

/// Binds and immediately drops a TCP listener so the returned URL points at
/// a port nothing is listening on -- a reliable way to force a
/// connection-refused error without depending on a mock server's lifetime.
fn unused_local_url() -> String {
    let listener = ok(TcpListener::bind("127.0.0.1:0"), "bind ephemeral port");
    let addr = ok(listener.local_addr(), "read local addr");
    drop(listener);
    format!("http://{addr}")
}

// ---- create_subscription ----

#[tokio::test]
async fn create_subscription_2xx_returns_ok() {
    let server = MockServer::start().await;
    let req = sample_request();

    Mock::given(method("PUT"))
        .and(path(format!(
            "/v1/projects/{PROJECT}/subscriptions/cf-rs-my-fn"
        )))
        .and(body_json(json!({
            "topic": req.topic,
            "pushConfig": {"pushEndpoint": req.push_config.push_endpoint},
            "ackDeadlineSeconds": req.ack_deadline_seconds,
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(sample_subscription_json(&req.push_config.push_endpoint)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(server.uri());
    let sub = ok(
        client.create_subscription("cf-rs-my-fn", &req).await,
        "create_subscription",
    );

    assert_eq!(sub.name, "projects/test-project/subscriptions/cf-rs-my-fn");
    assert_eq!(sub.topic, req.topic);
    assert_eq!(
        some(sub.push_config, "push_config").push_endpoint,
        req.push_config.push_endpoint
    );
    assert_eq!(sub.ack_deadline_seconds, 30);
}

#[tokio::test]
async fn create_subscription_409_returns_http_error() {
    let server = MockServer::start().await;
    let req = sample_request();

    Mock::given(method("PUT"))
        .and(path(format!(
            "/v1/projects/{PROJECT}/subscriptions/cf-rs-my-fn"
        )))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": {"code": 409, "message": "already exists", "status": "ALREADY_EXISTS"}
        })))
        .mount(&server)
        .await;

    let client = client_for(server.uri());
    let e = err(
        client.create_subscription("cf-rs-my-fn", &req).await,
        "create_subscription should fail on 409",
    );

    match e {
        PubSubError::Http { status, body } => {
            assert_eq!(status, 409);
            assert!(body.contains("ALREADY_EXISTS"), "body={body}");
        }
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[tokio::test]
async fn create_subscription_unreachable_returns_unreachable_error() {
    let client = client_for(unused_local_url());
    let e = err(
        client
            .create_subscription("cf-rs-my-fn", &sample_request())
            .await,
        "create_subscription should fail when server is unreachable",
    );

    assert!(
        matches!(e, PubSubError::Unreachable { .. }),
        "expected Unreachable, got {e:?}"
    );
}

// ---- get_subscription ----

#[tokio::test]
async fn get_subscription_200_returns_some_with_fields_populated() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/projects/{PROJECT}/subscriptions/cf-rs-my-fn"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(sample_subscription_json("http://example.invalid/push")),
        )
        .mount(&server)
        .await;

    let client = client_for(server.uri());
    let sub = some(
        ok(
            client.get_subscription("cf-rs-my-fn").await,
            "get_subscription",
        ),
        "expected Some(subscription)",
    );

    assert_eq!(sub.name, "projects/test-project/subscriptions/cf-rs-my-fn");
    assert_eq!(sub.topic, "projects/test-project/topics/my-topic");
    assert_eq!(
        some(sub.push_config, "push_config").push_endpoint,
        "http://example.invalid/push"
    );
    assert_eq!(sub.ack_deadline_seconds, 30);
}

#[tokio::test]
async fn get_subscription_404_returns_none() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/projects/{PROJECT}/subscriptions/cf-rs-missing"
        )))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}
        })))
        .mount(&server)
        .await;

    let client = client_for(server.uri());
    let sub = ok(
        client.get_subscription("cf-rs-missing").await,
        "get_subscription on 404 should be Ok",
    );

    assert!(sub.is_none());
}

#[tokio::test]
async fn get_subscription_unreachable_returns_unreachable_error() {
    let client = client_for(unused_local_url());
    let e = err(
        client.get_subscription("cf-rs-my-fn").await,
        "get_subscription should fail when server is unreachable",
    );

    assert!(
        matches!(e, PubSubError::Unreachable { .. }),
        "expected Unreachable, got {e:?}"
    );
}

// ---- delete_subscription ----

#[tokio::test]
async fn delete_subscription_204_returns_ok() {
    let server = MockServer::start().await;

    Mock::given(method("DELETE"))
        .and(path(format!(
            "/v1/projects/{PROJECT}/subscriptions/cf-rs-my-fn"
        )))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let client = client_for(server.uri());
    ok(
        client.delete_subscription("cf-rs-my-fn").await,
        "delete_subscription on 204 should be Ok",
    );
}

#[tokio::test]
async fn delete_subscription_404_is_treated_as_success() {
    let server = MockServer::start().await;

    Mock::given(method("DELETE"))
        .and(path(format!(
            "/v1/projects/{PROJECT}/subscriptions/cf-rs-missing"
        )))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}
        })))
        .mount(&server)
        .await;

    let client = client_for(server.uri());
    ok(
        client.delete_subscription("cf-rs-missing").await,
        "delete_subscription on 404 should be treated as success",
    );
}

// ---- recreate_subscription (the push-config "update" path) ----

#[tokio::test]
async fn recreate_subscription_deletes_then_puts() {
    let server = MockServer::start().await;
    let req = SubscriptionRequest {
        push_config: PushConfig {
            push_endpoint: "http://127.0.0.1:9090/_cf/push/my-fn".to_string(),
        },
        ..sample_request()
    };

    Mock::given(method("DELETE"))
        .and(path(format!(
            "/v1/projects/{PROJECT}/subscriptions/cf-rs-my-fn"
        )))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!(
            "/v1/projects/{PROJECT}/subscriptions/cf-rs-my-fn"
        )))
        .and(body_json(json!({
            "topic": req.topic,
            "pushConfig": {"pushEndpoint": req.push_config.push_endpoint},
            "ackDeadlineSeconds": req.ack_deadline_seconds,
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(sample_subscription_json(&req.push_config.push_endpoint)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(server.uri());
    let sub = ok(
        client.recreate_subscription("cf-rs-my-fn", &req).await,
        "recreate_subscription",
    );

    assert_eq!(
        some(sub.push_config, "push_config").push_endpoint,
        "http://127.0.0.1:9090/_cf/push/my-fn"
    );
}
