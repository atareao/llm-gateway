use std::net::SocketAddr;

use anyhow::{Context, Result};
use llm_gateway::proxy::{AppState, router};

/// Dirección de escucha por defecto.
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = llm_gateway::config::load_default()?;
    tracing::info!(routes = config.route.len(), "configuración cargada");

    let addr: SocketAddr = std::env::var("GATEWAY_LISTEN")
        .unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_owned())
        .parse()
        .context("GATEWAY_LISTEN no es una dirección válida (host:puerto)")?;

    let app = router(AppState::new(config));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("no se pudo escuchar en {addr}"))?;

    tracing::info!(%addr, "gateway escuchando");
    axum::serve(listener, app)
        .await
        .context("error en el servidor HTTP")?;

    Ok(())
}
