"""Minimal Pub/Sub-triggered (CloudEvents) function using the official
`functions-framework` package.

Runs unmodified on open-functions (`open-functions fn deploy on-orders-py
--source examples/hello-python-pubsub --entry-point on_msg --trigger-topic
orders-py`) and on Google Cloud Run functions via Eventarc's Pub/Sub
trigger. Behavioral parity with the Rust reference implementation in
`examples/hello-pubsub/src/main.rs`.

Test-only behavior, controlled by env vars (mirrors examples/hello-pubsub,
used by open-functions's own test suite):
- `FAIL`: raises an exception instead of processing the message
  (functions-framework turns this into a 500, which open-pubusb's Push
  delivery then retries per its own backoff policy).
"""

import base64
import os
from typing import Any

import functions_framework
from cloudevents.http import CloudEvent


@functions_framework.cloud_event
def on_msg(cloud_event: CloudEvent) -> None:
    """Logs the contents of an incoming Pub/Sub `MessagePublishedData` event.

    Args:
        cloud_event: The CloudEvent delivered by functions-framework. Its
            `.data` is a dict shaped like GCP's Pub/Sub
            `MessagePublishedData`: `{"message": {"data": <base64>,
            "attributes": {...}, "messageId": ..., "publishTime": ...},
            "subscription": ...}`.

    Raises:
        RuntimeError: If the `FAIL` env var is set, to simulate a failure
            (functions-framework responds with a non-2xx status instead of
            processing the message).
    """
    if os.environ.get("FAIL"):
        raise RuntimeError("simulated failure")

    data: dict[str, Any] = cloud_event.data
    message: dict[str, Any] = data["message"]
    message_text = base64.b64decode(message["data"]).decode("utf-8", errors="replace")
    attributes: dict[str, Any] = message.get("attributes", {})

    print(
        f"received pubsub message: type={cloud_event['type']} "
        f"source={cloud_event['source']} subscription={data['subscription']} "
        f"message_id={message['messageId']} data={message_text} "
        f"attributes={attributes}"
    )
