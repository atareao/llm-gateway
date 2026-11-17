//! Instalación idempotente del `MeterProvider` global.
//!
//! Vive en su propio binario (Cargo compila cada fichero de `tests/` como un
//! proceso aparte) porque llama a `telemetry::init` **después** de
//! `ensure_meter_provider`, y en `tests/telemetry.rs` eso cambiaría el provider
//! global del camino feliz (que necesita un reader OTLP de métricas).

mod common;

use axum::Router;
use axum::http::StatusCode;
use llm_gateway_rs::config::GatewayConfig;
use llm_gateway_rs::proxy::{AppState, router};
use llm_gateway_rs::telemetry;
use tokio::net::TcpListener;

/// Levanta una app axum en un puerto efímero y devuelve su origen
/// (`http://host:puerto`, sin `/v1`), que es lo que necesita `GET /metrics`.
async fn spawn_root(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve gateway");
    });
    format!("http://{addr}")
}

/// `init` después de `ensure_meter_provider` (o dos veces) no debe fallar por
/// registrar dos veces el `PrometheusExporter` sobre el mismo registro.
#[tokio::test]
async fn init_after_ensure_meter_provider_is_idempotent() {
    // Un consumidor de la librería que use `Metrics` antes de `init` instala
    // primero un provider solo-Prometheus: el `already registered` no puede
    // tumbar el arranque.
    telemetry::ensure_meter_provider();

    let yaml = "route:\n  - name: meter-route\n    backend:\n      - base_url: \"http://127.0.0.1:9/v1\"\n        model: \"m\"\n\
         observability:\n  otlp:\n    endpoint: \"http://127.0.0.1:5080\"\n    organization: \"default\"\n    signals:\n\
         \x20     metrics: true\n      logs: false\n      traces: true\n";
    let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");
    assert_eq!(
        telemetry::planned_exporters(&config),
        vec!["metrics", "traces"],
        "la config del test debe pedir métricas y trazas"
    );

    let guard = telemetry::init(&config).expect("init tras ensure_meter_provider no debe fallar");
    // Un segundo `init` tampoco falla: la instalación es idempotente.
    let second = telemetry::init(&config).expect("segundo init no debe fallar");
    assert!(guard.exports_otlp(), "las trazas OTLP deben quedar activas");
    assert!(second.exports_otlp());

    // `/metrics` sigue operativo: el registro Prometheus no quedó vacío ni
    // duplicado.
    let state = AppState::new(config).expect("construir AppState");
    let gateway = spawn_root(router(state)).await;
    let response = reqwest::get(format!("{gateway}/metrics"))
        .await
        .expect("GET /metrics");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.expect("cuerpo de /metrics");
    assert!(
        body.contains("# TYPE llm_gateway_requests_total counter"),
        "cuerpo de /metrics inesperado:\n{body}"
    );
}
