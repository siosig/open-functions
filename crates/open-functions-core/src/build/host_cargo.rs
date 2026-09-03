//! Host `cargo build` Builder implementation. Implemented in US1 (T033).

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout, Command};

use crate::build::{BuildError, BuildRequest, Builder, metadata};

/// Builds function source directories with the host's own `cargo` binary
/// (as opposed to `container::ContainerBuilder`, which builds inside
/// `rust:1-bookworm`).
pub struct HostCargoBuilder {
    /// e.g. `"cargo"`, from `config.build.cargo_bin`.
    pub cargo_bin: String,
}

#[async_trait::async_trait]
impl Builder for HostCargoBuilder {
    async fn is_available(&self) -> bool {
        Command::new(&self.cargo_bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success())
    }

    async fn build(&self, request: &BuildRequest) -> Result<(), BuildError> {
        let source_dir = request.source_dir.clone();
        let requested_bin = request.bin.clone();
        let bin_name = tokio::task::spawn_blocking(move || {
            metadata::resolve_bin_target(&source_dir, requested_bin.as_deref())
        })
        .await
        .map_err(|e| BuildError::Io(std::io::Error::other(e)))??;

        if let Some(parent) = request.log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if let Some(parent) = request.artifact_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let log_file = tokio::fs::File::create(&request.log_path).await?;

        let mut child = Command::new(&self.cargo_bin)
            .arg("build")
            .arg("--release")
            .arg("--target-dir")
            .arg(&request.cargo_target_dir)
            .current_dir(&request.source_dir)
            .env("CARGO_TERM_COLOR", "never")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(BuildError::Spawn)?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BuildError::Io(std::io::Error::other("child stdout missing")))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| BuildError::Io(std::io::Error::other("child stderr missing")))?;

        // Detached, not awaited as part of build completion: `child.wait()`
        // below is the only reliable "the build process itself is done"
        // signal. A grandchild the compiler spawns and that outlives cargo
        // (e.g. an `RUSTC_WRAPPER` compiler-cache daemon such as sccache,
        // which auto-starts a persistent server that inherits these same
        // piped fds) can keep a pipe's write end open long after `cargo`
        // itself exits -- waiting for both pipes to reach EOF before ever
        // calling `child.wait()` (the previous design here) would then hang
        // for the full build timeout even though the build already finished.
        // Mirrors `runtime::process::ProcessDriver`'s own log-draining,
        // which is likewise fire-and-forget rather than part of the
        // spawn/exit critical path.
        tokio::spawn(drain_build_output_to_log(stdout, stderr, log_file));

        let wait_result = tokio::time::timeout(request.timeout, child.wait()).await;

        let status = match wait_result {
            Ok(Ok(status)) => status,
            Ok(Err(source)) => return Err(BuildError::Io(source)),
            Err(_elapsed) => {
                // Best-effort kill; `kill_on_drop(true)` also covers the case
                // where this future is cancelled before we get here.
                let _ = child.kill().await;
                return Err(BuildError::Timeout(request.timeout));
            }
        };

        if !status.success() {
            return Err(BuildError::NonZeroExit(
                status.code().unwrap_or(-1),
                request.log_path.clone(),
            ));
        }

        let built_path = request.cargo_target_dir.join("release").join(&bin_name);
        tokio::fs::copy(&built_path, &request.artifact_path)
            .await
            .map_err(|source| BuildError::CopyArtifact {
                from: built_path.clone(),
                to: request.artifact_path.clone(),
                source,
            })?;

        // `fs::copy` already preserves permissions on Unix, so the built
        // release binary (typically 0o755) should already be executable.
        // Set the bits explicitly anyway as a robustness safety net in case
        // the source permissions were ever more restrictive.
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

/// Interleaves `stdout`/`stderr` into `log_file` in the order lines arrive,
/// flushing after each write so a concurrent `follow` of the log file (T081)
/// sees progress live rather than a buffered dump at the end. Runs until
/// both streams reach EOF -- which, per the caller's own doc comment, may
/// never happen if a grandchild process outlives `cargo` holding a pipe
/// open; that's fine here since this task is detached and not on the
/// build's own completion path. Write errors are swallowed (best-effort
/// logging must never be why an otherwise-successful build fails).
async fn drain_build_output_to_log(
    stdout: ChildStdout,
    stderr: ChildStderr,
    mut log_file: tokio::fs::File,
) {
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();
    let mut stdout_done = false;
    let mut stderr_done = false;
    loop {
        tokio::select! {
            line = stdout_lines.next_line(), if !stdout_done => {
                match line {
                    Ok(Some(l)) => write_log_line(&mut log_file, &l).await,
                    Ok(None) | Err(_) => stdout_done = true,
                }
            }
            line = stderr_lines.next_line(), if !stderr_done => {
                match line {
                    Ok(Some(l)) => write_log_line(&mut log_file, &l).await,
                    Ok(None) | Err(_) => stderr_done = true,
                }
            }
            else => break,
        }
        if stdout_done && stderr_done {
            break;
        }
    }
}

async fn write_log_line(log_file: &mut tokio::fs::File, line: &str) {
    let _ = log_file.write_all(line.as_bytes()).await;
    let _ = log_file.write_all(b"\n").await;
    let _ = log_file.flush().await;
}
