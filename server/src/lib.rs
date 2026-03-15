pub mod cache;
pub mod config;
pub mod error;
pub mod extract;
pub mod fetcher;
pub mod image_proxy;
pub mod models;
pub mod routes;
pub mod state;
pub mod telemetry;
pub mod utils;

use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use cache::build_cache;
use config::Config;
use error::AppError;
use state::AppState;

pub async fn build_app(config: Config) -> Result<Router, AppError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .brotli(true)
        .gzip(true)
        .deflate(true)
        .build()
        .map_err(|error| {
            AppError::internal_with_message(format!("failed to build http client: {error}"))
        })?;

    build_app_with_client(config, client).await
}

pub async fn build_app_with_client(
    config: Config,
    client: reqwest::Client,
) -> Result<Router, AppError> {
    let cache = build_cache(&config).await?;
    Ok(router_with_state(AppState {
        config,
        client,
        cache,
    }))
}

pub fn router_with_state(state: AppState) -> Router {
    routes::router()
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::HEAD,
                    axum::http::Method::OPTIONS,
                ])
                .allow_headers(Any),
        )
        .layer(TraceLayer::new_for_http())
}
