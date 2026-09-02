//! Unit tests for Pub/Sub Push envelope parsing and CloudEvent construction
//! (T044), covering `super::convert`'s implementation of the rules in
//! `specs/001-cloud-functions-local/contracts/function-contract.md`'s
//! "Pub/Sub Push → CloudEvent conversion" section.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::convert::TryFrom;

use cloudevents::AttributesReader;
use serde_json::{Value, json};

use super::convert::{CloudEventParams, PushConvertError, parse_push_envelope, to_cloud_event};

fn event_data_json(event: &cloudevents::Event) -> Value {
    let data = event
        .data()
        .cloned()
        .expect("built event should always carry data");
    Value::try_from(data).expect("event data should always be JSON")
}

#[test]
fn normal_case_all_fields_present() {
    let body = json!({
        "message": {
            "data": "aGVsbG8=",
            "attributes": {"k": "v"},
            "messageId": "123",
            "publishTime": "2026-09-02T01:02:03.456Z",
            "orderingKey": "order-1"
        },
        "subscription": "projects/p/subscriptions/s",
        "deliveryAttempt": 3
    })
    .to_string();

    let envelope = parse_push_envelope(body.as_bytes()).expect("should parse");
    assert_eq!(envelope.message_id, "123");
    assert_eq!(envelope.publish_time, "2026-09-02T01:02:03.456Z");
    assert_eq!(envelope.data, b"hello");
    assert_eq!(envelope.attributes.get("k").map(String::as_str), Some("v"));
    assert_eq!(envelope.ordering_key.as_deref(), Some("order-1"));
    assert_eq!(envelope.subscription, "projects/p/subscriptions/s");
    assert_eq!(envelope.delivery_attempt, Some(3));

    let params = CloudEventParams {
        project: "local",
        topic: "orders",
    };
    let event = to_cloud_event(&envelope, &params);

    assert_eq!(event.id(), "123");
    assert_eq!(
        event.source().as_str(),
        "//pubsub.googleapis.com/projects/local/topics/orders"
    );
    assert_eq!(event.ty(), "google.cloud.pubsub.topic.v1.messagePublished");
    assert_eq!(event.datacontenttype(), Some("application/json"));
    assert_eq!(
        event
            .time()
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        Some("2026-09-02T01:02:03.456Z".to_string())
    );

    let data = event_data_json(&event);
    assert_eq!(data["message"]["data"], "aGVsbG8=");
    assert_eq!(data["message"]["attributes"]["k"], "v");
    assert_eq!(data["message"]["messageId"], "123");
    assert_eq!(data["message"]["publishTime"], "2026-09-02T01:02:03.456Z");
    assert_eq!(data["message"]["orderingKey"], "order-1");
    assert_eq!(data["subscription"], "projects/p/subscriptions/s");
    assert_eq!(data["deliveryAttempt"], 3);
}

#[test]
fn duplicate_camel_and_snake_case_keys_are_deduplicated() {
    let body = json!({
        "message": {
            "data": "aGVsbG8=",
            "messageId": "123",
            "message_id": "123",
            "publishTime": "2026-09-02T01:02:03.456Z",
            "publish_time": "2026-09-02T01:02:03.456Z",
            "orderingKey": ""
        },
        "subscription": "projects/p/subscriptions/s"
    })
    .to_string();

    let envelope = parse_push_envelope(body.as_bytes()).expect("should parse");
    assert_eq!(envelope.message_id, "123");
    assert_eq!(envelope.publish_time, "2026-09-02T01:02:03.456Z");

    let params = CloudEventParams {
        project: "local",
        topic: "orders",
    };
    let event = to_cloud_event(&envelope, &params);
    let data = event_data_json(&event);

    let message = data["message"]
        .as_object()
        .expect("message should be an object");
    assert!(message.contains_key("messageId"));
    assert!(!message.contains_key("message_id"));
    assert!(!message.contains_key("publish_time"));
}

#[test]
fn publish_time_falls_back_to_snake_case_field() {
    let body = json!({
        "message": {
            "data": "aGVsbG8=",
            "messageId": "123",
            "publish_time": "2026-09-02T01:02:03.456Z",
        },
        "subscription": "projects/p/subscriptions/s"
    })
    .to_string();

    let envelope = parse_push_envelope(body.as_bytes()).expect("should parse");
    assert_eq!(envelope.publish_time, "2026-09-02T01:02:03.456Z");
}

#[test]
fn publish_time_falls_back_to_receipt_time_when_both_absent() {
    let body = json!({
        "message": {
            "data": "aGVsbG8=",
            "messageId": "123"
        },
        "subscription": "projects/p/subscriptions/s"
    })
    .to_string();

    let envelope = parse_push_envelope(body.as_bytes()).expect("should parse");
    // Just assert it looks like a valid RFC3339 timestamp; exact value
    // depends on when the test ran.
    assert!(
        chrono::DateTime::parse_from_rfc3339(&envelope.publish_time).is_ok(),
        "expected RFC3339 timestamp, got {:?}",
        envelope.publish_time
    );

    let params = CloudEventParams {
        project: "local",
        topic: "orders",
    };
    let event = to_cloud_event(&envelope, &params);
    assert!(event.time().is_some());
}

#[test]
fn missing_message_field_is_rejected() {
    let body = json!({"subscription": "projects/p/subscriptions/s"}).to_string();
    let result = parse_push_envelope(body.as_bytes());
    assert!(matches!(result, Err(PushConvertError::MissingMessage)));
}

#[test]
fn invalid_base64_data_is_rejected() {
    let body = json!({
        "message": {
            "data": "not base64!!!",
            "messageId": "123"
        },
        "subscription": "projects/p/subscriptions/s"
    })
    .to_string();
    let result = parse_push_envelope(body.as_bytes());
    assert!(matches!(result, Err(PushConvertError::InvalidBase64)));
}

#[test]
fn non_json_body_is_rejected() {
    let body = b"not json at all";
    let result = parse_push_envelope(body);
    assert!(matches!(result, Err(PushConvertError::InvalidJson)));
}

#[test]
fn non_object_top_level_json_is_rejected() {
    let body = json!([1, 2, 3]).to_string();
    let result = parse_push_envelope(body.as_bytes());
    assert!(matches!(result, Err(PushConvertError::NotAnObject)));
}

#[test]
fn delivery_attempt_present_appears_in_data() {
    let body = json!({
        "message": {
            "data": "aGVsbG8=",
            "messageId": "123"
        },
        "subscription": "projects/p/subscriptions/s",
        "deliveryAttempt": 5
    })
    .to_string();

    let envelope = parse_push_envelope(body.as_bytes()).expect("should parse");
    let params = CloudEventParams {
        project: "local",
        topic: "orders",
    };
    let event = to_cloud_event(&envelope, &params);
    let data = event_data_json(&event);
    assert_eq!(data["deliveryAttempt"], 5);
}

#[test]
fn delivery_attempt_absent_does_not_appear_as_key() {
    let body = json!({
        "message": {
            "data": "aGVsbG8=",
            "messageId": "123"
        },
        "subscription": "projects/p/subscriptions/s"
    })
    .to_string();

    let envelope = parse_push_envelope(body.as_bytes()).expect("should parse");
    assert_eq!(envelope.delivery_attempt, None);

    let params = CloudEventParams {
        project: "local",
        topic: "orders",
    };
    let event = to_cloud_event(&envelope, &params);
    let data = event_data_json(&event);
    let obj = data.as_object().expect("data should be an object");
    assert!(
        !obj.contains_key("deliveryAttempt"),
        "deliveryAttempt should be absent, not null, when not present in the envelope"
    );
}
