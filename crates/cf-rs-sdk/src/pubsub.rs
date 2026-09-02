//! `MessagePublishedData` decoding for Pub/Sub-triggered CloudEvent functions.
//!
//! The wire JSON uses GCP's camelCase field names (`messageId`, `publishTime`,
//! `orderingKey`), per `contracts/function-contract.md`'s "Pub/Sub Push →
//! CloudEvent conversion" table and real Cloud Pub/Sub / Eventarc payloads — hence
//! `rename_all = "camelCase"` below, even though the Rust field names stay
//! snake_case per convention.

use serde::{Deserialize, Serialize};

/// A single Pub/Sub message, as embedded in a [`MessagePublishedData`]
/// CloudEvent payload.
///
/// Deserializes from GCP's camelCase wire JSON (`messageId`, `publishTime`,
/// `orderingKey`) into these snake_case fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PubsubMessage {
    /// The message payload, base64-decoded from the wire's `data` string.
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
    /// User-supplied key/value attributes published alongside the message.
    /// Empty (not absent) when the publisher sent none.
    #[serde(default)]
    pub attributes: std::collections::HashMap<String, String>,
    /// The unique ID Pub/Sub assigned this message.
    pub message_id: String,
    /// The RFC 3339 timestamp at which Pub/Sub received the message.
    pub publish_time: String,
    /// The ordering key, if the topic has message ordering enabled;
    /// empty string otherwise.
    #[serde(default)]
    pub ordering_key: String,
}

/// The `data` payload of a `google.cloud.pubsub.topic.v1.messagePublished`
/// [`CloudEvent`](crate::CloudEvent), as produced by ps-rs's Push→CloudEvent
/// conversion (and by real GCP Eventarc Pub/Sub triggers). Decode it from an
/// event with [`CloudEventExt::data_as`](crate::CloudEventExt::data_as).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePublishedData {
    /// The published message.
    pub message: PubsubMessage,
    /// The full subscription resource name the message was delivered
    /// through, e.g. `projects/local/subscriptions/cf-rs-on-msg`.
    pub subscription: String,
}

mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)
    }
}
