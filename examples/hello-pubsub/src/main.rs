//! Minimal Pub/Sub-triggered (CloudEvents) function using `open-functions-sdk`. Runs
//! unmodified on open-functions (registered with `--trigger-topic <topic>`) and on
//! Google Cloud Run functions via Eventarc's Pub/Sub trigger.
//!
//! Test-only behavior, controlled by env vars (mirrors examples/hello-http,
//! used by open-functions's own test suite):
//! - `FAIL=1`: returns `Err(...)` instead of processing the message (open-functions
//!   responds 500 to open-pubusb's Push delivery, which then retries per its own
//!   backoff policy).

use open_functions_sdk::cloudevent::DataError;
use open_functions_sdk::pubsub::MessagePublishedData;
use open_functions_sdk::{CloudEvent, CloudEventExt, Functions};
use cloudevents::AttributesReader;

#[tokio::main]
async fn main() -> Result<(), open_functions_sdk::Error> {
    Functions::new().cloud_event("on_msg", on_msg).serve().await
}

/// `CloudEventHandler` requires the returned error to itself implement
/// `std::error::Error` (so `Box<dyn Error + Send + Sync>` doesn't qualify —
/// it isn't `Sized`-bounded `Error` in stable std), hence this concrete enum.
#[derive(Debug, thiserror::Error)]
enum HandlerError {
    #[error(transparent)]
    Data(#[from] DataError),
    #[error("simulated failure")]
    Fail,
}

async fn on_msg(event: CloudEvent) -> Result<(), HandlerError> {
    let data: MessagePublishedData = event.data_as()?;

    if std::env::var("FAIL").is_ok() {
        tracing::warn!("simulating a failure on message {}", event.id());
        return Err(HandlerError::Fail);
    }

    let message_text = String::from_utf8_lossy(&data.message.data).to_string();
    // The SDK's structured-logging layer (per function-contract.md "Execution ID and logging")
    // only surfaces the `message` field text on stdout, not extra `tracing` fields, so
    // the interesting values are interpolated directly into the message here rather
    // than passed as separate fields (which `open-functions fn logs` would otherwise drop).
    tracing::info!(
        "received pubsub message: type={} source={} subscription={} message_id={} data={message_text} attributes={:?}",
        event.ty(),
        event.source(),
        data.subscription,
        data.message.message_id,
        data.message.attributes,
    );

    Ok(())
}
