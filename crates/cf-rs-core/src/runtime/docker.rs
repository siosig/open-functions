//! Shared Docker/`bollard` connection helper (T072), used by both
//! [`crate::runtime::container::ContainerDriver`] (US4 image-mode instances)
//! and [`crate::build::container::ContainerBuilder`] (US4 container-mode
//! source builds), per plan.md's ContainerDriver/container-builder Design
//! Notes.

use std::collections::HashMap;

use bollard::Docker;
use bollard::models::NetworkCreateRequest;
use bollard::query_parameters::ListNetworksOptionsBuilder;

/// The Docker network cf-rs creates (if missing) and connects every
/// image-mode function container to, per plan.md: create the `cf-rs` network
/// at startup if missing. If cf-rs itself runs inside Docker, Ansible connects the
/// cf-rs container to this same network so the reverse proxy can reach
/// function containers by IP.
pub const NETWORK_NAME: &str = "cf-rs";

/// Label applied to every container cf-rs creates, keyed by function name.
/// Used both to identify a function's own container(s) and, at startup, to
/// find and remove stale containers left over from a previous unclean
/// shutdown (plan.md: sweep label-tagged leftover containers at startup).
pub const LABEL_FUNCTION: &str = "cf-rs.function";

#[derive(Debug, thiserror::Error)]
pub enum DockerConnectError {
    #[error("failed to connect to the Docker daemon: {0}")]
    Connect(#[source] bollard::errors::Error),
}

/// Connects to the Docker daemon. `docker_socket` is `runtime.docker_socket`
/// from config: empty means "use bollard's own default resolution"
/// (`DOCKER_HOST` env var, else the platform-default local socket), per
/// ops-config.md's `[runtime]` schema.
///
/// This only builds a client — it does not itself verify the daemon is
/// actually reachable; use [`is_available`] for that. Note one exception:
/// for an *explicit, non-empty* `docker_socket`, `bollard` checks
/// synchronously that the path exists on disk (returning
/// `DockerConnectError` immediately if not) — a nonexistent path is
/// therefore a `connect()`-time error, not an `is_available()`-time one; a
/// path that exists but has nothing listening on it (e.g. a stale socket
/// file) still only fails at `is_available()`/actual-request time.
pub fn connect(docker_socket: &str) -> Result<Docker, DockerConnectError> {
    let docker = if docker_socket.is_empty() {
        Docker::connect_with_local_defaults()
    } else {
        Docker::connect_with_socket(docker_socket, 120, bollard::API_DEFAULT_VERSION)
    };
    docker.map_err(DockerConnectError::Connect)
}

/// Probes whether the Docker daemon is actually reachable right now.
/// `build.mode = auto` (register-time build-driver selection: host cargo if
/// available, else a container build, else reject with 412) and image-mode
/// registration's own availability precondition (T071) both need this — a
/// successfully *constructed* [`Docker`] client proves nothing about
/// whether a daemon is listening on the other end of the socket.
pub async fn is_available(docker: &Docker) -> bool {
    docker.ping().await.is_ok()
}

#[derive(Debug, thiserror::Error)]
pub enum EnsureNetworkError {
    #[error(transparent)]
    Docker(#[from] bollard::errors::Error),
}

/// Ensures the shared [`NETWORK_NAME`] Docker network exists, creating it if
/// not. Idempotent: a 409 (already exists — e.g. a concurrent creator raced
/// this call) is treated as success rather than an error, matching the
/// "create at startup if missing" contract rather than a
/// strict "must not already exist" one.
pub async fn ensure_network(docker: &Docker) -> Result<(), EnsureNetworkError> {
    let filters = HashMap::from([("name".to_string(), vec![NETWORK_NAME.to_string()])]);
    let existing = docker
        .list_networks(Some(
            ListNetworksOptionsBuilder::default()
                .filters(&filters)
                .build(),
        ))
        .await?;
    if existing
        .iter()
        .any(|n| n.name.as_deref() == Some(NETWORK_NAME))
    {
        return Ok(());
    }

    let result = docker
        .create_network(NetworkCreateRequest {
            name: NETWORK_NAME.to_string(),
            ..Default::default()
        })
        .await;
    match result {
        Ok(_) => Ok(()),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 409, ..
        }) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_with_empty_socket_uses_local_defaults() {
        // `connect_with_local_defaults()` only builds a client (parses env
        // vars / resolves the default socket path) — it never touches the
        // network, so this must succeed even with no Docker daemon running.
        assert!(connect("").is_ok());
    }

    #[test]
    fn connect_with_explicit_socket_succeeds() {
        assert!(connect("unix:///var/run/docker.sock").is_ok());
    }
}
