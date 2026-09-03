//! Integration tests for the Pub/Sub `TriggerBinding` reconciler (T046),
//! using `wiremock` to stand in for open-pubusb.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use open_functions_core::model::binding::BindingState;
use open_functions_core::pubsub::client::OpenPubusbClient;
use open_functions_core::pubsub::reconcile::{DesiredBinding, Reconciler};
use open_functions_core::registry::memory::MemoryStore;
use open_functions_core::registry::store::Store;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn desired(function_name: &str) -> DesiredBinding {
    DesiredBinding {
        function_name: function_name.to_string(),
        project: "local".to_string(),
        topic: "orders".to_string(),
        push_endpoint: "http://127.0.0.1:8080/_cf/push/on-orders".to_string(),
        ack_deadline_seconds: 70,
    }
}

fn reconciler(server: &MockServer, store: Arc<dyn Store>) -> Reconciler {
    let client = OpenPubusbClient::new(server.uri(), "local".to_string(), Duration::from_secs(5));
    Reconciler::new(
        client,
        store,
        Duration::from_millis(50),
        Duration::from_millis(200),
    )
}

#[tokio::test]
async fn unreachable_then_backoff_then_bound() {
    let server = MockServer::start().await;
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let recon = reconciler(&server, Arc::clone(&store));
    let d = desired("on-orders");

    // open-pubusb not mocked at all yet -> connection refused (wiremock server is
    // up, but we point the client at a different, unused port to simulate
    // "unreachable" cleanly).
    let dead_client = OpenPubusbClient::new(
        "http://127.0.0.1:1".to_string(),
        "local".to_string(),
        Duration::from_millis(200),
    );
    let dead_recon = Reconciler::new(
        dead_client,
        Arc::clone(&store),
        Duration::from_millis(50),
        Duration::from_millis(200),
    );
    let binding = dead_recon.try_bind(&d).await.expect("try_bind");
    assert_eq!(binding.state, BindingState::Pending);
    assert!(binding.next_retry_at.is_some());

    // Now open-pubusb is reachable (via the real mock server) and accepts the PUT.
    Mock::given(method("PUT"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "projects/local/subscriptions/open-functions-on-orders",
            "topic": "projects/local/topics/orders",
            "pushConfig": {"pushEndpoint": d.push_endpoint},
            "ackDeadlineSeconds": 70
        })))
        .mount(&server)
        .await;

    let bound = recon.try_bind(&d).await.expect("try_bind");
    assert_eq!(bound.state, BindingState::Bound);
    assert_eq!(
        bound.subscription,
        "projects/local/subscriptions/open-functions-on-orders"
    );

    let stored = store
        .get_binding("on-orders")
        .expect("get_binding")
        .expect("binding should be stored");
    assert_eq!(stored.state, BindingState::Bound);
}

#[tokio::test]
async fn conflict_with_matching_push_endpoint_is_treated_as_bound() {
    let server = MockServer::start().await;
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let recon = reconciler(&server, Arc::clone(&store));
    let d = desired("on-orders");

    Mock::given(method("PUT"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "projects/local/subscriptions/open-functions-on-orders",
            "topic": "projects/local/topics/orders",
            "pushConfig": {"pushEndpoint": d.push_endpoint},
            "ackDeadlineSeconds": 70
        })))
        .mount(&server)
        .await;

    let binding = recon.try_bind(&d).await.expect("try_bind");
    assert_eq!(binding.state, BindingState::Bound);
}

#[tokio::test]
async fn conflict_with_mismatched_push_endpoint_recreates() {
    let server = MockServer::start().await;
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let recon = reconciler(&server, Arc::clone(&store));
    let d = desired("on-orders");

    Mock::given(method("PUT"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(409))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "projects/local/subscriptions/open-functions-on-orders",
            "topic": "projects/local/topics/orders",
            "pushConfig": {"pushEndpoint": "http://stale-endpoint/"},
            "ackDeadlineSeconds": 70
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "projects/local/subscriptions/open-functions-on-orders",
            "topic": "projects/local/topics/orders",
            "pushConfig": {"pushEndpoint": d.push_endpoint},
            "ackDeadlineSeconds": 70
        })))
        .mount(&server)
        .await;

    let binding = recon.try_bind(&d).await.expect("try_bind");
    assert_eq!(binding.state, BindingState::Bound);
    assert_eq!(binding.push_endpoint, d.push_endpoint);
}

#[tokio::test]
async fn permanent_4xx_is_error_not_retried() {
    let server = MockServer::start().await;
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let recon = reconciler(&server, Arc::clone(&store));
    let d = desired("on-orders");

    Mock::given(method("PUT"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(400).set_body_string("topic not found"))
        .mount(&server)
        .await;

    let binding = recon.try_bind(&d).await.expect("try_bind");
    assert_eq!(binding.state, BindingState::Error);
    assert!(binding.next_retry_at.is_none());
}

#[tokio::test]
async fn unbind_success_removes_binding_entirely() {
    let server = MockServer::start().await;
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let recon = reconciler(&server, Arc::clone(&store));
    let d = desired("on-orders");

    Mock::given(method("PUT"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "projects/local/subscriptions/open-functions-on-orders",
            "topic": "projects/local/topics/orders",
            "pushConfig": {"pushEndpoint": d.push_endpoint},
            "ackDeadlineSeconds": 70
        })))
        .mount(&server)
        .await;
    recon.try_bind(&d).await.expect("try_bind");
    assert!(store.get_binding("on-orders").expect("get").is_some());

    Mock::given(method("DELETE"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    recon.try_unbind("on-orders").await.expect("try_unbind");
    assert!(store.get_binding("on-orders").expect("get").is_none());
}

#[tokio::test]
async fn unbind_failure_persists_unbinding_state_for_retry() {
    let dead_client = OpenPubusbClient::new(
        "http://127.0.0.1:1".to_string(),
        "local".to_string(),
        Duration::from_millis(200),
    );
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let recon = Reconciler::new(
        dead_client,
        Arc::clone(&store),
        Duration::from_millis(50),
        Duration::from_millis(200),
    );

    // Pre-seed a Bound binding as if it had been created earlier.
    store
        .put_binding(&open_functions_core::model::binding::TriggerBinding {
            function_name: "on-orders".to_string(),
            subscription: "projects/local/subscriptions/open-functions-on-orders".to_string(),
            topic: "orders".to_string(),
            push_endpoint: "http://127.0.0.1:8080/_cf/push/on-orders".to_string(),
            state: BindingState::Bound,
            last_error: None,
            next_retry_at: None,
        })
        .expect("seed binding");

    recon.try_unbind("on-orders").await.expect("try_unbind");

    let stored = store
        .get_binding("on-orders")
        .expect("get")
        .expect("binding should still be tracked (unbinding, not removed)");
    assert_eq!(stored.state, BindingState::Unbinding);
    assert!(stored.next_retry_at.is_some());
}

#[tokio::test]
async fn sweep_once_retries_due_pending_binding() {
    let server = MockServer::start().await;
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let recon = Arc::new(reconciler(&server, Arc::clone(&store)));

    // Seed a Pending binding whose retry is already due.
    store
        .put_binding(&open_functions_core::model::binding::TriggerBinding {
            function_name: "on-orders".to_string(),
            subscription: "open-functions-on-orders".to_string(),
            topic: "orders".to_string(),
            push_endpoint: "http://127.0.0.1:8080/_cf/push/on-orders".to_string(),
            state: BindingState::Pending,
            last_error: Some("previously unreachable".to_string()),
            next_retry_at: Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
        })
        .expect("seed binding");

    Mock::given(method("PUT"))
        .and(path(
            "/v1/projects/local/subscriptions/open-functions-on-orders",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "projects/local/subscriptions/open-functions-on-orders",
            "topic": "projects/local/topics/orders",
            "pushConfig": {"pushEndpoint": "http://127.0.0.1:8080/_cf/push/on-orders"},
            "ackDeadlineSeconds": 70
        })))
        .mount(&server)
        .await;

    recon.sweep_once(70, "local").await;

    let stored = store
        .get_binding("on-orders")
        .expect("get")
        .expect("binding present");
    assert_eq!(stored.state, BindingState::Bound);
}
