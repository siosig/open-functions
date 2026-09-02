//! `MessagePublishedData` decoding for Pub/Sub-triggered CloudEvent functions.
//!
//! The wire JSON uses GCP's camelCase field names (`messageId`, `publishTime`,
//! `orderingKey`), per `contracts/function-contract.md`'s "Pub/Sub Push →
//! CloudEvent conversion" table and real Cloud Pub/Sub / Eventarc payloads — hence
//! `rename_all = "camelCase"` below, even though the Rust field names stay
//! snake_case per convention.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PubsubMessage {
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
    #[serde(default)]
    pub attributes: std::collections::HashMap<String, String>,
    pub message_id: String,
    pub publish_time: String,
    #[serde(default)]
    pub ordering_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePublishedData {
    pub message: PubsubMessage,
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
