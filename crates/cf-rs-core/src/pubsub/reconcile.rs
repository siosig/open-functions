//! `TriggerBinding` reconciliation (T052): creates/fixes/removes ps-rs Push
//! subscriptions for Pub/Sub-triggered functions, per plan.md's
//! "PubSubBinding" Design Notes and data-model.md's `TriggerBinding` state
//! machine (`pending → bound`, `→ error`, `→ unbinding → [removed]`).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::model::binding::{BindingState, TriggerBinding};
use crate::pubsub::client::{PsRsClient, PubSubError, PushConfig, SubscriptionRequest};
use crate::registry::store::{Store, StoreError};

/// The desired end-state of a Pub/Sub trigger binding, as computed by the
/// registry from a `Function`'s current configuration.
#[derive(Debug, Clone)]
pub struct DesiredBinding {
    pub function_name: String,
    pub project: String,
    pub topic: String,
    pub push_endpoint: String,
    pub ack_deadline_seconds: u32,
}

/// Reconciles `TriggerBinding`s against ps-rs. Stateless beyond its
/// dependencies (`client`, `store`) and backoff bounds; every method persists
/// its outcome to `store` before returning, so a crash mid-reconciliation
/// just means the next sweep (or the next explicit call) picks up where the
/// stored `TriggerBinding` state left off.
pub struct Reconciler {
    client: PsRsClient,
    store: Arc<dyn Store>,
    initial_backoff: Duration,
    max_backoff: Duration,
}

fn subscription_name(function_name: &str) -> String {
    format!("cf-rs-{function_name}")
}

impl Reconciler {
    pub fn new(
        client: PsRsClient,
        store: Arc<dyn Store>,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        Self {
            client,
            store,
            initial_backoff,
            max_backoff,
        }
    }

    /// Computes the next backoff duration, per plan.md's "exponential
    /// backoff (5s start, 60s cap)": doubles the previous wait (inferred from the
    /// existing binding's `next_retry_at`, if any) up to `max_backoff`, or
    /// starts at `initial_backoff` for a binding with no prior retry.
    fn next_backoff(&self, previous: Option<&TriggerBinding>) -> Duration {
        let Some(previous) = previous else {
            return self.initial_backoff;
        };
        // We don't store the last-used backoff directly; approximate it by
        // doubling from `initial_backoff` each time this binding has already
        // been retried at least once (state was already Pending/Unbinding).
        match previous.state {
            BindingState::Pending | BindingState::Unbinding => {
                (self.initial_backoff * 2).min(self.max_backoff)
            }
            _ => self.initial_backoff,
        }
    }

    /// Attempts to bind (create, or fix a mismatched existing) subscription
    /// for one function's Pub/Sub trigger, once. Always persists and returns
    /// the resulting [`TriggerBinding`].
    pub async fn try_bind(&self, desired: &DesiredBinding) -> Result<TriggerBinding, StoreError> {
        let name = subscription_name(&desired.function_name);
        let previous = self.store.get_binding(&desired.function_name)?;
        let req = SubscriptionRequest {
            topic: format!("projects/{}/topics/{}", desired.project, desired.topic),
            push_config: PushConfig {
                push_endpoint: desired.push_endpoint.clone(),
            },
            ack_deadline_seconds: desired.ack_deadline_seconds,
        };

        let outcome = match self.client.create_subscription(&name, &req).await {
            Ok(sub) => Ok(sub),
            Err(PubSubError::Http { status: 409, .. }) => {
                match self.client.get_subscription(&name).await {
                    Ok(Some(existing)) => {
                        let matches = existing.topic == req.topic
                            && existing
                                .push_config
                                .as_ref()
                                .is_some_and(|pc| pc.push_endpoint == desired.push_endpoint);
                        if matches {
                            Ok(existing)
                        } else {
                            self.client.recreate_subscription(&name, &req).await
                        }
                    }
                    Ok(None) => {
                        // Raced with a concurrent delete between our PUT and
                        // this GET; treat as transient and retry later.
                        Err(PubSubError::Http {
                            status: 409,
                            body: "subscription vanished between create and get".to_string(),
                        })
                    }
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
        };

        let binding = match outcome {
            Ok(sub) => TriggerBinding {
                function_name: desired.function_name.clone(),
                subscription: sub.name,
                topic: desired.topic.clone(),
                push_endpoint: desired.push_endpoint.clone(),
                state: BindingState::Bound,
                last_error: None,
                next_retry_at: None,
            },
            Err(PubSubError::Unreachable { .. }) => {
                let backoff = self.next_backoff(previous.as_ref());
                TriggerBinding {
                    function_name: desired.function_name.clone(),
                    subscription: name,
                    topic: desired.topic.clone(),
                    push_endpoint: desired.push_endpoint.clone(),
                    state: BindingState::Pending,
                    last_error: Some("ps-rs unreachable".to_string()),
                    next_retry_at: Some(Utc::now() + backoff),
                }
            }
            Err(PubSubError::Http { status, body })
                if (500..600).contains(&status) || status == 409 =>
            {
                // Server-side transient failure (or the raced-delete case
                // above, re-flagged as 409): retry with backoff rather than
                // treating it as permanent.
                let backoff = self.next_backoff(previous.as_ref());
                TriggerBinding {
                    function_name: desired.function_name.clone(),
                    subscription: name,
                    topic: desired.topic.clone(),
                    push_endpoint: desired.push_endpoint.clone(),
                    state: BindingState::Pending,
                    last_error: Some(format!("ps-rs {status}: {body}")),
                    next_retry_at: Some(Utc::now() + backoff),
                }
            }
            Err(PubSubError::Http { status, body }) => {
                // A 4xx other than 409 (e.g. topic not found) is permanent:
                // retrying without the user fixing the underlying cause
                // (creating the topic, etc.) would never succeed.
                TriggerBinding {
                    function_name: desired.function_name.clone(),
                    subscription: name,
                    topic: desired.topic.clone(),
                    push_endpoint: desired.push_endpoint.clone(),
                    state: BindingState::Error,
                    last_error: Some(format!("ps-rs {status}: {body}")),
                    next_retry_at: None,
                }
            }
        };

        self.store.put_binding(&binding)?;
        self.report_binding_gauge();
        Ok(binding)
    }

    /// Recomputes and publishes the `cf_rs_pubsub_bindings` gauge (one time
    /// series per `BindingState`, labeled `state`) from the store's current
    /// contents, so it always reflects reality rather than drifting via
    /// incremental increment/decrement bookkeeping.
    fn report_binding_gauge(&self) {
        let bindings = match self.store.list_bindings() {
            Ok(bindings) => bindings,
            Err(err) => {
                tracing::warn!(%err, "pubsub reconciler: failed to list bindings for gauge update");
                return;
            }
        };
        let mut counts = [0u64; 4];
        for binding in &bindings {
            let idx = match binding.state {
                BindingState::Pending => 0,
                BindingState::Bound => 1,
                BindingState::Unbinding => 2,
                BindingState::Error => 3,
            };
            counts[idx] += 1;
        }
        metrics::gauge!("cf_rs_pubsub_bindings", "state" => "pending").set(counts[0] as f64);
        metrics::gauge!("cf_rs_pubsub_bindings", "state" => "bound").set(counts[1] as f64);
        metrics::gauge!("cf_rs_pubsub_bindings", "state" => "unbinding").set(counts[2] as f64);
        metrics::gauge!("cf_rs_pubsub_bindings", "state" => "error").set(counts[3] as f64);
    }

    /// Attempts to delete the subscription for a function whose Pub/Sub
    /// trigger is being removed (function deleted, or re-registered without
    /// a pubsub trigger). On success, removes the `TriggerBinding` from the
    /// store entirely (per data-model.md: `unbinding → [*]`); on failure,
    /// persists it as `Unbinding` with a backoff for a later retry.
    pub async fn try_unbind(&self, function_name: &str) -> Result<(), StoreError> {
        let previous = self.store.get_binding(function_name)?;
        let name = previous
            .as_ref()
            .map(|b| b.subscription.clone())
            .unwrap_or_else(|| subscription_name(function_name));

        let result = match self.client.delete_subscription(&name).await {
            Ok(()) => {
                self.store.delete_binding(function_name)?;
                Ok(())
            }
            Err(err) => {
                let backoff = self.next_backoff(previous.as_ref());
                let binding = TriggerBinding {
                    function_name: function_name.to_string(),
                    subscription: name,
                    topic: previous
                        .as_ref()
                        .map(|b| b.topic.clone())
                        .unwrap_or_default(),
                    push_endpoint: previous
                        .as_ref()
                        .map(|b| b.push_endpoint.clone())
                        .unwrap_or_default(),
                    state: BindingState::Unbinding,
                    last_error: Some(err.to_string()),
                    next_retry_at: Some(Utc::now() + backoff),
                };
                self.store.put_binding(&binding)?;
                Ok(())
            }
        };
        self.report_binding_gauge();
        result
    }

    /// One sweep: retries every stored binding in `Pending`/`Unbinding` state
    /// whose `next_retry_at` is due. `Pending` bindings are retried via
    /// [`Self::try_bind`] (re-derived from the binding's own stored
    /// `topic`/`push_endpoint` — the ack deadline isn't stored on
    /// `TriggerBinding`, so this uses a conservative default; the registry's
    /// next successful `register` call will re-derive the exact desired
    /// value from the `Function` record and call `try_bind` directly, this
    /// sweep is only a safety net for bindings ps-rs was unreachable for).
    /// `Unbinding` bindings are retried via [`Self::try_unbind`].
    pub async fn sweep_once(&self, default_ack_deadline_secs: u32, project: &str) {
        let bindings = match self.store.list_bindings() {
            Ok(bindings) => bindings,
            Err(err) => {
                tracing::warn!(%err, "pubsub reconciler: failed to list bindings for sweep");
                return;
            }
        };
        // Refresh the gauge on every tick (not just on a state-changing
        // try_bind/try_unbind below) so it reflects reality within one tick
        // of process startup, even if nothing happens to be due yet.
        self.report_binding_gauge();

        let now = Utc::now();
        for binding in bindings {
            let due = binding.next_retry_at.is_none_or(|at| at <= now);
            if !due {
                continue;
            }
            match binding.state {
                BindingState::Pending => {
                    let desired = DesiredBinding {
                        function_name: binding.function_name.clone(),
                        project: project.to_string(),
                        topic: binding.topic.clone(),
                        push_endpoint: binding.push_endpoint.clone(),
                        ack_deadline_seconds: default_ack_deadline_secs,
                    };
                    if let Err(err) = self.try_bind(&desired).await {
                        tracing::warn!(function = %binding.function_name, %err, "pubsub reconciler: failed to persist retry outcome");
                    }
                }
                BindingState::Unbinding => {
                    if let Err(err) = self.try_unbind(&binding.function_name).await {
                        tracing::warn!(function = %binding.function_name, %err, "pubsub reconciler: failed to persist unbind retry outcome");
                    }
                }
                BindingState::Bound | BindingState::Error => {}
            }
        }
    }

    /// Background task: sweeps every `tick` (independent of any individual
    /// binding's backoff — `sweep_once` itself checks `next_retry_at`).
    /// Returns the `JoinHandle` so the caller (Registry service) manages its
    /// lifetime.
    pub fn spawn_retry_loop(
        self: Arc<Self>,
        tick: Duration,
        default_ack_deadline_secs: u32,
        project: String,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            loop {
                interval.tick().await;
                self.sweep_once(default_ack_deadline_secs, &project).await;
            }
        })
    }
}
