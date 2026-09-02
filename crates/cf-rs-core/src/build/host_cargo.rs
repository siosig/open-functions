//! Host `cargo build` Builder implementation. Implemented in US1 (T033).

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

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

        let mut log_file = tokio::fs::File::create(&request.log_path).await?;

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

        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();

        // Interleave stdout/stderr into the log file in the order lines
        // arrive, flushing after each write so a concurrent `follow` of the
        // log file (a later admin-api task) sees progress live instead of a
        // buffered dump at the end.
        let wait_result = tokio::time::timeout(request.timeout, async {
            let mut stdout_done = false;
            let mut stderr_done = false;
            loop {
                tokio::select! {
                    line = stdout_lines.next_line(), if !stdout_done => {
                        match line {
                            Ok(Some(l)) => {
                                log_file.write_all(l.as_bytes()).await?;
                                log_file.write_all(b"\n").await?;
                                log_file.flush().await?;
                            }
                            Ok(None) => stdout_done = true,
                            Err(e) => return Err(e),
                        }
                    }
                    line = stderr_lines.next_line(), if !stderr_done => {
                        match line {
                            Ok(Some(l)) => {
                                log_file.write_all(l.as_bytes()).await?;
                                log_file.write_all(b"\n").await?;
                                log_file.flush().await?;
                            }
                            Ok(None) => stderr_done = true,
                            Err(e) => return Err(e),
                        }
                    }
                    else => break,
                }
                if stdout_done && stderr_done {
                    break;
                }
            }
            child.wait().await
        })
        .await;

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
