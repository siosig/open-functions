//! Thin REST client for open-pubusb's Pub/Sub-compatible subscription-management API.
//!
//! Endpoints and shapes come from the open-pubusb contract doc
//! (`open-pubusb/specs/001-local-pubsub-service/contracts/pubsub-api.md`, "REST (HTTP/JSON) subset"):
//!
//! | HTTP   | Path                                             | gRPC equivalent    |
//! |--------|---------------------------------------------------|--------------------|
//! | PUT    | `/v1/projects/{p}/subscriptions/{s}`               | CreateSubscription |
//! | GET    | `/v1/projects/{p}/subscriptions/{s}`               | GetSubscription    |
//! | DELETE | `/v1/projects/{p}/subscriptions/{s}`               | DeleteSubscription |
//!
//! Notably, this client does not use a PATCH method or a
//! `:modifyPushConfig` path for subscriptions -- any path other than the
//! documented ones returns `501`. So it changes `pushConfig` by deleting
//! the subscription and re-creating it (see
//! [`OpenPubusbClient::recreate_subscription`]). open-pubusb has since
//! grown a `:modifyPushConfig` verb (its commit 7c29c98), so this could be
//! simplified to a single call once that version is the supported floor;
//! delete-then-create keeps working against both.
//!
//! That same open-pubusb commit is what makes `pushConfig` on
//! PUT-to-create take effect at all: before it, the REST create handler
//! decoded a hand-written struct that silently dropped `pushConfig`, so
//! every subscription this client created came back pull-only and no Push
//! delivery ever arrived (see quickstart.md's Pub/Sub execution records).
//!
//! This client is intentionally a thin, honest HTTP wrapper: no retries, no
//! backoff, no swallowing of error statuses. Reconciliation policy (what to
//! do on 409/404/unreachable) belongs to the caller.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};

/// Extracts the bare subscription id from either form a caller may hold: a
/// short id (`open-functions-on-orders`, what [`crate::pubsub::reconcile`]
/// derives when it first creates a subscription) or a full resource name
/// (`projects/local/subscriptions/open-functions-on-orders`, what open-pubusb
/// *returns* and what `TriggerBinding.subscription` therefore stores, per
/// admin-api.md's documented `binding.subscription` shape).
///
/// Without this, [`OpenPubusbClient::subscription_url`] would prepend the
/// collection path to an already-qualified name and produce
/// `.../subscriptions/projects/local/subscriptions/<id>`, which open-pubusb
/// answers with `501` -- silently stranding the real subscription on every
/// unbind (the delete "fails", the reconciler retries the same broken URL
/// forever, and the subscription outlives the function that owned it).
fn subscription_id(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Errors returned by [`OpenPubusbClient`].
#[derive(Debug, thiserror::Error)]
pub enum PubSubError {
    /// The HTTP request never completed a round-trip with open-pubusb: DNS
    /// failure, connection refused, or a timeout. Callers should treat this
    /// as transient and retry with backoff.
    #[error("open-pubusb unreachable at {url}: {source}")]
    Unreachable {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    /// open-pubusb answered with a non-2xx status (or a 2xx body that could not
    /// be parsed as the expected JSON shape). Callers should treat this as
    /// a permanent error for the given request, not retry blindly.
    #[error("open-pubusb returned {status}: {body}")]
    Http { status: u16, body: String },
}

/// `google.pubsub.v1.PushConfig` (partial): the fields open-functions sets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushConfig {
    #[serde(rename = "pushEndpoint")]
    pub push_endpoint: String,
}

/// Body of `PUT /v1/projects/{p}/subscriptions/{s}` (CreateSubscription).
/// `name` is not included: it is implied by the URL, matching open-pubusb's
/// proto3 JSON mapping for the PUT-to-create convention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionRequest {
    /// `projects/{p}/topics/{t}`.
    pub topic: String,
    #[serde(rename = "pushConfig")]
    pub push_config: PushConfig,
    #[serde(rename = "ackDeadlineSeconds")]
    pub ack_deadline_seconds: u32,
}

/// `google.pubsub.v1.Subscription` (partial): only the fields open-functions reads.
/// Unknown fields in open-pubusb's response are ignored by serde's default
/// behavior, so this stays forward-compatible with the full proto shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    /// `projects/{p}/subscriptions/{s}`.
    pub name: String,
    /// `projects/{p}/topics/{t}`.
    pub topic: String,
    #[serde(rename = "pushConfig")]
    pub push_config: Option<PushConfig>,
    #[serde(rename = "ackDeadlineSeconds", default)]
    pub ack_deadline_seconds: u32,
}

/// REST client for open-pubusb's subscription-management surface.
pub struct OpenPubusbClient {
    base_url: String,
    project: String,
    http: reqwest::Client,
}

impl OpenPubusbClient {
    pub fn new(base_url: String, project: String, request_timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(request_timeout)
            .build()
            // `Client::builder().build()` only fails on TLS-backend
            // initialization errors, which cannot happen for this client
            // (no TLS backend is configured beyond the workspace-default
            // rustls feature). Fall back to an unconfigured client rather
            // than panicking; per-request behavior degrades to "no
            // explicit timeout" instead of hard-failing construction.
            .unwrap_or_default();
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            project,
            http,
        }
    }

    fn subscription_url(&self, name: &str) -> String {
        format!(
            "{}/v1/projects/{}/subscriptions/{}",
            self.base_url,
            self.project,
            subscription_id(name)
        )
    }

    /// Sends a request and returns the raw status + body text, mapping any
    /// failure to complete the HTTP round-trip (connect/timeout/etc.) to
    /// `PubSubError::Unreachable`. A response that *was* received -- even a
    /// non-2xx one -- is returned as `Ok`, so callers can inspect status.
    async fn execute(
        &self,
        method: Method,
        url: &str,
        body: Option<&SubscriptionRequest>,
    ) -> Result<(StatusCode, String), PubSubError> {
        let mut builder = self.http.request(method, url);
        if let Some(body) = body {
            builder = builder.json(body);
        }
        let response = builder
            .send()
            .await
            .map_err(|source| PubSubError::Unreachable {
                url: url.to_string(),
                source,
            })?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|source| PubSubError::Unreachable {
                url: url.to_string(),
                source,
            })?;
        Ok((status, text))
    }

    fn parse_subscription(status: StatusCode, body: String) -> Result<Subscription, PubSubError> {
        serde_json::from_str(&body).map_err(|err| PubSubError::Http {
            status: status.as_u16(),
            body: format!("invalid Subscription JSON in {status} response: {err}; body={body}"),
        })
    }

    /// `PUT /v1/projects/{p}/subscriptions/{s}` (CreateSubscription).
    ///
    /// A 409 (already exists) is returned as `PubSubError::Http{status:409,..}`
    /// like any other non-2xx status -- this client does not paper over it.
    /// Per plan.md's reconciliation design, the caller is expected to `get`
    /// the existing subscription afterward to decide what to do.
    pub async fn create_subscription(
        &self,
        name: &str,
        req: &SubscriptionRequest,
    ) -> Result<Subscription, PubSubError> {
        let url = self.subscription_url(name);
        let (status, body) = self.execute(Method::PUT, &url, Some(req)).await?;
        if status.is_success() {
            Self::parse_subscription(status, body)
        } else {
            Err(PubSubError::Http {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// `GET /v1/projects/{p}/subscriptions/{s}` (GetSubscription).
    ///
    /// Returns `Ok(None)` for a 404 specifically -- a legitimate "doesn't
    /// exist" outcome the reconciler needs to distinguish from a hard
    /// error. Any other non-2xx is `Err(PubSubError::Http{..})`.
    pub async fn get_subscription(&self, name: &str) -> Result<Option<Subscription>, PubSubError> {
        let url = self.subscription_url(name);
        let (status, body) = self.execute(Method::GET, &url, None).await?;
        if status == StatusCode::NOT_FOUND {
            Ok(None)
        } else if status.is_success() {
            Self::parse_subscription(status, body).map(Some)
        } else {
            Err(PubSubError::Http {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// `DELETE /v1/projects/{p}/subscriptions/{s}` (DeleteSubscription).
    ///
    /// A 404 is treated as success (already gone), per the contract's
    /// general convention and plan.md's "DELETE 2xx/404" note for this
    /// exact client.
    pub async fn delete_subscription(&self, name: &str) -> Result<(), PubSubError> {
        let url = self.subscription_url(name);
        let (status, body) = self.execute(Method::DELETE, &url, None).await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(PubSubError::Http {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// Changes an existing subscription's `pushConfig`.
    ///
    /// open-pubusb's REST subset exposes no PATCH method and no
    /// `:modifyPushConfig` path for subscriptions (see the module doc
    /// comment) -- only PUT/GET/DELETE plus the `:pull`/`:acknowledge`/
    /// `:modifyAckDeadline` action verbs, and any other path is `501`. So
    /// the only REST-level way to change `pushConfig` is to delete the
    /// subscription and re-create it with the desired `SubscriptionRequest`
    /// (which must carry the full desired state -- `topic` and
    /// `ackDeadlineSeconds` included -- since PUT-to-create needs them).
    ///
    /// This is a thin DELETE-then-PUT composition, not a retry loop: if
    /// either call fails, the error propagates immediately and the
    /// subscription may be left deleted-but-not-recreated. Callers (the
    /// reconciler) must treat a failure here as needing another
    /// reconciliation pass, the same as any other permanent/transient
    /// error from `create_subscription`/`delete_subscription` alone.
    pub async fn recreate_subscription(
        &self,
        name: &str,
        req: &SubscriptionRequest,
    ) -> Result<Subscription, PubSubError> {
        self.delete_subscription(name).await?;
        self.create_subscription(name, req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> OpenPubusbClient {
        OpenPubusbClient::new(
            "http://127.0.0.1:8085".to_string(),
            "local".to_string(),
            Duration::from_secs(5),
        )
    }

    #[test]
    fn subscription_url_accepts_a_bare_id() {
        assert_eq!(
            client().subscription_url("open-functions-on-orders"),
            "http://127.0.0.1:8085/v1/projects/local/subscriptions/open-functions-on-orders"
        );
    }

    /// `TriggerBinding.subscription` stores the *full* resource name
    /// open-pubusb returns from create, and `try_unbind` passes that value
    /// straight back in. Before `subscription_id` normalized it, this built
    /// `.../subscriptions/projects/local/subscriptions/...` -- a `501` from
    /// open-pubusb that left every unbound subscription stranded.
    #[test]
    fn subscription_url_accepts_a_full_resource_name() {
        assert_eq!(
            client().subscription_url("projects/local/subscriptions/open-functions-on-orders"),
            "http://127.0.0.1:8085/v1/projects/local/subscriptions/open-functions-on-orders"
        );
    }

    #[test]
    fn subscription_id_is_idempotent_and_never_empty_for_a_bare_id() {
        assert_eq!(subscription_id("a"), "a");
        assert_eq!(
            subscription_id(subscription_id("projects/p/subscriptions/a")),
            "a"
        );
    }
}
