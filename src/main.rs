use std::net::SocketAddr;

use anyhow::{Context, Result};
use llm_gateway::proxy::{AppState, router};

/// Dirección de escucha por defecto.
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let json = std::env::var("GATEWAY_LOG_FORMAT")
        .map(|value| value.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if json {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    let config = llm_gateway::config::load_default()?;
    tracing::info!(routes = config.route.len(), "configuración cargada");

    let addr: SocketAddr = std::env::var("GATEWAY_LISTEN")
        .unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_owned())
        .parse()
        .context("GATEWAY_LISTEN no es una dirección válida (host:puerto)")?;

    let state = AppState::new(config).context("no se pudo construir el cliente HTTP")?;
    let app = router(state.clone());

    // Se conservan los handles de keep-alive para abortarlos en el cierre.
    let warmup_handles = llm_gateway::warmup::spawn_warmup(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("no se pudo escuchar en {addr}"))?;

    tracing::info!(%addr, "gateway escuchando");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("error en el servidor HTTP")?;

    // Apagado ordenado: no dejar tareas de keep-alive activas durante el cierre.
    for handle in &warmup_handles {
        handle.abort();
    }
    tracing::info!(tasks = warmup_handles.len(), "cierre ordenado completado");

    Ok(())
}

/// Resuelve al recibir una señal de terminación: SIGINT (Ctrl-C) o SIGTERM.
///
/// Si no se puede instalar un manejador de señales, se registra un `warn` y esa
/// rama queda inerte en lugar de abortar el arranque.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %err, "no se pudo esperar SIGINT");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "no se pudo instalar el manejador de SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
