//! GCP-structured JSON logging layer for function processes.
//!
//! Per `function-contract.md` "Execution ID and logging", every `tracing` event emitted by a
//! function process must be written to stdout as a single JSON line shaped like:
//!
//! ```json
//! {"severity":"INFO","message":"hello","time":"2026-09-02T01:02:03.456789Z","logging.googleapis.com/labels":{"execution_id":"3f2b..."}}
//! ```
//!
//! `execution_id` comes from the `Function-Execution-Id` request header, carried via a
//! tracing span entered by [`execution_id_middleware`] for the lifetime of the request.

use tracing::{Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

/// Configure the GCP structured-JSON logging layer on stdout. Filtered by `RUST_LOG`
/// (defaults to `info`). Safe to call more than once per process; later calls are
/// no-ops (`try_init` failures are swallowed).
pub fn init() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(GcpLoggingLayer)
        .try_init();
}

/// Create the span that a request handler should enter for the duration of handling one
/// invocation, so nested log events pick up `execution_id`.
pub fn execution_span(execution_id: &str) -> tracing::Span {
    tracing::info_span!("function_invocation", execution_id = %execution_id)
}

/// Axum middleware that reads `Function-Execution-Id` from the request and instruments
/// the rest of the request handling with [`execution_span`], so every log line emitted
/// while handling the request carries the execution id.
pub async fn execution_id_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use tracing::Instrument;

    let execution_id = req
        .headers()
        .get("Function-Execution-Id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    match execution_id {
        Some(id) => next.run(req).instrument(execution_span(&id)).await,
        None => next.run(req).await,
    }
}

/// Span extension storing the `execution_id` field value recorded on
/// `function_invocation` spans.
struct ExecutionIdField(String);

/// A `tracing_subscriber::Layer` that writes one GCP-structured JSON object per event
/// directly to stdout, independent of the built-in `fmt` formatter (whose field names
/// don't match GCP's expected shape).
struct GcpLoggingLayer;

impl<S> Layer<S> for GcpLoggingLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
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
        let mut visitor = FieldVisitor::new("execution_id");
        attrs.record(&mut visitor);
        if let Some(execution_id) = visitor.into_value() {
            span.extensions_mut().insert(ExecutionIdField(execution_id));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let severity = match *event.metadata().level() {
            Level::ERROR => "ERROR",
            Level::WARN => "WARNING",
            Level::INFO => "INFO",
            Level::DEBUG | Level::TRACE => "DEBUG",
        };

        let mut visitor = FieldVisitor::new("message");
        event.record(&mut visitor);
        let message = visitor.into_value().unwrap_or_default();

        let time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);

        let mut line = serde_json::Map::new();
        line.insert(
            "severity".to_string(),
            serde_json::Value::String(severity.to_string()),
        );
        line.insert("message".to_string(), serde_json::Value::String(message));
        line.insert("time".to_string(), serde_json::Value::String(time));

        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(execution_id) = span.extensions().get::<ExecutionIdField>() {
                    line.insert(
                        "logging.googleapis.com/labels".to_string(),
                        serde_json::json!({ "execution_id": execution_id.0 }),
                    );
                }
            }
        }

        if let Ok(json) = serde_json::to_string(&line) {
            println!("{json}");
        }
    }
}

/// Captures the value of a single named field, formatted without the surrounding quotes
/// that `{:?}` would add for string values (`tracing`'s `message` field and `%`-recorded
/// fields are delivered via `record_debug`, whose `Debug` impl already forwards to
/// `Display` for those cases, but plain `&str` fields go through `record_str`).
struct FieldVisitor {
    name: &'static str,
    value: Option<String>,
}

impl FieldVisitor {
    fn new(name: &'static str) -> Self {
        Self { name, value: None }
    }

    fn into_value(self) -> Option<String> {
        self.value
    }
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == self.name {
            self.value = Some(format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == self.name {
            self.value = Some(value.to_string());
        }
    }
}
