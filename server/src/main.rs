use std::net::SocketAddr;

use unfurl_server::{build_app, config::Config, telemetry};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let telemetry = telemetry::init()?;

    let config = Config::from_env()?;
    let app = build_app(config.clone()).await?;
    let address = SocketAddr::new(config.host.parse()?, config.port);
    let listener = tokio::net::TcpListener::bind(address).await?;

    tracing::info!("listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    telemetry.shutdown();
    Ok(())
}
