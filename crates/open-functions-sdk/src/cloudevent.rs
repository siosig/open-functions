//! CloudEvent function registration (Functions Framework `cloudevent` signature type).
//!
//! Per `function-contract.md` "CloudEvents functions": the SDK accepts both CloudEvents 1.0
//! binary content mode (`ce-*` headers) and structured mode
//! (`Content-Type: application/cloudevents+json`), via `cloudevents-sdk`'s `axum`
//! binding (`cloudevents::Event` implements `axum::extract::FromRequest`). A handler
//! returning `Ok(())` yields `200` (empty body); `Err` yields `500`.

use std::future::Future;

use axum::Router;
use axum::extract::FromRequest;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::any;

/// A decoded CloudEvent, as received from the Functions Framework host.
pub type CloudEvent = cloudevents::Event;

/// Errors decoding a [`CloudEvent`]'s `data` payload via [`CloudEventExt::data_as`].
#[derive(Debug, thiserror::Error)]
pub enum DataError {
    /// The event has no `data` field at all (e.g. a CloudEvent carrying
    /// only attributes, no payload).
    #[error("CloudEvent has no data payload")]
    MissingData,
    /// The event has a `data` payload, but it either isn't valid JSON or
    /// doesn't deserialize into the requested type `T`.
    #[error("failed to decode CloudEvent data as JSON: {0}")]
    Decode(#[source] serde_json::Error),
}

/// Extension trait for decoding a [`CloudEvent`]'s `data` payload into a caller-chosen
/// type, mirroring the Functions Framework's typed event-data helpers. Import this
/// trait (`use open_functions_sdk::cloudevent::CloudEventExt;` or `open_functions_sdk::CloudEventExt`) to
/// call `event.data_as::<T>()`.
pub trait CloudEventExt {
    /// Decode this event's `data` payload as JSON into `T`.
    fn data_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, DataError>;
}

impl CloudEventExt for CloudEvent {
    fn data_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, DataError> {
        let data = self.data().ok_or(DataError::MissingData)?.clone();
        let value: serde_json::Value = data.try_into().map_err(DataError::Decode)?;
        serde_json::from_value(value).map_err(DataError::Decode)
    }
}

/// A handler for a CloudEvents-triggered function: receives the decoded event, returns
/// `Ok(())` on success (SDK responds `200`) or `Err(E)` on failure (SDK responds `500`
/// with the error's `Display` as the body).
///
/// Implemented for any `Clone + Send + Sync + 'static` closure/fn with signature
/// `async fn(CloudEvent) -> Result<(), E>` where `E: std::error::Error + Send + Sync +
/// 'static` (e.g. [`DataError`] from [`CloudEventExt::data_as`], or any `anyhow`-style
/// boxable error).
pub trait CloudEventHandler: Clone + Send + Sync + 'static {
    /// Invoke the handler with the decoded `event`, boxing any returned
    /// error so this trait stays object-safe-friendly across the SDK's
    /// generic handler types. Callers normally don't call this directly —
    /// [`Functions::cloud_event`](crate::Functions::cloud_event) wires it
    /// into the served router.
    fn call(
        self,
        event: CloudEvent,
    ) -> impl Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send;
}

impl<F, Fut, E> CloudEventHandler for F
where
    F: FnOnce(CloudEvent) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<(), E>> + Send,
    E: std::error::Error + Send + Sync + 'static,
{
    async fn call(self, event: CloudEvent) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self(event)
            .await
            .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)
    }
}

pub(crate) fn build_router<H>(handler: H) -> Router
where
    H: CloudEventHandler,
{
    Router::new()
        .route("/robots.txt", any(not_found))
        .route("/favicon.ico", any(not_found))
        .fallback(move |req: axum::extract::Request| {
            let handler = handler.clone();
            async move {
                let event = match <CloudEvent as FromRequest<()>>::from_request(req, &()).await {
                    Ok(event) => event,
                    Err(rejection_response) => return rejection_response,
                };
                match handler.call(event).await {
                    Ok(()) => StatusCode::OK.into_response(),
                    Err(err) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
                    }
                }
            }
        })
        .layer(axum::middleware::from_fn(
            crate::logging::execution_id_middleware,
        ))
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}
