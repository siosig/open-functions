//! Benchmarks the host's own added latency for one forwarded request (T084):
//! `cf_rs_forward_overhead_seconds` per `cf_rs_core::forward`'s doc comments
//! and `ops-config.md`'s metrics table — receive-to-forward-start plus
//! response-received-to-send, deliberately excluding backend processing
//! time. The mock backend below answers as fast as `axum::serve` can manage,
//! so the measured wall-clock time of `Forwarder::forward` approximates the
//! host's own overhead rather than a real function's startup/processing
//! variance (which is what `scripts/qa/coldstart.sh` and the real
//! `hello-http` e2e tests are for).
//!
//! Run with `cargo bench --bench forward --features bench -p cf-rs`
//! (`required-features = ["bench"]` in Cargo.toml skips this target on a
//! plain `cargo bench`, keeping the optional `criterion` dependency off the
//! default build).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use axum::routing::any;
use cf_rs_core::forward::RequestRewriteContext;
use criterion::{Criterion, criterion_group, criterion_main};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

// `cf-rs` has no lib target (binary crate only, see src/main.rs), so pull
// the forwarder in by path exactly as `tests/forward.rs` already does.
#[path = "../src/forward.rs"]
mod forward;
use forward::Forwarder;

const RESPONSE_BODY_LEN: usize = 1000;

async fn spawn_mock_backend() -> SocketAddr {
    let app = axum::Router::new().route(
        "/",
        any(|| async {
            Response::builder()
                .status(200)
                .body(Body::from(vec![b'x'; RESPONSE_BODY_LEN]))
                .expect("build mock response")
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock backend");
    let addr = listener.local_addr().expect("mock backend local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock backend");
    });
    addr
}

fn forward_overhead(c: &mut Criterion) {
    let rt = Runtime::new().expect("build tokio runtime");
    let addr = rt.block_on(spawn_mock_backend());
    let forwarder = Forwarder::new();
    let ctx = RequestRewriteContext {
        execution_id: "bench".to_string(),
        client_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        proto: "http",
        original_host: None,
    };

    c.bench_function("forward_overhead", |b| {
        b.iter(|| {
            rt.block_on(async {
                let req = Request::builder()
                    .method("GET")
                    .uri("/")
                    .body(Body::empty())
                    .expect("build request");
                let result = forwarder
                    .forward(addr, req, &ctx, Duration::from_secs(5))
                    .await;
                // A bench that silently measures failures is worse than one
                // that refuses to run.
                assert!(result.is_ok(), "forward failed during bench: {result:?}");
            });
        });
    });
}

criterion_group!(benches, forward_overhead);
criterion_main!(benches);
