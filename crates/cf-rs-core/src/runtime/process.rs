//! `tokio::process`-based [`Driver`]: starts the built/copied function
//! artifact as a plain child process, per the Functions Framework Contract's
//! environment variable table (contracts/function-contract.md).

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

use super::cgroup::CgroupLimiter;
use super::{Driver, DriverError, InstanceExit, InstanceHandle, InstanceSpec, readiness};

pub struct ProcessDriver {
    pub limiter: Arc<CgroupLimiter>,
}

#[async_trait::async_trait]
impl Driver for ProcessDriver {
    async fn spawn(&self, spec: &InstanceSpec) -> Result<InstanceHandle, DriverError> {
        // Reserve a free port by binding `:0`, then release it immediately so
        // the child can bind it. Standard "reserve a port" trick; the small
        // TOCTOU race here is inherent and universally accepted for this
        // pattern.
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(DriverError::Spawn)?;
        let port = listener.local_addr().map_err(DriverError::Spawn)?.port();
        drop(listener);
        let addr = SocketAddr::from(([127, 0, 0, 1], port));

        let home = spec
            .artifact_path
            .parent()
            .unwrap_or_else(|| Path::new("/tmp"))
            .to_string_lossy()
            .to_string();

        let mut command = Command::new(&spec.artifact_path);
        command
            .env_clear()
            .envs(&spec.env)
            .env("PORT", port.to_string())
            .env("FUNCTION_TARGET", &spec.entry_point)
            .env("FUNCTION_SIGNATURE_TYPE", spec.signature_type)
            .env("K_SERVICE", &spec.function_name)
            .env(
                "K_REVISION",
                format!("{}-{:05}", spec.function_name, spec.revision),
            )
            .env("K_CONFIGURATION", &spec.function_name)
            .env("CF_RS_MEMORY_MIB", spec.memory_mib.to_string())
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", home)
            .env("LANG", "C.UTF-8")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(DriverError::Spawn)?;
        let pid = child.id();
        let instance_id = pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());

        if let Some(pid) = pid {
            // `apply` never fails to block startup (FR-014a): a failure just
            // disables the limiter and logs a one-time warning internally.
            let _ = self
                .limiter
                .apply(&spec.function_name, &instance_id, pid, spec.memory_mib);
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Race readiness against the child exiting early (e.g. a bad
        // FUNCTION_TARGET causing the SDK to exit(1) before binding PORT).
        let readiness_outcome = {
            let ready_fut = readiness::wait_ready(addr, spec.start_timeout);
            tokio::pin!(ready_fut);
            tokio::select! {
                ready = &mut ready_fut => ReadinessOutcome::Polled(ready),
                status = child.wait() => ReadinessOutcome::Exited(status),
            }
        };

        match readiness_outcome {
            ReadinessOutcome::Polled(Ok(())) => {}
            ReadinessOutcome::Polled(Err(timeout)) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                self.limiter.cleanup(&spec.function_name, &instance_id);
                return Err(DriverError::ReadyTimeout(timeout));
            }
            ReadinessOutcome::Exited(status) => {
                self.limiter.cleanup(&spec.function_name, &instance_id);
                let code = status.ok().and_then(|s| s.code());
                return Err(DriverError::ExitedBeforeReady(code));
            }
        }

        if let Some(stdout) = stdout {
            spawn_log_drain(
                stdout,
                "stdout",
                spec.function_name.clone(),
                instance_id.clone(),
            );
        }
        if let Some(stderr) = stderr {
            spawn_log_drain(
                stderr,
                "stderr",
                spec.function_name.clone(),
                instance_id.clone(),
            );
        }

        let (stop_tx, mut stop_rx) = oneshot::channel::<Duration>();
        let (exit_tx, exit_rx) = oneshot::channel::<InstanceExit>();

        let limiter = Arc::clone(&self.limiter);
        let function_name = spec.function_name.clone();
        let instance_id_bg = instance_id;
        let signal_pid = pid;

        tokio::spawn(async move {
            let exit = tokio::select! {
                status = child.wait() => {
                    InstanceExit::Crashed(status.ok().and_then(|s| s.code()))
                }
                Ok(grace) = &mut stop_rx => {
                    send_sigterm(signal_pid);
                    if tokio::time::timeout(grace, child.wait()).await.is_err() {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                    }
                    InstanceExit::Stopped
                }
            };

            limiter.cleanup(&function_name, &instance_id_bg);
            let _ = exit_tx.send(exit);
        });

        Ok(InstanceHandle {
            addr,
            stop_tx: Some(stop_tx),
            exit_rx,
        })
    }
}

enum ReadinessOutcome {
    /// `wait_ready` completed (ready, or timed out).
    Polled(Result<(), Duration>),
    /// The child exited before `wait_ready` resolved.
    Exited(std::io::Result<std::process::ExitStatus>),
}

/// Sends SIGTERM to `pid`, if known. Best-effort: a missing/already-exited
/// pid is not an error worth surfacing (the caller escalates to a hard kill
/// on its own timeout regardless).
fn send_sigterm(pid: Option<u32>) {
    let Some(pid) = pid else { return };

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGTERM: i32 = 15;

    // SAFETY: `kill(2)` with a plain integer pid and signal number has no
    // preconditions beyond being a valid syscall; at worst (pid already
    // exited) it returns -1/ESRCH, which we intentionally ignore.
    unsafe {
        kill(pid as i32, SIGTERM);
    }
}

/// Drains a piped child stdout/stderr line-by-line, so the child never blocks
/// on a full pipe buffer, and passes each line through to `tracing` at a
/// basic level. `T037`'s `LogPipe` will properly parse/structure this later;
/// this is just drain + passthrough.
fn spawn_log_drain<R>(reader: R, stream: &'static str, function_name: String, instance_id: String)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if stream == "stderr" {
                tracing::warn!(
                    target: "cf_rs::instance_stdout",
                    function = %function_name,
                    instance = %instance_id,
                    stream,
                    %line,
                );
            } else {
                tracing::info!(
                    target: "cf_rs::instance_stdout",
                    function = %function_name,
                    instance = %instance_id,
                    stream,
                    %line,
                );
            }
        }
    });
}
