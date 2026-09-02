//! HTTP function registration (Functions Framework `http` signature type).

use axum::Router;
use axum::http::StatusCode;
use axum::routing::any;

pub(crate) fn build_router<H, T>(handler: H) -> Router
where
    H: axum::handler::Handler<T, ()>,
    T: 'static,
{
    Router::new()
        .route("/robots.txt", any(not_found))
        .route("/favicon.ico", any(not_found))
        .fallback(handler)
        .layer(axum::middleware::from_fn(
            crate::logging::execution_id_middleware,
        ))
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}
