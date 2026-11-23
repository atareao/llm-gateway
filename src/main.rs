use std::net::SocketAddr;

use anyhow::{Context, Result};
use llm_gateway_rs::proxy::{AppState, router};

/// Dirección de escucha por defecto.
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> Result<()> {
    // La configuración se lee antes de instalar el subscriber: si falla, el
    // error se propaga igualmente (exit code ≠ 0) sin necesidad de logging.
    let config = llm_gateway_rs::config::load_default()?;

    // `telemetry::init` instala el subscriber de `tracing` (texto o JSON según
    // `GATEWAY_LOG_FORMAT`) más los layers OTLP que pida la config, y deja los
    // `MeterProvider`/`TracerProvider`/`LoggerProvider` globales listos. Debe
    // ir ANTES de construir `AppState`, cuyos instrumentos los toman del
    // `MeterProvider` global.
    let telemetry =
        llm_gateway_rs::telemetry::init(&config).context("no se pudo inicializar la telemetría")?;

    tracing::info!(routes = config.route.len(), "configuración cargada");

    let addr: SocketAddr = std::env::var("GATEWAY_LISTEN")
        .unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_owned())
        .parse()
        .context("GATEWAY_LISTEN no es una dirección válida (host:puerto)")?;

    let state = AppState::new(config).context("no se pudo construir el cliente HTTP")?;
    let app = router(state.clone());

    // Se conservan los handles de keep-alive para abortarlos en el cierre.
    let warmup_handles = llm_gateway_rs::warmup::spawn_warmup(state.clone());
    // Refresco en segundo plano del catálogo dinámico (patrón de `warmup`).
    let discovery_handles = state.discovery.spawn_refresh();
    // Sondeo proactivo de la cuota diaria free (patrón de `spawn_refresh`).
    let quota_handles = state.discovery.spawn_quota_poll();

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("no se pudo escuchar en {addr}"))?;

    tracing::info!(%addr, "gateway escuchando");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("error en el servidor HTTP")?;

    // Apagado ordenado: primero se detiene el keep-alive para que ningún warmup
    // en vuelo registre señales contra instrumentos ya cerrados, y después se
    // vacían las señales OTLP pendientes. Aunque este flush no llegase a
    // ejecutarse, `TelemetryGuard` lo repite al dropearse.
    for handle in &warmup_handles {
        handle.abort();
    }
    for handle in &discovery_handles {
        handle.abort();
    }
    for handle in &quota_handles {
        handle.abort();
    }
    telemetry.flush();
    tracing::info!(
        tasks = warmup_handles.len() + discovery_handles.len() + quota_handles.len(),
        "cierre ordenado completado"
    );

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
