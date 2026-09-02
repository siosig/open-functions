//! Prometheus metrics (T082/US5), per `ops-config.md`'s `[metrics]` table.
//! `metrics-exporter-prometheus`'s recorder (installed once at startup by
//! `cf-rs`'s `ops::init_metrics`) makes the `metrics::counter!`/`histogram!`/
//! `gauge!` macros used at each site below globally available -- there is no
//! central registry object to hold here, so this module exists to document,
//! in one place, every `cf_rs_*` metric this crate emits and exactly where,
//! rather than to define shared constants (the existing pre-US5 metrics
//! already established the pattern of inline string literals per call site,
//! e.g. `pubsub::reconcile`'s `cf_rs_pubsub_bindings`).
//!
//! | Metric | Type | Labels | Emitted in |
//! |---|---|---|---|
//! | `cf_rs_instances` | gauge | `function`, `state` | `pool::instance` (`finish_start`/`report_dead`/`reap_idle_once`/`stop_all`) |
//! | `cf_rs_instance_starts_total` | counter | `function`, `result` | `pool::instance::finish_start` |
//! | `cf_rs_instance_crashes_total` | counter | `function` | `pool::instance::report_dead` |
//! | `cf_rs_cold_start_seconds` | histogram | `function`, `driver` | `pool::instance::finish_start` |
//! | `cf_rs_queue_wait_seconds` | histogram | `function` | `pool::instance::acquire` |
//! | `cf_rs_builds_total` | counter | `function`, `mode`, `result` | `registry::service::register_source` |
//! | `cf_rs_build_duration_seconds` | histogram | `mode` | `registry::service::register_source` |
//! | `cf_rs_functions` | gauge | `state` | `registry::service::report_function_state_gauge` |
//! | `cf_rs_build_info` | gauge | `version`, `git_sha` | `cf-rs`'s `serve::run`, once at startup |
//! | `cf_rs_pubsub_bindings` | gauge | `state` | `pubsub::reconcile::Reconciler::report_binding_gauge` |
//!
//! (`cf_rs_invocations_total`, `cf_rs_invocation_duration_seconds`,
//! `cf_rs_forward_overhead_seconds`, and `cf_rs_pubsub_push_received_total`
//! predate US5 -- see `cf-rs`'s `server::invoke` and `forward` modules.)
