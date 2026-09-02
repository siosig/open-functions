//! Prometheus metrics (T082/US5), per `ops-config.md`'s `[metrics]` table.
//! `metrics-exporter-prometheus`'s recorder (installed once at startup by
//! `open-functions`'s `ops::init_metrics`) makes the `metrics::counter!`/`histogram!`/
//! `gauge!` macros used at each site below globally available -- there is no
//! central registry object to hold here, so this module exists to document,
//! in one place, every `open_functions_*` metric this crate emits and exactly where,
//! rather than to define shared constants (the existing pre-US5 metrics
//! already established the pattern of inline string literals per call site,
//! e.g. `pubsub::reconcile`'s `open_functions_pubsub_bindings`).
//!
//! | Metric | Type | Labels | Emitted in |
//! |---|---|---|---|
//! | `open_functions_instances` | gauge | `function`, `state` | `pool::instance` (`finish_start`/`report_dead`/`reap_idle_once`/`stop_all`) |
//! | `open_functions_instance_starts_total` | counter | `function`, `result` | `pool::instance::finish_start` |
//! | `open_functions_instance_crashes_total` | counter | `function` | `pool::instance::report_dead` |
//! | `open_functions_cold_start_seconds` | histogram | `function`, `driver` | `pool::instance::finish_start` |
//! | `open_functions_queue_wait_seconds` | histogram | `function` | `pool::instance::acquire` |
//! | `open_functions_builds_total` | counter | `function`, `mode`, `result` | `registry::service::register_source` |
//! | `open_functions_build_duration_seconds` | histogram | `mode` | `registry::service::register_source` |
//! | `open_functions_functions` | gauge | `state` | `registry::service::report_function_state_gauge` |
//! | `open_functions_build_info` | gauge | `version`, `git_sha` | `open-functions`'s `serve::run`, once at startup |
//! | `open_functions_pubsub_bindings` | gauge | `state` | `pubsub::reconcile::Reconciler::report_binding_gauge` |
//!
//! (`open_functions_invocations_total`, `open_functions_invocation_duration_seconds`,
//! `open_functions_forward_overhead_seconds`, and `open_functions_pubsub_push_received_total`
//! predate US5 -- see `open-functions`'s `server::invoke` and `forward` modules.)
