//! Pub/Sub Push envelope parsing and CloudEvent construction.
//!
//! Converts the JSON body open-pubusb sends to `/_cf/push/{name}` (Pub/Sub standard
//! Push format) into a [`cloudevents::Event`] the host POSTs to the function
//! instance in binary content mode, per
//! `specs/001-cloud-functions-local/contracts/function-contract.md`'s
//! "Pub/Sub Push → CloudEvent conversion" section.

use std::collections::BTreeMap;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use cloudevents::{Event, EventBuilder, EventBuilderV10};
use serde_json::Value;

/// Errors returned while parsing a Pub/Sub Push request body.
///
/// Per function-contract.md: "body is not a JSON object, `message` is
/// missing, or `message.data` is not base64 → 400 (not passed to the
/// function)". A `subscription`
/// name mismatch is *not* a parse error (the contract only calls for a
/// `warn!` there) so it is not represented here — callers compare
/// [`PushEnvelope::subscription`] themselves.
#[derive(Debug, thiserror::Error)]
pub enum PushConvertError {
    #[error("request body is not valid JSON")]
    InvalidJson,
    #[error("request body is not a JSON object")]
    NotAnObject,
    #[error("missing required field \"message\"")]
    MissingMessage,
    #[error("message.data is not valid base64")]
    InvalidBase64,
}

/// The parsed, decoded components of a Pub/Sub Push request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushEnvelope {
    /// From `message.messageId`, falling back to `message.message_id`, or
    /// an empty string if both are absent. Not in the contract's rejection
    /// list, and Pub/Sub always sends one in practice, so this is a
    /// judgment call rather than an error.
    pub message_id: String,
    /// From `message.publishTime`, falling back to `message.publish_time`,
    /// falling back to the receipt time (`Utc::now().to_rfc3339()`) if both
    /// are absent.
    pub publish_time: String,
    /// `message.data`, base64-decoded.
    pub data: Vec<u8>,
    /// `message.attributes`, defaulting to an empty map if absent.
    pub attributes: BTreeMap<String, String>,
    /// `message.orderingKey`, or `None` if absent. The contract requires
    /// the key to always appear (possibly empty) in the *output* CloudEvent
    /// data; that defaulting happens in [`to_cloud_event`].
    pub ordering_key: Option<String>,
    /// The top-level `subscription` field, defaulting to an empty string if
    /// absent (not in the contract's rejection list).
    pub subscription: String,
    /// The top-level `deliveryAttempt` field, if present.
    pub delivery_attempt: Option<u32>,
}

/// Parses a raw Pub/Sub Push request body into its component parts, applying
/// function-contract.md's validation rules.
pub fn parse_push_envelope(raw: &[u8]) -> Result<PushEnvelope, PushConvertError> {
    let root: Value = serde_json::from_slice(raw).map_err(|_| PushConvertError::InvalidJson)?;
    let root = root.as_object().ok_or(PushConvertError::NotAnObject)?;

    let message = root
        .get("message")
        .and_then(Value::as_object)
        .ok_or(PushConvertError::MissingMessage)?;

    let data = match message.get("data") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(|_| PushConvertError::InvalidBase64)?,
        Some(_) => return Err(PushConvertError::InvalidBase64),
    };

    let attributes = message
        .get("attributes")
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), json_value_to_attr_string(v)))
                .collect()
        })
        .unwrap_or_default();

    let message_id = string_field(message, "messageId")
        .or_else(|| string_field(message, "message_id"))
        .unwrap_or_default();

    let publish_time = string_field(message, "publishTime")
        .or_else(|| string_field(message, "publish_time"))
        .unwrap_or_else(|| Utc::now().to_rfc3339());

    let ordering_key = string_field(message, "orderingKey");

    let subscription = string_field(root, "subscription").unwrap_or_default();

    let delivery_attempt = root
        .get("deliveryAttempt")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());

    Ok(PushEnvelope {
        message_id,
        publish_time,
        data,
        attributes,
        ordering_key,
        subscription,
        delivery_attempt,
    })
}

fn string_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn json_value_to_attr_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Parameters needed to build the CloudEvent's `source`, beyond what's in
/// the [`PushEnvelope`] itself.
pub struct CloudEventParams<'a> {
    /// `pubsub.project` configuration value.
    pub project: &'a str,
    /// `Function.trigger.topic`.
    pub topic: &'a str,
}

/// Builds the CloudEvent to POST to the function instance, per
/// function-contract.md's conversion table. The caller (a concurrent task
/// building the actual HTTP POST) handles binary-content-mode
/// serialization; this function just builds the `cloudevents::Event` value.
pub fn to_cloud_event(envelope: &PushEnvelope, params: &CloudEventParams<'_>) -> Event {
    let source = format!(
        "//pubsub.googleapis.com/projects/{}/topics/{}",
        params.project, params.topic
    );

    // `message.publishTime`/`publish_time`, parsed as RFC3339. If it fails
    // to parse (should not happen for a well-formed Pub/Sub payload, but
    // `PushEnvelope::publish_time` may already be a synthesized fallback
    // string), fall back to the receipt time.
    let time: DateTime<Utc> = DateTime::parse_from_rfc3339(&envelope.publish_time)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());

    let ordering_key = envelope.ordering_key.clone().unwrap_or_default();

    let mut message = serde_json::Map::new();
    message.insert(
        "data".to_string(),
        Value::String(base64::engine::general_purpose::STANDARD.encode(&envelope.data)),
    );
    message.insert(
        "attributes".to_string(),
        serde_json::to_value(&envelope.attributes)
            .unwrap_or_else(|_| Value::Object(Default::default())),
    );
    message.insert(
        "messageId".to_string(),
        Value::String(envelope.message_id.clone()),
    );
    message.insert(
        "publishTime".to_string(),
        Value::String(envelope.publish_time.clone()),
    );
    message.insert("orderingKey".to_string(), Value::String(ordering_key));

    let mut data = serde_json::Map::new();
    data.insert("message".to_string(), Value::Object(message));
    data.insert(
        "subscription".to_string(),
        Value::String(envelope.subscription.clone()),
    );
    // "deliveryAttempt is present only when a dead_letter_policy exists":
    // include the key in the built event's data only when present in the
    // envelope, never as an explicit `null`.
    if let Some(attempt) = envelope.delivery_attempt {
        data.insert("deliveryAttempt".to_string(), Value::Number(attempt.into()));
    }

    let builder = EventBuilderV10::new()
        .id(envelope.message_id.clone())
        .source(source)
        .ty("google.cloud.pubsub.topic.v1.messagePublished")
        .time(time)
        .data("application/json", Value::Object(data));

    // `build()` can only fail on a missing id/source/ty (all set above) or
    // an unparseable `time` (infallible here since we pass an already
    // -parsed `DateTime<Utc>`, not a raw string). This is therefore
    // unreachable in practice; `unwrap_or_default()` is a safe fallback
    // rather than a panic.
    builder.build().unwrap_or_default()
}
