//! Instance execution: starting function instances (as processes or containers)
//! and driving them through readiness, graceful stop, and crash detection.
//!
//! [`Driver`] is the abstraction `InstancePool` (T038) builds on. [`process`]
//! implements it for `tokio::process`-spawned instances (source/host-cargo
//! path); [`container`] will implement it for `bollard`-managed containers
//! (US4).

pub mod cgroup;
pub mod container;
pub mod docker;
pub mod process;
pub mod readiness;

/// Everything a [`Driver`] needs to start one function instance.
#[derive(Debug, Clone)]
pub struct InstanceSpec {
    pub function_name: String,
    pub revision: u32,
    pub entry_point: String,
    /// `http` | `cloudevent`, set by the caller from `Function::trigger` (FF contract's
    /// `FUNCTION_SIGNATURE_TYPE`).
    pub signature_type: &'static str,
    pub env: std::collections::BTreeMap<String, String>,
    pub memory_mib: u32,
    pub start_timeout: std::time::Duration,
    /// Path to the built/copied executable (source/host-cargo path). Unused by
    /// container drivers.
    pub artifact_path: std::path::PathBuf,
    /// Image reference to run (image-mode / US4). `None` for the
    /// source/host-cargo path; `Some` for `ContainerDriver`, which ignores
    /// `artifact_path`.
    pub image_ref: Option<String>,
}

/// A running function instance, however it was started.
///
/// The instance's process/container lifetime is owned by a background task
/// spawned by the [`Driver`] that created this handle — not by this struct
/// itself. That task is the sole place that calls `wait()`/equivalent on the
/// underlying child, so exit detection (crash) works even if this handle is
/// never polled. [`stop`](InstanceHandle::stop) and
/// [`wait`](InstanceHandle::wait) both consume `self` and resolve once that
/// background task reports the instance has exited, for whatever reason:
///
/// - `stop(grace)` sends a graceful-stop request (SIGTERM for `ProcessDriver`)
///   to the background task and waits for it to confirm the instance exited,
///   escalating to a hard kill itself if `grace` elapses first. Always
///   resolves to `InstanceExit::Stopped`.
/// - `wait()` does not request a stop; it resolves whenever the instance exits
///   for any reason — including a driver-initiated stop, though callers that
///   want to detect *unexpected* exits (crashes) are the intended use — with
///   `InstanceExit::Crashed(_)` if the instance exited on its own, or
///   `InstanceExit::Stopped` if something else (e.g. a concurrent `stop()`
///   caller — not expected given `self` is consumed, but the background task
///   itself has no such restriction) stopped it first.
///
/// Dropping an `InstanceHandle` without calling either leaves the instance
/// running; the background task keeps it alive until it exits on its own.
pub struct InstanceHandle {
    pub addr: std::net::SocketAddr,
    stop_tx: Option<tokio::sync::oneshot::Sender<std::time::Duration>>,
    exit_rx: tokio::sync::oneshot::Receiver<InstanceExit>,
}

impl InstanceHandle {
    /// Requests a graceful stop (SIGTERM for processes) and waits up to `grace`
    /// before the driver escalates to a hard kill. Returns once fully stopped.
    pub async fn stop(mut self, grace: std::time::Duration) -> InstanceExit {
        if let Some(stop_tx) = self.stop_tx.take() {
            // If the receiving end is already gone the instance has already
            // exited (or is about to report as much) on its own; either way
            // `exit_rx` below resolves to the real outcome.
            let _ = stop_tx.send(grace);
        }
        self.exit_rx.await.unwrap_or(InstanceExit::Crashed(None))
    }

    /// Resolves when the instance exits for any reason (including a
    /// driver-initiated stop). Useful for detecting crashes.
    pub async fn wait(self) -> InstanceExit {
        self.exit_rx.await.unwrap_or(InstanceExit::Crashed(None))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceExit {
    /// Stopped via `InstanceHandle::stop()`.
    Stopped,
    /// The process/container exited on its own (crash), with an exit code if known.
    Crashed(Option<i32>),
}

/// Starts and readies function instances. Implemented by [`process::ProcessDriver`]
/// (source/host-cargo path) and [`container::ContainerDriver`] (image-mode
/// path, US4).
#[async_trait::async_trait]
pub trait Driver: Send + Sync {
    /// Starts one instance and returns once it is ready to accept connections
    /// (per `readiness::wait_ready`), or an error if it fails to become ready
    /// within `spec.start_timeout`.
    async fn spawn(&self, spec: &InstanceSpec) -> Result<InstanceHandle, DriverError>;

    /// Whether this driver's prerequisite runtime is actually reachable right
    /// now (e.g. the Docker daemon, for `ContainerDriver`). Used at
    /// image-mode registration time (US4) to reject with a clear
    /// `FAILED_PRECONDITION` instead of accepting a registration that can
    /// never successfully start an instance. `ProcessDriver` has no such
    /// prerequisite beyond the host itself already running this process, so
    /// the default is unconditionally available.
    async fn is_available(&self) -> bool {
        true
    }

    /// Label value for the `driver` label on `cf_rs_cold_start_seconds`
    /// (T082/US5): `"process"` for `process::ProcessDriver`, `"container"`
    /// for `container::ContainerDriver`.
    fn kind(&self) -> &'static str {
        "process"
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("failed to spawn instance: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("instance did not become ready within {0:?}")]
    ReadyTimeout(std::time::Duration),
    #[error("instance exited before becoming ready (code: {0:?})")]
    ExitedBeforeReady(Option<i32>),
}
