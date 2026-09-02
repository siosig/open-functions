//! Integration tests for the network-facing `Forwarder` (T039), exercising it
//! against real local HTTP servers (a normal echo server, a slow server for
//! timeout testing, and a port nobody's listening on for connection-refused).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::any;
use cf_rs_core::forward::{ForwardFailure, RequestRewriteContext};
use tokio::net::TcpListener;

#[path = "../src/forward.rs"]
mod forward;
use forward::Forwarder;

fn ctx(execution_id: &str) -> RequestRewriteContext {
    RequestRewriteContext {
        execution_id: execution_id.to_string(),
        client_addr: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)),
        proto: "http",
        original_host: Some("fn.local".to_string()),
    }
}

async fn spawn_echo_server() -> SocketAddr {
    let app = axum::Router::new().route(
        "/{*path}",
        any(|req: Request| async move {
            let path = req.uri().path().to_string();
            let query = req.uri().query().unwrap_or("").to_string();
            format!("{path}?{query}").into_response()
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

async fn spawn_slow_server(delay: Duration) -> SocketAddr {
    let app = axum::Router::new().route(
        "/{*path}",
        any(move || async move {
            tokio::time::sleep(delay).await;
            "slow"
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

#[tokio::test]
async fn forwards_request_and_rewrites_response_headers() {
    let addr = spawn_echo_server().await;
    let forwarder = Forwarder::new();

    let req = Request::builder()
        .method("GET")
        .uri("/hello/world?x=1")
        .body(axum::body::Body::empty())
        .expect("build request");

    let resp = forwarder
        .forward(addr, req, &ctx("exec-1"), Duration::from_secs(5))
        .await
        .expect("forward should succeed");

    assert_eq!(
        resp.headers()
            .get("function-execution-id")
            .and_then(|v| v.to_str().ok()),
        Some("exec-1")
    );

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(&body[..], b"/hello/world?x=1");
}

#[tokio::test]
async fn connection_refused_when_nothing_listening() {
    // Bind to get a free port, then drop the listener so nothing is there.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);

    let forwarder = Forwarder::new();
    let req = Request::builder()
        .uri("/x")
        .body(axum::body::Body::empty())
        .expect("build request");

    let result = forwarder
        .forward(addr, req, &ctx("exec-2"), Duration::from_secs(5))
        .await;

    assert_eq!(result.unwrap_err(), ForwardFailure::ConnectionRefused);
}

#[tokio::test]
async fn timeout_when_instance_too_slow() {
    let addr = spawn_slow_server(Duration::from_secs(5)).await;
    let forwarder = Forwarder::new();

    let req = Request::builder()
        .uri("/x")
        .body(axum::body::Body::empty())
        .expect("build request");

    let result = forwarder
        .forward(addr, req, &ctx("exec-3"), Duration::from_millis(100))
        .await;

    assert_eq!(result.unwrap_err(), ForwardFailure::Timeout);
}

#[tokio::test]
async fn client_supplied_execution_id_is_overwritten() {
    let addr = spawn_echo_server().await;
    let forwarder = Forwarder::new();

    let req = Request::builder()
        .uri("/a")
        .header("Function-Execution-Id", "forged")
        .body(axum::body::Body::empty())
        .expect("build request");

    let resp = forwarder
        .forward(addr, req, &ctx("real-id"), Duration::from_secs(5))
        .await
        .expect("forward should succeed");

    assert_eq!(
        resp.headers()
            .get("function-execution-id")
            .and_then(|v| v.to_str().ok()),
        Some("real-id")
    );
}
