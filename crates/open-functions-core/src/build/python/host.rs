//! Host-mode `PythonBuilder` (T028): runs every dependency-resolution step
//! directly on the host, per `contracts/python-function-contract.md`'s
//! "Dependency resolution and artifacts" steps 1-4 (step 5, container-mode, is a separate
//! `ContainerPythonBuilder`, T034).

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;

use super::{
    BuildLog, Installer, PythonBuildError, PythonBuildOutcome, PythonBuildRequest, PythonBuilder,
    requirements, snapshot,
};

/// Interpreter names tried in order when `python.python_bin` is unset.
const AUTODETECT_CANDIDATES: &[&str] = &["python3.14", "python3", "python"];

/// Runs the Python build pipeline with the host's own `python`/`uv`/`pip`.
pub struct HostPythonBuilder {
    /// `python.python_bin` (empty string = autodetect), from config.
    pub python_bin_override: String,
    /// `python.uv_bin`, e.g. `"uv"`.
    pub uv_bin: String,
}

#[async_trait::async_trait]
impl PythonBuilder for HostPythonBuilder {
    async fn is_available(&self) -> bool {
        resolve_python_bin(&self.python_bin_override).await.is_ok()
    }

    async fn build(
        &self,
        request: &PythonBuildRequest,
    ) -> Result<PythonBuildOutcome, PythonBuildError> {
        let deadline = Instant::now() + request.timeout;
        let log_path = request.artifact_dir.join("build.log");
        let mut log = BuildLog::create(&log_path).await?;

        let python_bin = resolve_python_bin(&request.python_bin.clone().unwrap_or_default())
            .await
            .map_err(|tried| PythonBuildError::UnsupportedPython { tried })?;

        log.step("snapshot").await;
        let source_dir = request.source_dir.clone();
        let snapshot_dir = request.artifact_dir.join("src");
        tokio::task::spawn_blocking(move || snapshot::snapshot_source(&source_dir, &snapshot_dir))
            .await
            .map_err(|e| PythonBuildError::Io(std::io::Error::other(e)))?
            .map_err(PythonBuildError::SnapshotFailed)?;

        log.step("resolve-requirements").await;
        let requirements_path = requirements::resolve_requirements(
            &request.artifact_dir,
            &request.functions_framework_spec,
        )
        .await?;

        let venv_dir = request.artifact_dir.join("venv");
        let tool = resolve_tool(request.installer, &self.uv_bin).await;
        create_venv_and_install(
            &tool,
            &self.uv_bin,
            &python_bin,
            &venv_dir,
            &requirements_path,
            &request.passthrough_env,
            &mut log,
            &log_path,
            remaining(deadline)?,
        )
        .await?;

        verify_entry_point(
            &venv_dir,
            &request.artifact_dir.join("src"),
            &request.entry_point,
            &mut log,
            &log_path,
            remaining(deadline)?,
        )
        .await?;

        Ok(PythonBuildOutcome {
            tool: tool.as_str().to_string(),
        })
    }
}

fn remaining(deadline: Instant) -> Result<Duration, PythonBuildError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(PythonBuildError::Timeout(Duration::ZERO));
    }
    Ok(deadline - now)
}

#[derive(Debug, Clone, Copy)]
enum Tool {
    Uv,
    Pip,
}

impl Tool {
    fn as_str(self) -> &'static str {
        match self {
            Tool::Uv => "uv",
            Tool::Pip => "pip",
        }
    }
}

async fn resolve_tool(installer: Installer, uv_bin: &str) -> Tool {
    match installer {
        Installer::Uv => Tool::Uv,
        Installer::Pip => Tool::Pip,
        Installer::Auto => {
            if uv_available(uv_bin).await {
                Tool::Uv
            } else {
                Tool::Pip
            }
        }
    }
}

async fn uv_available(uv_bin: &str) -> bool {
    Command::new(uv_bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Tries `override_bin` (if non-empty) or, if empty, each of
/// [`AUTODETECT_CANDIDATES`] in order, verifying `sys.version_info[:2] ==
/// (3, 14)` for each candidate that's actually runnable. Returns the first
/// match's resolved path/name, or every name tried (for the error message)
/// if none matched.
async fn resolve_python_bin(override_bin: &str) -> Result<String, Vec<String>> {
    let candidates: Vec<&str> = if override_bin.is_empty() {
        AUTODETECT_CANDIDATES.to_vec()
    } else {
        vec![override_bin]
    };

    let mut tried = Vec::new();
    for candidate in candidates {
        tried.push(candidate.to_string());
        if is_python_314(candidate).await {
            return Ok(candidate.to_string());
        }
    }
    Err(tried)
}

async fn is_python_314(bin: &str) -> bool {
    let output = Command::new(bin)
        .args(["-c", "import sys; print(sys.version_info[:2])"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim() == "(3, 14)"
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn create_venv_and_install(
    tool: &Tool,
    uv_bin: &str,
    python_bin: &str,
    venv_dir: &Path,
    requirements_path: &Path,
    env: &std::collections::BTreeMap<String, String>,
    log: &mut BuildLog,
    log_path: &Path,
    timeout: Duration,
) -> Result<(), PythonBuildError> {
    let deadline = Instant::now() + timeout;
    match tool {
        Tool::Uv => {
            let mut venv_cmd = Command::new(uv_bin);
            venv_cmd
                .arg("venv")
                .arg(venv_dir)
                .arg("--python")
                .arg(python_bin)
                .arg("--no-python-downloads")
                .arg("--no-managed-python")
                // A revision's artifact dir is only allocated a new number on a
                // *successful* build (`existing.current_revision + 1`, reused
                // across repeated failures at the same number -- see
                // service.rs's `register_source_python`), so a retry after any
                // failed attempt targets the same `venv_dir`. Without `--clear`,
                // uv refuses to create a venv over one that already exists
                // (even a partial one from the failed attempt), so the retry
                // fails on this step regardless of whether the actual problem
                // was fixed.
                .arg("--clear")
                .envs(env);
            if !run_logged(log, "venv (uv)", venv_cmd, remaining(deadline)?).await? {
                return Err(PythonBuildError::VenvFailed(log_path.to_path_buf()));
            }

            let mut install_cmd = Command::new(uv_bin);
            install_cmd
                .arg("pip")
                .arg("install")
                .arg("--python")
                .arg(venv_dir.join("bin/python"))
                .arg("--compile-bytecode")
                .arg("-r")
                .arg(requirements_path)
                .envs(env);
            if !run_logged(log, "install (uv)", install_cmd, remaining(deadline)?).await? {
                return Err(PythonBuildError::Install(log_path.to_path_buf()));
            }
        }
        Tool::Pip => {
            let mut venv_cmd = Command::new(python_bin);
            venv_cmd
                .arg("-m")
                .arg("venv")
                .arg("--clear") // see the uv branch above for why this is needed on a retry
                .arg(venv_dir)
                .envs(env);
            if !run_logged(log, "venv (pip)", venv_cmd, remaining(deadline)?).await? {
                return Err(PythonBuildError::VenvFailed(log_path.to_path_buf()));
            }

            let venv_python = venv_dir.join("bin/python");
            let mut install_cmd = Command::new(&venv_python);
            install_cmd
                .arg("-m")
                .arg("pip")
                .arg("install")
                .arg("--disable-pip-version-check")
                .arg("-r")
                .arg(requirements_path)
                .envs(env);
            if !run_logged(log, "install (pip)", install_cmd, remaining(deadline)?).await? {
                return Err(PythonBuildError::Install(log_path.to_path_buf()));
            }

            let mut compile_cmd = Command::new(&venv_python);
            compile_cmd
                .arg("-m")
                .arg("compileall")
                .arg("-q")
                .arg(venv_dir);
            // Best-effort: a compileall failure doesn't invalidate an
            // otherwise-successful install (bytecode is only a warm-start
            // optimization here, unlike uv's `--compile-bytecode` which is
            // part of the same install invocation).
            let _ = run_logged(log, "compileall (pip)", compile_cmd, remaining(deadline)?).await;
        }
    }
    Ok(())
}

async fn verify_entry_point(
    venv_dir: &Path,
    source_dir: &Path,
    entry_point: &str,
    log: &mut BuildLog,
    log_path: &Path,
    timeout: Duration,
) -> Result<(), PythonBuildError> {
    let mut cmd = Command::new(venv_dir.join("bin/python"));
    cmd.arg("-c").arg(format!(
        "import importlib; m = importlib.import_module('main'); getattr(m, '{entry_point}')"
    ));
    cmd.current_dir(source_dir);
    if !run_logged(log, "verify-entry-point", cmd, timeout).await? {
        return Err(PythonBuildError::EntryPoint(log_path.to_path_buf()));
    }
    Ok(())
}

/// Runs `command` to completion (or `timeout`), writing a `== step: {step}
/// ==` header plus interleaved stdout+stderr to `log`. Returns whether the
/// process exited successfully; the caller maps that to the appropriate
/// [`PythonBuildError`] variant (different steps fail differently).
async fn run_logged(
    log: &mut BuildLog,
    step: &str,
    mut command: Command,
    timeout: Duration,
) -> Result<bool, PythonBuildError> {
    log.step(step).await;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_elapsed| PythonBuildError::Timeout(timeout))?
        .map_err(PythonBuildError::Io)?;
    log.write_output(&output.stdout).await;
    log.write_output(&output.stderr).await;
    Ok(output.status.success())
}
