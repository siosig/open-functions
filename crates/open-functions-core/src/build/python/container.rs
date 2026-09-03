//! Container-mode `PythonBuilder` (T034): runs
//! `contracts/python-function-contract.md`'s "Dependency resolution and
//! artifacts" steps 3-4 (venv creation, dependency install, entry-point
//! verification) inside `python.container_image`, after steps 1-2
//! (snapshot, requirements resolution) run on the host -- reusing
//! [`snapshot`]/[`requirements`], exactly like [`super::host::HostPythonBuilder`].

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptionsBuilder, StartContainerOptions, WaitContainerOptionsBuilder,
};
use futures_util::StreamExt;

use super::{
    BuildLog, PythonBuildError, PythonBuildOutcome, PythonBuildRequest, PythonBuilder,
    requirements, snapshot,
};

/// Bind-mount path for the artifact directory inside the build container --
/// matches `runtime::launch::CONTAINER_ARTIFACT_DIR` so the venv's own
/// absolute paths (baked in at creation time here) stay valid when the same
/// directory is bind-mounted again at launch time.
const CONTAINER_ARTIFACT_DIR: &str = "/function";
const CONTAINER_CACHE_DIR: &str = "/cache/uv";
const CONTAINER_SOURCE_DIR: &str = "/source";

/// Custom exit codes the build script (below) uses so a single combined
/// `sh -c` invocation still yields the same fine-grained
/// [`PythonBuildError`] classification `HostPythonBuilder` gets from running
/// each step as a separate process.
const EXIT_VENV_FAILED: i64 = 91;
const EXIT_INSTALL_FAILED: i64 = 92;
const EXIT_ENTRY_FAILED: i64 = 93;

/// Runs the Python build pipeline's dependency-resolution steps inside
/// `python.container_image` (default `ghcr.io/astral-sh/uv:python3.14-trixie-slim`).
pub struct ContainerPythonBuilder {
    pub docker: Docker,
}

#[async_trait::async_trait]
impl PythonBuilder for ContainerPythonBuilder {
    async fn is_available(&self) -> bool {
        crate::runtime::docker::is_available(&self.docker).await
    }

    async fn build(
        &self,
        request: &PythonBuildRequest,
    ) -> Result<PythonBuildOutcome, PythonBuildError> {
        let log_path = request.artifact_dir.join("build.log");
        let mut log = BuildLog::create(&log_path).await?;

        log.step("snapshot").await;
        let source_dir = request.source_dir.clone();
        let snapshot_dir = request.artifact_dir.join("src");
        tokio::task::spawn_blocking(move || snapshot::snapshot_source(&source_dir, &snapshot_dir))
            .await
            .map_err(|e| PythonBuildError::Io(std::io::Error::other(e)))?
            .map_err(PythonBuildError::SnapshotFailed)?;

        log.step("resolve-requirements").await;
        requirements::resolve_requirements(
            &request.artifact_dir,
            &request.functions_framework_spec,
        )
        .await?;

        // The cache bind source must exist and be owned by the same uid the
        // container runs as (below), or `uv`'s writes into it fail with
        // permission denied -- Docker would otherwise auto-create it as
        // root the first time.
        let cache_dir = request.cache_root.join("uv");
        tokio::fs::create_dir_all(&cache_dir).await?;

        log.step("container-build").await;
        ensure_image(&self.docker, &request.container_image)
            .await
            .map_err(|err| PythonBuildError::Io(std::io::Error::other(err)))?;

        let container_name = format!(
            "open-functions-pybuild-{}-{}-{}",
            request.function_name,
            request.revision,
            uuid::Uuid::new_v4().simple()
        );
        let body = ContainerCreateBody {
            image: Some(request.container_image.clone()),
            env: Some(container_env(request)),
            cmd: Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                build_script(&request.entry_point),
            ]),
            user: Some(format!("{}:{}", host_uid(), host_gid())),
            host_config: Some(HostConfig {
                binds: Some(vec![
                    format!(
                        "{}:{}:rw",
                        request.artifact_dir.display(),
                        CONTAINER_ARTIFACT_DIR
                    ),
                    format!("{}:{}:rw", cache_dir.display(), CONTAINER_CACHE_DIR),
                    format!(
                        "{}:{}:ro",
                        request.source_dir.display(),
                        CONTAINER_SOURCE_DIR
                    ),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_options = CreateContainerOptionsBuilder::default()
            .name(&container_name)
            .build();
        let created = self
            .docker
            .create_container(Some(create_options), body)
            .await
            .map_err(|err| PythonBuildError::Io(std::io::Error::other(err)))?;
        let container_id = created.id;

        if let Err(err) = self
            .docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await
        {
            let _ = force_remove(&self.docker, &container_id).await;
            return Err(PythonBuildError::Io(std::io::Error::other(err)));
        }

        let mut wait_stream = self.docker.wait_container(
            &container_id,
            Some(WaitContainerOptionsBuilder::default().build()),
        );
        let wait_result = tokio::time::timeout(request.timeout, wait_stream.next()).await;

        let exit_code = match wait_result {
            Err(_elapsed) => {
                let _ = force_remove(&self.docker, &container_id).await;
                return Err(PythonBuildError::Timeout(request.timeout));
            }
            Ok(Some(Ok(resp))) => resp.status_code,
            // Stream ended with no item, or an error item: the container's
            // fate is unknown but it's no longer usable either way.
            Ok(None) | Ok(Some(Err(_))) => 1,
        };

        let logs = fetch_logs(&self.docker, &container_id).await;
        log.write_output(logs.as_bytes()).await;
        let _ = force_remove(&self.docker, &container_id).await;

        match exit_code {
            0 => Ok(PythonBuildOutcome {
                tool: "uv".to_string(),
            }),
            EXIT_VENV_FAILED => Err(PythonBuildError::VenvFailed(log_path)),
            EXIT_INSTALL_FAILED => Err(PythonBuildError::Install(log_path)),
            EXIT_ENTRY_FAILED => Err(PythonBuildError::EntryPoint(log_path)),
            _ => Err(PythonBuildError::Install(log_path)),
        }
    }
}

/// Builds the container's `Env` list: the allowlisted passthrough env
/// (`request.passthrough_env`, computed on the host) with `UV_CACHE_DIR`
/// and `HOME` overridden to their container-side paths (the host-side
/// values baked into `passthrough_env` don't exist inside the container).
fn container_env(request: &PythonBuildRequest) -> Vec<String> {
    let mut env = request.passthrough_env.clone();
    env.insert("UV_CACHE_DIR".to_string(), CONTAINER_CACHE_DIR.to_string());
    env.insert("HOME".to_string(), CONTAINER_ARTIFACT_DIR.to_string());
    env.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// The single combined `sh -c` script contracts/python-function-contract.md's
/// container-mode step describes: venv creation, dependency install, and
/// entry-point verification, each guarded so a failure exits with a
/// distinct code ([`EXIT_VENV_FAILED`]/[`EXIT_INSTALL_FAILED`]/
/// [`EXIT_ENTRY_FAILED`]) the caller maps back to the matching
/// [`PythonBuildError`] variant. `entry_point` is safe to interpolate
/// unescaped: `model::validate` already restricts it to `^[A-Za-z_][A-Za-z0-9_]*$`
/// before any build request reaches here.
fn build_script(entry_point: &str) -> String {
    let dir = CONTAINER_ARTIFACT_DIR;
    format!(
        "uv venv {dir}/venv --python python3.14 --no-python-downloads --no-managed-python --clear \
         || {{ echo 'venv creation failed' >&2; exit {EXIT_VENV_FAILED}; }}; \
         uv pip install --python {dir}/venv/bin/python --compile-bytecode \
         -r {dir}/requirements.open-functions.txt \
         || {{ echo 'dependency install failed' >&2; exit {EXIT_INSTALL_FAILED}; }}; \
         cd {dir}/src && {dir}/venv/bin/python -c \
         \"import importlib; m = importlib.import_module('main'); getattr(m, '{entry_point}')\" \
         || {{ echo 'entry point verification failed' >&2; exit {EXIT_ENTRY_FAILED}; }}"
    )
}

fn host_uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: `getuid(2)` takes no arguments and cannot fail.
    unsafe { getuid() }
}

fn host_gid() -> u32 {
    unsafe extern "C" {
        fn getgid() -> u32;
    }
    // SAFETY: `getgid(2)` takes no arguments and cannot fail.
    unsafe { getgid() }
}

/// Pulls `image_ref` if it isn't already present locally. Mirrors
/// `runtime::container::ensure_image`'s identical "pull if missing" logic
/// (duplicated rather than shared -- that one is private to its module and
/// returns a different error type).
async fn ensure_image(docker: &Docker, image_ref: &str) -> Result<(), String> {
    match docker.inspect_image(image_ref).await {
        Ok(_) => return Ok(()),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => {}
        Err(err) => return Err(format!("failed to inspect image {image_ref:?}: {err}")),
    }

    let options = CreateImageOptionsBuilder::default()
        .from_image(image_ref)
        .build();
    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(item) = stream.next().await {
        item.map_err(|err| format!("failed to pull image {image_ref:?}: {err}"))?;
    }
    Ok(())
}

/// Fetches a finished container's combined stdout+stderr as one string
/// (a one-shot fetch, not `follow`-streamed -- the container has already
/// exited by the time this is called).
async fn fetch_logs(docker: &Docker, container_id: &str) -> String {
    let options = LogsOptionsBuilder::default()
        .stdout(true)
        .stderr(true)
        .build();
    let mut stream = docker.logs(container_id, Some(options));
    let mut out = String::new();
    while let Some(item) = stream.next().await {
        let Ok(chunk) = item else { break };
        match chunk {
            LogOutput::StdOut { message }
            | LogOutput::StdErr { message }
            | LogOutput::Console { message } => {
                out.push_str(&String::from_utf8_lossy(&message));
            }
            LogOutput::StdIn { .. } => {}
        }
    }
    out
}

/// Force-removes a container (SIGKILL if still running, then delete).
/// Best-effort: this always runs after the container's fate has already
/// been decided, so a removal failure here has nothing further to report.
async fn force_remove(docker: &Docker, container_id: &str) {
    let options = RemoveContainerOptionsBuilder::default().force(true).build();
    if let Err(err) = docker.remove_container(container_id, Some(options)).await {
        tracing::warn!(
            target: "open_functions::build_python_container",
            container_id = %container_id,
            error = %err,
            "failed to remove build container during cleanup",
        );
    }
}
