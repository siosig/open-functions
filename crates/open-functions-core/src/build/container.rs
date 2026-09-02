//! Container `cargo build` Builder implementation (T074, US4): builds a
//! function's source *inside* a `rust:1-bookworm` container via `bollard`,
//! instead of running the host's own `cargo` (as `host_cargo::HostCargoBuilder`
//! does). Mirrors `HostCargoBuilder`'s overall shape and `BuildError`
//! semantics; see that module's doc comments for the parts that are
//! identical (bin-target resolution, log/artifact directory setup, artifact
//! copy + executable-bit handling).
//!
//! Per plan.md's "container mode" design notes (research.md R6): pull
//! `rust:1-bookworm` if not already present locally, then create a container
//! with three bind mounts (source dir: read-only; the function's
//! `CARGO_TARGET_DIR`: read-write; the shared cargo registry cache dir:
//! read-write onto `/usr/local/cargo/registry`), run `cargo build --release`
//! inside it as the *host's* uid:gid (so files written into the bind-mounted
//! host directories aren't left owned by root), and use its exit code the
//! same way `HostCargoBuilder` uses its child process's exit status.

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateImageOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::build::{BuildError, BuildRequest, Builder, metadata};
use crate::runtime::docker;

/// The build image, per plan.md's design notes.
const IMAGE: &str = "rust:1-bookworm";

/// Container-internal mount point for the (read-only) source directory.
const CONTAINER_SOURCE_DIR: &str = "/build/source";

/// Container-internal mount point for the (read-write) `CARGO_TARGET_DIR`.
const CONTAINER_TARGET_DIR: &str = "/build/target";

/// Container-internal `CARGO_HOME`. `rust:1-bookworm` already defaults
/// `CARGO_HOME` to this path; it is set explicitly here anyway so the
/// registry-cache bind target below (a subdirectory of it) is correct
/// regardless of the base image's own default.
const CONTAINER_CARGO_HOME: &str = "/usr/local/cargo";

/// Builds function source directories inside a `rust:1-bookworm` Docker
/// container (as opposed to `host_cargo::HostCargoBuilder`, which runs the
/// host's own `cargo` directly).
pub struct ContainerBuilder {
    /// `runtime.docker_socket` from config: empty means "use bollard's own
    /// default resolution" (see `runtime::docker::connect`).
    pub docker_socket: String,
}

#[async_trait::async_trait]
impl Builder for ContainerBuilder {
    async fn is_available(&self) -> bool {
        match docker::connect(&self.docker_socket) {
            Ok(client) => docker::is_available(&client).await,
            Err(_) => false,
        }
    }

    async fn build(&self, request: &BuildRequest) -> Result<(), BuildError> {
        let source_dir = request.source_dir.clone();
        let requested_bin = request.bin.clone();
        let bin_name = tokio::task::spawn_blocking(move || {
            metadata::resolve_bin_target(&source_dir, requested_bin.as_deref())
        })
        .await
        .map_err(|e| BuildError::Io(std::io::Error::other(e)))??;

        let (uid, gid) = tokio::task::spawn_blocking(resolve_host_uid_gid)
            .await
            .map_err(|e| BuildError::Io(std::io::Error::other(e)))??;

        if let Some(parent) = request.log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if let Some(parent) = request.artifact_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // These two are bind-mount sources: create them as the host process's
        // own uid/gid *before* the container starts, so a container running
        // as that same uid:gid (see `user` below) can actually write into
        // them. A missing bind-mount source dir would otherwise be
        // auto-created by dockerd itself (typically root-owned), which the
        // unprivileged in-container user could not write to.
        tokio::fs::create_dir_all(&request.cargo_target_dir).await?;
        tokio::fs::create_dir_all(&request.cache_dir).await?;

        let mut log_file = tokio::fs::File::create(&request.log_path).await?;

        let docker = docker::connect(&self.docker_socket)
            .map_err(|e| BuildError::Io(std::io::Error::other(e)))?;

        ensure_image_present(&docker).await?;

        let host_config = HostConfig {
            binds: Some(vec![
                format!(
                    "{}:{}:ro",
                    request.source_dir.display(),
                    CONTAINER_SOURCE_DIR
                ),
                format!(
                    "{}:{}:rw",
                    request.cargo_target_dir.display(),
                    CONTAINER_TARGET_DIR
                ),
                format!(
                    "{}:{}:rw",
                    request.cache_dir.display(),
                    format!("{CONTAINER_CARGO_HOME}/registry")
                ),
            ]),
            ..Default::default()
        };

        let config = ContainerCreateBody {
            image: Some(IMAGE.to_string()),
            cmd: Some(vec![
                "cargo".to_string(),
                "build".to_string(),
                "--release".to_string(),
            ]),
            working_dir: Some(CONTAINER_SOURCE_DIR.to_string()),
            env: Some(vec![
                format!("CARGO_TARGET_DIR={CONTAINER_TARGET_DIR}"),
                format!("CARGO_HOME={CONTAINER_CARGO_HOME}"),
                "CARGO_TERM_COLOR=never".to_string(),
            ]),
            host_config: Some(host_config),
            user: Some(format!("{uid}:{gid}")),
            ..Default::default()
        };

        let created = docker
            .create_container(None, config)
            .await
            .map_err(|e| BuildError::Spawn(std::io::Error::other(e)))?;
        let container_id = created.id;

        let outcome = run_and_wait(&docker, &container_id, request, &mut log_file).await;

        // Always remove the build container (force: true kills it first if
        // it's still running, e.g. after a timeout) so build containers
        // don't accumulate across runs.
        let _ = docker
            .remove_container(
                &container_id,
                Some(RemoveContainerOptionsBuilder::default().force(true).build()),
            )
            .await;

        outcome?;

        let built_path = request.cargo_target_dir.join("release").join(&bin_name);
        tokio::fs::copy(&built_path, &request.artifact_path)
            .await
            .map_err(|source| BuildError::CopyArtifact {
                from: built_path.clone(),
                to: request.artifact_path.clone(),
                source,
            })?;

        // See `HostCargoBuilder::build`'s identical step: `fs::copy` already
        // preserves the source file's permissions on Unix, but set the
        // executable bits explicitly anyway as a robustness safety net.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = tokio::fs::metadata(&request.artifact_path).await?;
            let mut perms = metadata.permissions();
            let mode = perms.mode() | 0o755;
            perms.set_mode(mode);
            tokio::fs::set_permissions(&request.artifact_path, perms).await?;
        }

        Ok(())
    }
}

/// Starts the already-created container, then concurrently drains its
/// combined stdout+stderr log stream into `log_file` and waits for it to
/// exit, both bounded by `request.timeout`. Returns `Ok(())` on a zero exit
/// code, `BuildError::NonZeroExit` on a non-zero one, or
/// `BuildError::Timeout` if `request.timeout` elapses first.
async fn run_and_wait(
    docker: &Docker,
    container_id: &str,
    request: &BuildRequest,
    log_file: &mut tokio::fs::File,
) -> Result<(), BuildError> {
    docker
        .start_container(container_id, None)
        .await
        .map_err(|e| BuildError::Spawn(std::io::Error::other(e)))?;

    let logs_stream = docker.logs(
        container_id,
        Some(
            LogsOptionsBuilder::default()
                .follow(true)
                .stdout(true)
                .stderr(true)
                .build(),
        ),
    );
    tokio::pin!(logs_stream);

    let wait_stream = docker.wait_container(container_id, None);
    tokio::pin!(wait_stream);

    let timed = tokio::time::timeout(request.timeout, async {
        tokio::join!(drain_logs(&mut logs_stream, log_file), wait_stream.next())
    })
    .await;

    let (logs_result, wait_item) = match timed {
        Ok(pair) => pair,
        Err(_elapsed) => return Err(BuildError::Timeout(request.timeout)),
    };

    logs_result.map_err(BuildError::Io)?;

    let status_code: i64 = match wait_item {
        Some(Ok(response)) => response.status_code,
        Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => code,
        Some(Err(e)) => return Err(BuildError::Io(std::io::Error::other(e))),
        None => {
            return Err(BuildError::Io(std::io::Error::other(
                "docker wait_container stream ended without a response",
            )));
        }
    };

    if status_code != 0 {
        return Err(BuildError::NonZeroExit(
            status_code as i32,
            request.log_path.clone(),
        ));
    }

    Ok(())
}

/// Drains a container log stream to completion, writing each chunk's bytes
/// to `log_file` as it arrives (combined stdout+stderr, chronologically, per
/// plan.md's `build.log` contract). With `follow: true` this stream ends
/// once the container stops producing output, i.e. once it exits.
async fn drain_logs<S>(mut stream: S, log_file: &mut tokio::fs::File) -> std::io::Result<()>
where
    S: futures_util::Stream<Item = Result<LogOutput, bollard::errors::Error>> + Unpin,
{
    while let Some(item) = stream.next().await {
        let output = match item {
            Ok(output) => output,
            // A log-stream read error doesn't decide build success/failure
            // (that's `wait_container`'s job) -- stop draining and let the
            // caller's `wait_container` result decide the outcome.
            Err(_e) => break,
        };
        let bytes: &[u8] = match &output {
            LogOutput::StdOut { message } => message,
            LogOutput::StdErr { message } => message,
            LogOutput::StdIn { message } => message,
            LogOutput::Console { message } => message,
        };
        log_file.write_all(bytes).await?;
        log_file.flush().await?;
    }
    Ok(())
}

/// Pulls `IMAGE` if it isn't already present locally.
async fn ensure_image_present(docker: &Docker) -> Result<(), BuildError> {
    match docker.inspect_image(IMAGE).await {
        Ok(_) => return Ok(()),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => {}
        Err(e) => return Err(BuildError::Io(std::io::Error::other(e))),
    }

    let pull_stream = docker.create_image(
        Some(
            CreateImageOptionsBuilder::default()
                .from_image(IMAGE)
                .build(),
        ),
        None,
        None,
    );
    tokio::pin!(pull_stream);
    while let Some(item) = pull_stream.next().await {
        item.map_err(|e| BuildError::Io(std::io::Error::other(e)))?;
    }
    Ok(())
}

/// Resolves the host process's own uid:gid via `id -u`/`id -g` (shelled out
/// rather than pulling in a new FFI dependency for `getuid`/`getgid`, since
/// no such dependency is already present in this workspace). Used to run the
/// build container as this same uid:gid, per plan.md's design notes, so
/// files it writes into the bind-mounted host directories (target dir,
/// cache dir) are owned by the host process and not by root.
fn resolve_host_uid_gid() -> Result<(u32, u32), BuildError> {
    Ok((run_id_command("-u")?, run_id_command("-g")?))
}

fn run_id_command(flag: &str) -> Result<u32, BuildError> {
    let output = std::process::Command::new("id")
        .arg(flag)
        .output()
        .map_err(BuildError::Spawn)?;
    if !output.status.success() {
        return Err(BuildError::Spawn(std::io::Error::other(format!(
            "`id {flag}` exited with status {:?}",
            output.status.code()
        ))));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .map_err(|e| BuildError::Spawn(std::io::Error::other(e)))
}
