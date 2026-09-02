//! Minimal HTTP function using `cf-rs-sdk`. Runs unmodified on cf-rs
//! (`cf-rs fn deploy hello --source examples/hello-http --entry-point hello`)
//! and on Google Cloud Run functions (deploy this directory as a container).
//!
//! Test-only behavior, controlled by env vars (used by cf-rs's own test
//! suite, harmless in normal use):
//! - `FAIL=1`: returns 500 instead of handling the request.
//! - `CRASH=1`: exits the process immediately (simulates an instance crash).
//! - `SLEEP_MS=<n>`: sleeps for `n` milliseconds before responding.

use cf_rs_sdk::{Functions, HttpRequest, HttpResponse};

#[tokio::main]
async fn main() -> Result<(), cf_rs_sdk::Error> {
    Functions::new().http("hello", hello).serve().await
}

async fn hello(req: HttpRequest) -> HttpResponse {
    if std::env::var("CRASH").is_ok() {
        tracing::error!("simulating a crash on request");
        std::process::exit(1);
    }

    if let Ok(ms) = std::env::var("SLEEP_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
    }

    if std::env::var("FAIL").is_ok() {
        tracing::warn!("simulating a failure on request");
        return match axum::http::Response::builder()
            .status(500)
            .body(axum::body::Body::from("simulated failure"))
        {
            Ok(resp) => resp,
            // A fixed status code and a plain byte body can't actually fail to
            // build; this arm only exists to avoid unwrap/expect.
            Err(_) => HttpResponse::default(),
        };
    }

    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    tracing::info!(%path, %query, "handling request");

    let body = if query.is_empty() {
        format!("Hello {path}")
    } else {
        format!("Hello {path}?{query}")
    };

    HttpResponse::new(axum::body::Body::from(body))
}
