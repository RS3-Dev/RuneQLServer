pub mod handlers;
pub mod packed;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::http::handlers::AppState;

async fn log_request(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let t0 = Instant::now();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let ua = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();

    let count = state.requests_served.fetch_add(1, Ordering::Relaxed) + 1;
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    let elapsed_us = t0.elapsed().as_micros();

    info!(
        req = count,
        method = %method,
        uri = %uri,
        status,
        elapsed_us,
        ua = %ua,
        "HTTP request"
    );
    resp
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/", get(handlers::workbench))
        .route("/health", get(handlers::health))
        .route("/v1/meta", get(handlers::meta))
        .route("/v1/eval", get(handlers::eval_get).post(handlers::eval_post))
        .route("/v1/search", get(handlers::search_get))
        .route("/v1/find", get(handlers::find_get))
        .route("/v1/values", get(handlers::values_get))
        .route("/v1/items/{config_name}", get(handlers::item_get))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .layer(cors)
        .layer(middleware::from_fn_with_state(state.clone(), log_request))
        .with_state(state)
}
