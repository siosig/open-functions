//! Cross-cutting operational bootstrap: tracing/logging, Prometheus metrics,
//! shutdown-signal coordination, and `sd_notify` systemd integration.
//!
//! See `specs/001-cloud-functions-local/contracts/ops-config.md`, sections
//! "Log format", "Metrics", "Signals and exit codes" and "systemd integration".
//!
//! This module only sets up infrastructure; it does not know about the
//! `invoke`/`admin` listeners, which are wired up elsewhere and call into
//! these functions once at startup / shutdown.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Builds an [`EnvFilter`] from `log.level`, honoring the "environment variable precedence"
/// rule that `RUST_LOG`, if set, takes priority over `log.level`.
fn build_env_filter(level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level))
}

/// Installs the global `tracing` subscriber according to `log.format`
/// (`"json" | "text" | "journald"`).
///
/// Safe to call more than once (e.g. across tests in the same process): uses
/// `try_init()` semantics throughout and ignores "a global subscriber is
/// already set" errors.
pub fn init_tracing(log: &crate::config::LogConfig) -> Result<(), anyhow::Error> {
    match log.format.as_str() {
        "text" => {
            let filter = build_env_filter(&log.level);
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
        "journald" => match tracing_journald::layer() {
            Ok(journald_layer) => {
                let filter = build_env_filter(&log.level);
                let _ = tracing_subscriber::registry()
                    .with(filter)
                    .with(journald_layer)
                    .try_init();
            }
            Err(err) => {
                eprintln!(
                    "warning: log.format = \"journald\" requested but tracing_journald::layer() \
                     failed ({err}); falling back to json logging"
                );
                let filter = build_env_filter(&log.level);
                let _ = tracing_subscriber::fmt()
                    .json()
                    .with_env_filter(filter)
                    .try_init();
            }
        },
        // "json" and any other value default to json (format is validated
        // elsewhere in crate::config::validate).
        _ => {
            let filter = build_env_filter(&log.level);
            let _ = tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .try_init();
        }
    }
    Ok(())
}

/// Installs the global `metrics` recorder backed by a Prometheus exporter and
/// returns the [`metrics_exporter_prometheus::PrometheusHandle`] used to
/// render `GET /metrics` responses.
pub fn init_metrics() -> Result<metrics_exporter_prometheus::PrometheusHandle, anyhow::Error> {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .map_err(anyhow::Error::from)
}

/// Coordinates graceful shutdown: a `tokio::sync::watch` channel flips to
/// `true` once SIGTERM or SIGINT is received.
pub struct Shutdown {
    tx: tokio::sync::watch::Sender<bool>,
    rx: tokio::sync::watch::Receiver<bool>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        Self { tx, rx }
    }

    /// A new receiver observing the shutdown flag. Cloned from the shared
    /// receiver; each caller can `.changed().await` or `.borrow()` it
    /// independently.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.rx.clone()
    }

    /// Resolves when SIGTERM or SIGINT is received (Unix), then flips the
    /// watch channel to `true`. Call once and spawn as a task; per
    /// ops-config.md, SIGHUP is ignored (dynamic config reload is out of
    /// scope).
    pub async fn wait_for_signal(&self) {
        use tokio::signal::unix::{SignalKind, signal};

        // `signal()` only fails if the underlying signal registration fails
        // (e.g. resource exhaustion); in that unlikely case we still want to
        // shut down cleanly rather than never observing a stop signal, so we
        // fall back to waiting forever on that particular stream instead of
        // panicking.
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("warning: failed to register SIGTERM handler: {err}");
                std::future::pending().await
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("warning: failed to register SIGINT handler: {err}");
                std::future::pending().await
            }
        };

        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }

        let _ = self.tx.send(true);
    }
}

/// Notifies the service manager that startup is complete
/// (`READY=1`, per the "systemd integration" contract: sent after both listeners
/// bind + redb open + existing function metadata restore complete).
///
/// A no-op (never errors, never panics) when not running under systemd
/// (`NOTIFY_SOCKET` unset) — `sd_notify::notify` already returns `Ok(())` in
/// that case, but the result is discarded here regardless so a failure to
/// notify systemd can never crash the host.
pub fn notify_ready() {
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);
}

/// Notifies the service manager that shutdown has begun (`STOPPING=1`), per
/// the "Signals and exit codes" contract. Same no-op-on-failure behavior as
/// [`notify_ready`].
pub fn notify_stopping() {
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Stopping]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LogConfig;

    fn log_config(format: &str) -> LogConfig {
        LogConfig {
            format: format.to_string(),
            level: "info".to_string(),
            function_ring_buffer_lines: 1000,
        }
    }

    #[test]
    fn init_tracing_json_does_not_panic() {
        assert!(init_tracing(&log_config("json")).is_ok());
    }

    #[test]
    fn init_tracing_text_does_not_panic() {
        assert!(init_tracing(&log_config("text")).is_ok());
    }

    #[test]
    fn init_tracing_journald_does_not_panic() {
        // Falls back to json on dev machines / CI without a journald socket.
        assert!(init_tracing(&log_config("journald")).is_ok());
    }

    #[test]
    fn init_tracing_called_repeatedly_does_not_panic() {
        assert!(init_tracing(&log_config("json")).is_ok());
        assert!(init_tracing(&log_config("text")).is_ok());
        assert!(init_tracing(&log_config("journald")).is_ok());
    }

    #[tokio::test]
    async fn shutdown_watch_channel_signals_subscribers() {
        let shutdown = Shutdown::new();
        let mut rx = shutdown.subscribe();
        assert!(!*rx.borrow());

        let waiter = tokio::spawn(async move {
            let changed = rx.changed().await;
            assert!(changed.is_ok(), "sender must not be dropped");
            *rx.borrow()
        });

        // Simulate what `wait_for_signal` does once a signal arrives, without
        // depending on actually sending OS signals to the test process.
        let sent = shutdown.tx.send(true);
        assert!(sent.is_ok(), "receiver must not be dropped");

        match waiter.await {
            Ok(flipped) => assert!(flipped),
            Err(_) => panic!("waiter task must not panic"),
        }
    }

    #[test]
    fn notify_ready_and_stopping_do_not_panic_without_systemd() {
        // In test environments NOTIFY_SOCKET is normally unset, so these
        // must be no-ops rather than errors/panics.
        notify_ready();
        notify_stopping();
    }
}
