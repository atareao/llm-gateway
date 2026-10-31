use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // El arranque del servidor HTTP llega en el lote B. Por ahora solo se
    // valida la configuración.
    let config = llm_gateway::config::load_default()?;
    tracing::info!(routes = config.route.len(), "configuración cargada");

    Ok(())
}
