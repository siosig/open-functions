# open-functions-sdk

## Table of contents

- [Overview](#overview)
- [Writing an HTTP function](#writing-an-http-function)
- [Writing a CloudEvent (Pub/Sub) function](#writing-a-cloudevent-pubsub-function)
- [Running locally](#running-locally)
- [Deploying to Google Cloud Run functions unmodified](#deploying-to-google-cloud-run-functions-unmodified)
- [Structured logging and execution IDs](#structured-logging-and-execution-ids)
- [Error handling](#error-handling)

## Overview

`open-functions-sdk` is a Rust implementation of the [Google Cloud Functions
Framework](https://github.com/GoogleCloudPlatform/functions-framework)
contract: the same startup environment variables (`PORT`, `FUNCTION_TARGET`,
`FUNCTION_SIGNATURE_TYPE`), the same `http` / `cloudevent` signature types,
and the same GCP structured JSON logging format that Google's own
first-party Functions Framework runtimes implement.

A function written against this SDK runs unmodified in two places:

- **Locally**, hosted by [`open-functions`](https://crates.io/crates/open-functions) (this
  crate's sibling in the `open-functions` workspace), which builds/runs the function
  as a process or a container and proxies requests to it.
- **On Google Cloud Run functions**, deployed as-is via `gcloud run deploy`
  or `gcloud functions deploy` — no code changes, no conditional
  compilation, no open-functions-specific glue.

The API surface is intentionally small: a builder ([`Functions`]) to
register one or more named handlers, and two handler kinds mirroring the
Functions Framework's two signature types — `http` (any [`axum`] handler)
and `cloudevent` (a typed [`CloudEvent`] handler for Eventarc/Pub/Sub
triggers).

[`Functions`]: https://docs.rs/open-functions-sdk/latest/open_functions_sdk/struct.Functions.html
[`axum`]: https://docs.rs/axum
[`CloudEvent`]: https://docs.rs/open-functions-sdk/latest/open_functions_sdk/type.CloudEvent.html

## Writing an HTTP function

```rust
use open_functions_sdk::{Functions, HttpRequest, HttpResponse};

#[tokio::main]
async fn main() -> Result<(), open_functions_sdk::Error> {
    Functions::new().http("hello", hello).serve().await
}

async fn hello(req: HttpRequest) -> HttpResponse {
    let path = req.uri().path().to_string();
    HttpResponse::new(axum::body::Body::from(format!("Hello {path}")))
}
```

`.http(name, handler)` accepts any `axum::handler::Handler` — a plain async
function taking any combination of `axum` extractors (`Json<T>`, `Query<T>`,
`HttpRequest` itself, ...) and returning anything implementing
`axum::response::IntoResponse`. `HttpRequest` and `HttpResponse` are type
aliases for `axum::extract::Request` and `axum::response::Response`, so
existing `axum` knowledge transfers directly; see
[`examples/hello-http`](../../examples/hello-http/src/main.rs) in this
repository for a complete, runnable version of the above.

## Writing a CloudEvent (Pub/Sub) function

```rust
use open_functions_sdk::cloudevent::DataError;
use open_functions_sdk::pubsub::MessagePublishedData;
use open_functions_sdk::{CloudEvent, CloudEventExt, Functions};

#[tokio::main]
async fn main() -> Result<(), open_functions_sdk::Error> {
    Functions::new().cloud_event("on_msg", on_msg).serve().await
}

async fn on_msg(event: CloudEvent) -> Result<(), DataError> {
    let data: MessagePublishedData = event.data_as()?;
    let text = String::from_utf8_lossy(&data.message.data);
    tracing::info!(message = %text, "received pubsub message");
    Ok(())
}
```

`.cloud_event(name, handler)` registers a handler that receives a decoded
[`CloudEvent`] (`cloudevents::Event`) and returns `Ok(())` (the SDK responds
`200`) or `Err(E)` where `E: std::error::Error` (the SDK responds `500` with
the error's `Display` as the body). The `pubsub` module provides
[`MessagePublishedData`] / [`PubsubMessage`], matching the payload shape of
a `google.cloud.pubsub.topic.v1.messagePublished` CloudEvent — decode it
from any event via the [`CloudEventExt::data_as`] extension trait. See
[`examples/hello-pubsub`](../../examples/hello-pubsub/src/main.rs) for a
complete version, including a custom error enum for handlers that need more
than one failure variant (`CloudEventHandler`'s bound requires the error
type itself implement `std::error::Error`, so `Box<dyn Error>` doesn't
qualify directly).

[`MessagePublishedData`]: https://docs.rs/open-functions-sdk/latest/open_functions_sdk/pubsub/struct.MessagePublishedData.html
[`PubsubMessage`]: https://docs.rs/open-functions-sdk/latest/open_functions_sdk/pubsub/struct.PubsubMessage.html
[`CloudEventExt::data_as`]: https://docs.rs/open-functions-sdk/latest/open_functions_sdk/cloudevent/trait.CloudEventExt.html#tymethod.data_as

## Running locally

### Standalone, without open-functions

`Functions::serve()` resolves its configuration entirely from environment
variables, so a function binary can be run directly with `cargo run` —
useful for testing the SDK layer in isolation, without open-functions or a real
Pub/Sub source in the loop:

```bash
PORT=8080 FUNCTION_TARGET=hello FUNCTION_SIGNATURE_TYPE=http \
  cargo run --manifest-path examples/hello-http/Cargo.toml
curl localhost:8080/world
```

For a `cloudevent` target, `FUNCTION_SIGNATURE_TYPE=cloudevent` and a POST
with the CloudEvents binary-mode headers (`ce-specversion`, `ce-id`,
`ce-source`, `ce-type`, ...) reaches the handler the same way open-functions or
Eventarc would deliver it.

### Via open-functions

The normal path is to let `open-functions` build and run the function — it resolves
`PORT` to a free port, sets `FUNCTION_TARGET` / `FUNCTION_SIGNATURE_TYPE`
from the deployment, and proxies HTTP (and, for Pub/Sub triggers,
CloudEvent-converted Push) traffic to it:

```bash
cargo run -p open-functions -- serve --data-dir ./tmp/data
cargo run -p open-functions -- fn deploy hello --source ./examples/hello-http --entry-point hello
curl http://127.0.0.1:8080/hello/world
```

See the workspace root [`README.md`](../../README.md) for the full
`open-functions` CLI walkthrough, including Pub/Sub triggers, container
images, and admin/observability endpoints.

## Deploying to Google Cloud Run functions unmodified

Because the SDK implements the same Functions Framework contract Cloud Run
functions expects, the exact same source directory deploys with zero code
changes:

```bash
gcloud run deploy hello-http --source examples/hello-http --region asia-northeast1 --allow-unauthenticated \
  --set-env-vars FUNCTION_TARGET=hello,FUNCTION_SIGNATURE_TYPE=http
curl "$(gcloud run services describe hello-http --format 'value(status.url)')/world?x=1"
```

Cloud Run sets `PORT` itself (always `8080` in a container) and the
`--set-env-vars` above supply `FUNCTION_TARGET` / `FUNCTION_SIGNATURE_TYPE`,
exactly mirroring what `open-functions` sets when running the function as a
container. For a Pub/Sub-triggered (`cloudevent`) function, wire up an
Eventarc trigger instead of `--set-env-vars FUNCTION_SIGNATURE_TYPE=http`:

```bash
gcloud eventarc triggers create hello-pubsub-trigger \
  --destination-run-service hello-pubsub --location asia-northeast1 \
  --event-filters type=google.cloud.pubsub.topic.v1.messagePublished \
  --transport-topic orders
```

No conditional compilation, feature flags, or SDK configuration differ
between the two targets — the same binary (or container image) is valid
input to both `open-functions fn deploy` and `gcloud run deploy`.

## Structured logging and execution IDs

`Functions::serve()` calls [`logging::init()`] before binding, which
installs a `tracing_subscriber` layer that writes one GCP-structured JSON
object per `tracing` event to stdout:

```json
{"severity":"INFO","message":"hello","time":"2026-09-02T01:02:03.456789Z","logging.googleapis.com/labels":{"execution_id":"3f2b..."}}
```

- `severity` is derived from the `tracing::Level` (`ERROR` → `ERROR`, `WARN`
  → `WARNING`, `INFO` → `INFO`, `DEBUG`/`TRACE` → `DEBUG`), matching GCP
  Cloud Logging's expected severities.
- The filter defaults to `info` and honors `RUST_LOG` the same way `env_logger`/
  `tracing_subscriber::EnvFilter` normally would.
- `execution_id` is populated automatically for any log line emitted while
  handling a request: both `.http()` and `.cloud_event()` routers install
  [`logging::execution_id_middleware`], which reads the inbound
  `Function-Execution-Id` header (set by open-functions, and by Cloud Run's own
  request-handling layer) and enters a tracing span carrying it for the
  duration of that request. Handler code doesn't need to thread the
  execution ID through manually — any `tracing::info!`/`warn!`/`error!`
  call made from within (or below, through normal async call chains) the
  handler picks it up.
- `open-functions fn logs <name>` (or Cloud Logging, on Cloud Run) parses these JSON
  lines directly, so `logging.googleapis.com/labels.execution_id` lets you
  correlate every log line with the specific invocation that produced it.

[`logging::init()`]: https://docs.rs/open-functions-sdk/latest/open_functions_sdk/logging/fn.init.html
[`logging::execution_id_middleware`]: https://docs.rs/open-functions-sdk/latest/open_functions_sdk/logging/fn.execution_id_middleware.html

## Error handling

`Functions::serve()` returns `Result<(), open_functions_sdk::Error>`; a `main`
function should propagate it with `?` (as in the examples above) so the
process exits non-zero on setup failure — matching the Functions
Framework's own startup-failure behavior, which both `open-functions` and Cloud Run
detect and surface. The error cases are:

- `MissingTarget`: `FUNCTION_TARGET` doesn't match any name registered via
  `.http()` or `.cloud_event()`.
- `SignatureMismatch`: `FUNCTION_SIGNATURE_TYPE` doesn't match how the
  resolved target was registered (e.g. `cloudevent` configured for an
  `.http()` target).
- `InvalidPort`: `PORT` is set but isn't a valid `u16`.
- `Bind`: the resolved address couldn't be bound (e.g. the port is already
  in use).
- `Serve`: the underlying `axum`/`hyper` server returned an I/O error while
  running.

`Functions::router()` resolves the same environment-based configuration and
returns just the `axum::Router` without binding a socket, which is useful
for exercising a function's routing/signature logic in tests via
`tower::ServiceExt::oneshot` without spinning up a real listener — see this
crate's own `tests/http_contract.rs` and `tests/cloudevent_contract.rs` for
worked examples.
