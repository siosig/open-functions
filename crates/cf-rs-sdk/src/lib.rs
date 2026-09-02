//! SDK for writing Rust functions compatible with the Google Cloud Run functions
//! Functions Framework contract, runnable locally on cf-rs and unmodified on Cloud Run.
//!
//! Start with [`Functions`]: register one or more handlers with
//! [`Functions::http`] or [`Functions::cloud_event`], then call
//! [`Functions::serve`] to resolve `PORT` / `FUNCTION_TARGET` /
//! `FUNCTION_SIGNATURE_TYPE` from the environment and start listening. See
//! this crate's `README.md` for complete worked examples.

#![warn(missing_docs)]

pub mod cloudevent;
pub mod env;
pub mod http;
pub mod logging;
pub mod pubsub;

use axum::Router;

/// Errors returned by the SDK during setup or serving.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// [`Functions::serve`] could not bind a TCP listener to the resolved
    /// address (usually `0.0.0.0:<PORT>`), e.g. because the port is already
    /// in use.
    #[error("failed to bind to {addr}: {source}")]
    Bind {
        /// The address that could not be bound.
        addr: std::net::SocketAddr,
        /// The underlying OS error from the bind attempt.
        #[source]
        source: std::io::Error,
    },
    /// `FUNCTION_TARGET` (or its default, `"function"`) does not match the
    /// name any handler was registered under via [`Functions::http`] or
    /// [`Functions::cloud_event`].
    #[error("FUNCTION_TARGET={target:?} is not registered")]
    MissingTarget {
        /// The `FUNCTION_TARGET` value that was looked up, if the
        /// environment variable was set at all.
        target: Option<String>,
    },
    /// `FUNCTION_SIGNATURE_TYPE` was set to `http` for a target registered
    /// via [`Functions::cloud_event`], or to `cloudevent` for a target
    /// registered via [`Functions::http`].
    #[error(
        "FUNCTION_SIGNATURE_TYPE={configured} does not match the registered signature {actual} for target {target}"
    )]
    SignatureMismatch {
        /// The `FUNCTION_TARGET` whose signature did not match.
        target: String,
        /// The `FUNCTION_SIGNATURE_TYPE` value that was configured (`"http"` or `"cloudevent"`).
        configured: String,
        /// The signature type the target was actually registered as (`"http"` or `"cloudevent"`).
        actual: &'static str,
    },
    /// `PORT` was set to a value that does not parse as a `u16`.
    #[error("invalid PORT value {value:?}")]
    InvalidPort {
        /// The raw, unparsable `PORT` value.
        value: String,
    },
    /// The `axum` server returned an I/O error while serving requests.
    #[error("server error: {0}")]
    Serve(#[source] std::io::Error),
}

/// Builder for registering HTTP and CloudEvent functions.
///
/// A process typically registers exactly one target and calls
/// [`serve`](Functions::serve), but multiple targets can be registered in
/// the same binary; which one actually runs is selected at startup by
/// `FUNCTION_TARGET` (see [`env::function_target`]).
#[derive(Default)]
pub struct Functions {
    http_targets: std::collections::HashMap<String, Router>,
    cloud_event_targets: std::collections::HashMap<String, Router>,
}

impl Functions {
    /// Create an empty builder with no registered targets.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an HTTP function under `name`, using any `axum` `Handler`.
    pub fn http<H, T>(mut self, name: impl Into<String>, handler: H) -> Self
    where
        H: axum::handler::Handler<T, ()>,
        T: 'static,
    {
        let router = http::build_router(handler);
        self.http_targets.insert(name.into(), router);
        self
    }

    /// Register a CloudEvent function under `name`.
    pub fn cloud_event<H>(mut self, name: impl Into<String>, handler: H) -> Self
    where
        H: cloudevent::CloudEventHandler,
    {
        let router = cloudevent::build_router(handler);
        self.cloud_event_targets.insert(name.into(), router);
        self
    }

    /// Resolve `FUNCTION_TARGET` / `FUNCTION_SIGNATURE_TYPE` / `PORT` from the environment
    /// and return the `axum::Router` that would be served, without binding a socket.
    /// Intended for tests (`tower::ServiceExt::oneshot`).
    pub fn router(&self) -> Result<Router, Error> {
        let target = env::function_target();
        let signature = env::signature_type();

        if let Some(router) = self.http_targets.get(&target) {
            if signature == env::SignatureType::CloudEvent {
                return Err(Error::SignatureMismatch {
                    target,
                    configured: "cloudevent".to_string(),
                    actual: "http",
                });
            }
            return Ok(router.clone());
        }
        if let Some(router) = self.cloud_event_targets.get(&target) {
            if signature == env::SignatureType::Http {
                return Err(Error::SignatureMismatch {
                    target,
                    configured: "http".to_string(),
                    actual: "cloudevent",
                });
            }
            return Ok(router.clone());
        }
        Err(Error::MissingTarget {
            target: Some(target),
        })
    }

    /// Resolve configuration from the environment and serve until the process receives
    /// a termination signal. Errors during setup are returned; the caller should exit(1).
    pub async fn serve(self) -> Result<(), Error> {
        logging::init();
        let router = self.router()?;
        let port = env::port()?;
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|source| Error::Bind { addr, source })?;
        tracing::info!(%addr, "serving function");
        axum::serve(listener, router).await.map_err(Error::Serve)?;
        Ok(())
    }
}

/// The incoming request passed to an HTTP-triggered handler.
///
/// A type alias for `axum::extract::Request`, mirroring the Functions
/// Framework's `HttpRequest`. Any `axum` extractor (`Json`, `Query`,
/// `Path`, ...) can be used in a handler's signature in its place, since
/// [`Functions::http`] accepts any `axum::handler::Handler`.
pub type HttpRequest = axum::extract::Request;

/// The response returned by an HTTP-triggered handler.
///
/// A type alias for `axum::response::Response`, mirroring the Functions
/// Framework's `HttpResponse`. Anything implementing `axum::response::IntoResponse`
/// can be returned from a handler in its place.
pub type HttpResponse = axum::response::Response;

pub use cloudevent::{CloudEvent, CloudEventExt};
