//! SDK for writing Rust functions compatible with the Google Cloud Run functions
//! Functions Framework contract, runnable locally on cf-rs and unmodified on Cloud Run.

pub mod cloudevent;
pub mod env;
pub mod http;
pub mod logging;
pub mod pubsub;

use axum::Router;

/// Errors returned by the SDK during setup or serving.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to bind to {addr}: {source}")]
    Bind {
        addr: std::net::SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("FUNCTION_TARGET={target:?} is not registered")]
    MissingTarget { target: Option<String> },
    #[error(
        "FUNCTION_SIGNATURE_TYPE={configured} does not match the registered signature {actual} for target {target}"
    )]
    SignatureMismatch {
        target: String,
        configured: String,
        actual: &'static str,
    },
    #[error("invalid PORT value {value:?}")]
    InvalidPort { value: String },
    #[error("server error: {0}")]
    Serve(#[source] std::io::Error),
}

/// Builder for registering HTTP and CloudEvent functions.
#[derive(Default)]
pub struct Functions {
    http_targets: std::collections::HashMap<String, Router>,
    cloud_event_targets: std::collections::HashMap<String, Router>,
}

impl Functions {
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

/// Type aliases mirroring the Functions Framework's `HttpRequest` / `HttpResponse`.
pub type HttpRequest = axum::extract::Request;
pub type HttpResponse = axum::response::Response;

pub use cloudevent::{CloudEvent, CloudEventExt};
