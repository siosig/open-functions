//! `bollard`-based [`Driver`] for image-mode function instances (US4):
//! creates, starts, and inspects a Docker container running the function's
//! `image_ref`, per plan.md's "Runtime drivers" Design Notes and
//! research.md's R7 image-mode restatement.
//!
//! Image-mode instances always listen on the fixed container-internal port
//! [`CONTAINER_PORT`] (8080), unlike [`super::process::ProcessDriver`]'s
//! dynamically-reserved localhost port: each container is only reachable by
//! its own IP on the isolated [`super::docker::NETWORK_NAME`] Docker
//! network, so there is no local port-collision risk to avoid.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, ContainerWaitResponse, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, InspectContainerOptionsBuilder,
    ListContainersOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    StartContainerOptions, StopContainerOptionsBuilder, WaitContainerOptionsBuilder,
};
use futures_util::StreamExt;
use tokio::sync::oneshot;

use crate::logs::pipe::{self, Stream as LogStream};
use crate::logs::ring::{LogRingBuffer, LogStore};

use super::docker::{LABEL_FUNCTION, NETWORK_NAME, ensure_network};
use super::{Driver, DriverError, InstanceExit, InstanceHandle, InstanceSpec, readiness};

/// Fixed internal port every image-mode function instance listens on, per
/// contracts/function-contract.md's startup table ("container: `8080`").
const CONTAINER_PORT: u16 = 8080;

/// `stop(t=...)` grace period passed to the Docker daemon itself (it sends
/// SIGTERM, waits this many seconds, then SIGKILLs), per plan.md's
/// "stop is `stop(t=5)` -> `remove`". [`InstanceHandle::stop`]'s own caller-supplied
/// grace is honored too -- see `spawn`'s background task below.
const DEFAULT_STOP_GRACE_SECS: i32 = 5;

/// Runs image-mode function instances as Docker containers via `bollard`.
///
/// Callers (registration/pool wiring, T075) are expected to have already
/// probed daemon availability (`docker::is_available`) and, at process
/// startup, called [`sweep_stale_containers`] once to clean up any
/// label-tagged containers left over from an unclean prior shutdown -- see
/// plan.md's "sweep label-tagged leftover containers at startup". `spawn` itself also calls
/// [`ensure_network`] defensively before every spawn: it is a cheap,
/// idempotent no-op once the network already exists, so there is no harm in
/// not relying on a one-time startup call having happened first.
pub struct ContainerDriver {
    pub docker: Docker,
    /// Per-function log ring buffers (T079/US5): every container's stdout/
    /// stderr is drained into it, mirroring `ProcessDriver`'s
    /// `spawn_log_drain` -- see `spawn_container_log_drain` below.
    pub log_store: Arc<LogStore>,
}

impl ContainerDriver {
    pub fn new(docker: Docker) -> Self {
        Self {
            docker,
            log_store: Arc::new(LogStore::default()),
        }
    }
}

#[async_trait::async_trait]
impl Driver for ContainerDriver {
    async fn is_available(&self) -> bool {
        super::docker::is_available(&self.docker).await
    }

    fn kind(&self) -> &'static str {
        "container"
    }

    async fn spawn(&self, spec: &InstanceSpec) -> Result<InstanceHandle, DriverError> {
        let image_ref = spec.image_ref.as_deref().ok_or_else(|| {
            spawn_err("ContainerDriver::spawn requires InstanceSpec.image_ref to be Some(_)")
        })?;

        ensure_network(&self.docker).await.map_err(spawn_err)?;
        ensure_image(&self.docker, image_ref).await?;

        let container_name = format!(
            "open-functions-{}-{}-{}",
            spec.function_name,
            spec.revision,
            uuid::Uuid::new_v4().simple()
        );

        let env = container_env(spec);
        let memory_bytes = i64::from(spec.memory_mib) * 1024 * 1024;
        let body = ContainerCreateBody {
            image: Some(image_ref.to_string()),
            env: Some(env),
            host_config: Some(HostConfig {
                memory: Some(memory_bytes),
                network_mode: Some(NETWORK_NAME.to_string()),
                ..Default::default()
            }),
            labels: Some(HashMap::from([(
                LABEL_FUNCTION.to_string(),
                spec.function_name.clone(),
            )])),
            ..Default::default()
        };

        let create_options = CreateContainerOptionsBuilder::default()
            .name(&container_name)
            .build();
        let created = self
            .docker
            .create_container(Some(create_options), body)
            .await
            .map_err(spawn_err)?;
        let container_id = created.id;

        self.docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await
            .map_err(|err| {
                // Best-effort cleanup: a container that failed to start still
                // occupies its name/id until removed.
                spawn_err(err)
            })?;

        let addr = match inspect_addr(&self.docker, &container_id).await {
            Ok(addr) => addr,
            Err(err) => {
                let _ = force_remove(&self.docker, &container_id).await;
                return Err(err);
            }
        };

        // Race readiness against the container exiting early (e.g. a bad
        // FUNCTION_TARGET causing the Functions Framework SDK to exit(1)
        // before binding PORT), mirroring ProcessDriver::spawn's shape.
        let readiness_outcome = {
            let ready_fut = readiness::wait_ready(addr, spec.start_timeout);
            tokio::pin!(ready_fut);
            let mut wait_stream = self.docker.wait_container(
                &container_id,
                Some(WaitContainerOptionsBuilder::default().build()),
            );
            tokio::select! {
                ready = &mut ready_fut => ReadinessOutcome::Polled(ready),
                wait_result = wait_stream.next() => ReadinessOutcome::Exited(wait_result),
            }
        };

        match readiness_outcome {
            ReadinessOutcome::Polled(Ok(())) => {}
            ReadinessOutcome::Polled(Err(timeout)) => {
                stop_then_remove(&self.docker, &container_id, DEFAULT_STOP_GRACE_SECS).await;
                return Err(DriverError::ReadyTimeout(timeout));
            }
            ReadinessOutcome::Exited(wait_result) => {
                let code = exit_code_from_wait(wait_result);
                let _ = force_remove(&self.docker, &container_id).await;
                return Err(DriverError::ExitedBeforeReady(code));
            }
        }

        spawn_container_log_drain(
            self.docker.clone(),
            container_id.clone(),
            spec.function_name.clone(),
            spec.revision,
            Arc::clone(&self.log_store),
        );

        let (stop_tx, mut stop_rx) = oneshot::channel::<Duration>();
        let (exit_tx, exit_rx) = oneshot::channel::<InstanceExit>();

        let docker_bg = self.docker.clone();
        let container_id_bg = container_id.clone();

        tokio::spawn(async move {
            let mut wait_stream = docker_bg.wait_container(
                &container_id_bg,
                Some(WaitContainerOptionsBuilder::default().build()),
            );

            let exit = tokio::select! {
                wait_result = wait_stream.next() => {
                    InstanceExit::Crashed(exit_code_from_wait(wait_result))
                }
                Ok(grace) = &mut stop_rx => {
                    drop(wait_stream);
                    let t = i32::try_from(grace.as_secs()).unwrap_or(DEFAULT_STOP_GRACE_SECS);
                    let stop_opts = StopContainerOptionsBuilder::default().t(t).build();
                    // `stop_container` itself asks the daemon to SIGTERM, wait
                    // up to `t` seconds, then SIGKILL -- the outer timeout
                    // here is a defensive escalation in case the daemon call
                    // itself hangs (e.g. a stalled socket), not the primary
                    // stop mechanism. The unconditional `force_remove` below
                    // (SIGKILL + remove) covers both the normal and the
                    // escalated case.
                    let outer_timeout = grace + Duration::from_secs(2);
                    if tokio::time::timeout(
                        outer_timeout,
                        docker_bg.stop_container(&container_id_bg, Some(stop_opts)),
                    )
                    .await
                    .is_err()
                    {
                        tracing::warn!(
                            target: "open_functions::runtime_container",
                            container_id = %container_id_bg,
                            "stop_container did not return within the grace period; escalating to force-remove",
                        );
                    }
                    InstanceExit::Stopped
                }
            };

            // Docker containers persist after exit unless removed, unlike a
            // `tokio::process::Child`; always remove so stopped/crashed
            // instances don't accumulate.
            force_remove(&docker_bg, &container_id_bg).await;

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
    /// The container exited before `wait_ready` resolved.
    Exited(Option<Result<ContainerWaitResponse, bollard::errors::Error>>),
}

/// Builds the container `Env` list per contracts/function-contract.md's
/// startup table, mirroring `ProcessDriver::spawn`'s `.env(...)` chain with
/// two differences: `PORT` is fixed at [`CONTAINER_PORT`] (container mode
/// always uses the fixed internal port, per the contract table), and
/// `PATH`/`HOME`/`LANG` are omitted -- those are host-process concerns the
/// function's own container image's base already handles, unlike a
/// bare-metal child process which inherits nothing without them.
///
/// User env vars are listed first so the reserved names below always win on
/// conflict, matching `ProcessDriver`'s `.envs(&spec.env)` followed by the
/// reserved `.env(...)` calls (later entries win for duplicate keys).
fn container_env(spec: &InstanceSpec) -> Vec<String> {
    let mut env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    env.push(format!("PORT={CONTAINER_PORT}"));
    env.push(format!("FUNCTION_TARGET={}", spec.entry_point));
    env.push(format!("FUNCTION_SIGNATURE_TYPE={}", spec.signature_type));
    env.push(format!("K_SERVICE={}", spec.function_name));
    env.push(format!(
        "K_REVISION={}-{:05}",
        spec.function_name, spec.revision
    ));
    env.push(format!("K_CONFIGURATION={}", spec.function_name));
    env.push(format!("OPEN_FUNCTIONS_MEMORY_MIB={}", spec.memory_mib));
    env
}

/// Pulls `image_ref` if it isn't already present locally. Per T073's task
/// line ("pull if missing"): checks with `inspect_image` first rather than
/// unconditionally re-pulling on every spawn.
async fn ensure_image(docker: &Docker, image_ref: &str) -> Result<(), DriverError> {
    match docker.inspect_image(image_ref).await {
        Ok(_) => return Ok(()),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => {}
        Err(err) => return Err(spawn_err(err)),
    }

    let options = CreateImageOptionsBuilder::default()
        .from_image(image_ref)
        .build();
    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(item) = stream.next().await {
        item.map_err(spawn_err)?;
    }
    Ok(())
}

/// Inspects `container_id` and reads its IP address on [`NETWORK_NAME`],
/// per plan.md: `inspect` reads `NetworkSettings.Networks["open-functions"].IPAddress`.
async fn inspect_addr(docker: &Docker, container_id: &str) -> Result<SocketAddr, DriverError> {
    let inspected = docker
        .inspect_container(
            container_id,
            Some(InspectContainerOptionsBuilder::default().build()),
        )
        .await
        .map_err(spawn_err)?;

    let ip_str = inspected
        .network_settings
        .as_ref()
        .and_then(|ns| ns.networks.as_ref())
        .and_then(|nets| nets.get(NETWORK_NAME))
        .and_then(|endpoint| endpoint.ip_address.as_deref())
        .filter(|ip| !ip.is_empty())
        .ok_or_else(|| {
            spawn_err(format!(
                "container {container_id} has no IP address on network {NETWORK_NAME}"
            ))
        })?;

    let ip: IpAddr = ip_str
        .parse()
        .map_err(|err| spawn_err(format!("invalid container IP {ip_str:?}: {err}")))?;

    Ok(SocketAddr::new(ip, CONTAINER_PORT))
}

/// Extracts an exit code from a `wait_container` stream item, if any. Both a
/// missing item (`None`, stream ended without yielding) and an error item
/// are treated as "exited, code unknown" -- either way the container is no
/// longer usable.
fn exit_code_from_wait(
    wait_result: Option<Result<ContainerWaitResponse, bollard::errors::Error>>,
) -> Option<i32> {
    match wait_result {
        Some(Ok(resp)) => i32::try_from(resp.status_code).ok(),
        _ => None,
    }
}

/// `stop(t=grace_secs)` then unconditional force-remove, per plan.md's
/// "stop is `stop(t=5)` -> `remove`". Used for the pre-ready timeout path;
/// the steady-state background task (post-ready) has its own copy of this
/// shape inline, since it also needs to race the stop request against the
/// container exiting on its own.
async fn stop_then_remove(docker: &Docker, container_id: &str, grace_secs: i32) {
    let stop_opts = StopContainerOptionsBuilder::default().t(grace_secs).build();
    let _ = docker.stop_container(container_id, Some(stop_opts)).await;
    force_remove(docker, container_id).await;
}

/// Force-removes a container (SIGKILL if still running, then delete).
/// Best-effort: errors are swallowed since this is always cleanup after the
/// container's fate (ready-timeout, crash, or requested stop) has already
/// been decided -- there is nothing further to report to the caller if
/// removal itself fails (e.g. it was already removed by a concurrent
/// sweep).
async fn force_remove(docker: &Docker, container_id: &str) {
    let remove_opts = RemoveContainerOptionsBuilder::default().force(true).build();
    if let Err(err) = docker
        .remove_container(container_id, Some(remove_opts))
        .await
    {
        tracing::warn!(
            target: "open_functions::runtime_container",
            container_id = %container_id,
            error = %err,
            "failed to remove container during cleanup",
        );
    }
}

/// Lists and force-removes every container carrying [`LABEL_FUNCTION`]
/// (any value -- Docker's `label=key` filter form matches regardless of the
/// label's value), including stopped ones. Intended to be called once at
/// `open-functions serve` startup (T075/T076, not this module's concern) to clean up
/// containers left over from an unclean prior shutdown, per plan.md: "sweep
/// label-tagged leftover containers at startup". Returns the number of containers
/// removed.
pub async fn sweep_stale_containers(docker: &Docker) -> Result<usize, bollard::errors::Error> {
    let filters: HashMap<String, Vec<String>> =
        HashMap::from([("label".to_string(), vec![LABEL_FUNCTION.to_string()])]);
    let containers = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await?;

    let mut removed = 0usize;
    for container in containers {
        let Some(id) = container.id else { continue };
        let remove_opts = RemoveContainerOptionsBuilder::default().force(true).build();
        match docker.remove_container(&id, Some(remove_opts)).await {
            Ok(()) => removed += 1,
            Err(err) => {
                tracing::warn!(
                    target: "open_functions::runtime_container",
                    container_id = %id,
                    error = %err,
                    "failed to remove stale container during startup sweep",
                );
            }
        }
    }
    Ok(removed)
}

/// Drains a container's combined stdout/stderr via `docker.logs(...,
/// follow=true)` (T073's plan.md note: "logs stream -> LogPipe", never
/// actually wired until T079/US5), splitting each `LogOutput` chunk into
/// lines and feeding them through the same [`pipe::parse_line`] +
/// [`pipe::emit_function_log`] + [`LogRingBuffer::push`] pipeline
/// `runtime::process::ProcessDriver`'s `spawn_log_drain` uses. Runs until the
/// log stream ends (the container is removed) or errors.
fn spawn_container_log_drain(
    docker: Docker,
    container_id: String,
    function_name: String,
    revision: u32,
    log_store: Arc<LogStore>,
) {
    tokio::spawn(async move {
        let buffer = log_store.buffer_for(&function_name);
        let options = LogsOptionsBuilder::default()
            .follow(true)
            .stdout(true)
            .stderr(true)
            .build();
        let mut stream = docker.logs(&container_id, Some(options));
        let mut stdout_buf = String::new();
        let mut stderr_buf = String::new();
        while let Some(item) = stream.next().await {
            let Ok(chunk) = item else { break };
            match chunk {
                LogOutput::StdOut { message } | LogOutput::Console { message } => {
                    feed_lines(
                        &mut stdout_buf,
                        &message,
                        LogStream::Stdout,
                        &function_name,
                        revision,
                        &container_id,
                        &buffer,
                    );
                }
                LogOutput::StdErr { message } => {
                    feed_lines(
                        &mut stderr_buf,
                        &message,
                        LogStream::Stderr,
                        &function_name,
                        revision,
                        &container_id,
                        &buffer,
                    );
                }
                LogOutput::StdIn { .. } => {}
            }
        }
    });
}

/// Appends `chunk` to `buf` and drains every complete (`\n`-terminated) line
/// out of it, parsing and retaining each one. A trailing partial line (no
/// `\n` yet) stays in `buf` for the next chunk -- Docker's log stream makes
/// no line-alignment guarantee per frame.
fn feed_lines(
    buf: &mut String,
    chunk: &[u8],
    stream: LogStream,
    function_name: &str,
    revision: u32,
    instance_id: &str,
    ring: &LogRingBuffer,
) {
    buf.push_str(&String::from_utf8_lossy(chunk));
    while let Some(pos) = buf.find('\n') {
        let line: String = buf.drain(..=pos).collect();
        let line = line.trim_end_matches('\n');
        if line.is_empty() {
            continue;
        }
        let record = pipe::parse_line(line, stream, None);
        pipe::emit_function_log(function_name, revision, instance_id, &record);
        ring.push(record);
    }
}

/// Wraps any displayable error as `DriverError::Spawn`, the only `DriverError`
/// variant broad enough to carry a Docker-API failure (its `mod.rs` doc
/// comment predates the container driver and only names `std::io::Error`,
/// which every such error is losslessly representable as via
/// `io::Error::other`).
fn spawn_err(err: impl std::fmt::Display) -> DriverError {
    DriverError::Spawn(std::io::Error::other(err.to_string()))
}
