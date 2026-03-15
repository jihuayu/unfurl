use std::net::SocketAddr;

use tracing_subscriber::EnvFilter;
use unfurl_server::{build_app, config::Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    let app = build_app(config.clone()).await?;
    let address = SocketAddr::new(config.host.parse()?, config.port);
    let listener = tokio::net::TcpListener::bind(address).await?;

    tracing::info!("listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}
