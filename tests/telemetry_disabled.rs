//! Test de integración del camino *desactivado* de la telemetría OTLP.
//!
//! Los providers OTel globales y el subscriber de `tracing` solo pueden
//! instalarse **una vez por proceso**: por eso este caso vive en su propio
//! binario de test (Cargo compila cada fichero de `tests/` como un proceso
//! distinto) y no compite con `tests/telemetry.rs`.
//!
//! Comprueba el escenario "señales desactivadas no exportan": con
//! `metrics: false` y `logs: false`, el receptor OTLP no debe recibir ninguna
//! exportación de esas señales, mientras que `traces: true` sí exporta (lo que
//! además demuestra que el receptor es funcional y que no es un falso negativo).

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use llm_gateway_rs::config::GatewayConfig;
use llm_gateway_rs::proxy::{AppState, router};
use llm_gateway_rs::telemetry;
use serde_json::json;
use tokio::net::TcpListener;

/// Capturas del receptor OTLP, por señal (`metrics`/`logs`/`traces`).
type Captured = Arc<Mutex<BTreeMap<String, usize>>>;

#[derive(Clone)]
struct Receiver {
    captured: Captured,
}

/// Levanta una app axum en un puerto efímero y devuelve su **origen**
/// (`http://host:puerto`, sin `/v1`), que es lo que necesitan el receptor OTLP
/// (endpoint base) y el propio gateway (para construir `/v1/...` y `/metrics`).
async fn spawn_root(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    format!("http://{addr}")
}

/// Levanta el receptor OTLP mock y devuelve su URL base y el contador de
/// exportaciones por señal.
async fn spawn_otlp_receiver() -> (String, Captured) {
    async fn record(
        State(receiver): State<Receiver>,
        Path((_org, signal)): Path<(String, String)>,
        _headers: HeaderMap,
        _body: Bytes,
    ) -> Response {
        *receiver
            .captured
            .lock()
            .expect("lock de capturas")
            .entry(signal)
            .or_default() += 1;
        StatusCode::OK.into_response()
    }

    let captured: Captured = Arc::new(Mutex::new(BTreeMap::new()));
    let app = Router::new()
        .route("/api/{org}/v1/{signal}", post(record))
        .with_state(Receiver {
            captured: Arc::clone(&captured),
        });

    (spawn_root(app).await, captured)
}

/// Levanta un backend OpenAI-compatible que responde `200` a cualquier POST y
/// devuelve su `base_url` (`http://host:puerto/v1`).
async fn spawn_backend() -> String {
    async fn chat() -> Response {
        axum::Json(json!({
            "id": "chatcmpl-disabled",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hola"},
                "finish_reason": "stop"
            }]
        }))
        .into_response()
    }
    common::spawn(Router::new().route("/v1/chat/completions", post(chat))).await
}

/// Sirve el gateway en un puerto efímero y devuelve su URL base (sin `/v1`).
async fn serve_gateway(config: GatewayConfig) -> String {
    let state = AppState::new(config).expect("construir AppState");
    spawn_root(router(state)).await
}

/// Métricas y logs desactivados: no deben exportarse por OTLP; las trazas sí.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_signals_are_not_exported() {
    let (receiver_url, captured) = spawn_otlp_receiver().await;
    let backend_url = spawn_backend().await;

    let yaml = format!(
        "route:\n  - name: disabled-route\n    backend:\n      - base_url: \"{backend_url}\"\n        model: \"m\"\n\
         observability:\n  otlp:\n    endpoint: \"{receiver_url}\"\n    organization: \"default\"\n    protocol: http\n\
         \x20   interval_ms: 200\n    signals:\n      metrics: false\n      logs: false\n      traces: true\n"
    );
    let config: GatewayConfig = serde_saphyr::from_str(&yaml).expect("config de test válida");
    assert_eq!(
        telemetry::planned_exporters(&config),
        vec!["traces"],
        "solo las trazas deben planificarse"
    );

    let guard = telemetry::init(&config).expect("init de telemetría");

    let gateway = serve_gateway(config).await;
    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": "disabled-route", "messages": []}))
        .send()
        .await
        .expect("request al gateway");
    assert_eq!(response.status(), StatusCode::OK);

    // Margen de sobra para varios intervalos de exportación de métricas: si el
    // `PeriodicReader` OTLP estuviese activo, ya habría exportado.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let counts = captured.lock().expect("lock de capturas").clone();
    assert_eq!(
        counts.get("metrics"),
        None,
        "métricas desactivadas no deben exportarse: {counts:?}"
    );
    assert_eq!(
        counts.get("logs"),
        None,
        "logs desactivados no deben exportarse: {counts:?}"
    );

    // Con las trazas activas el receptor sí recibe exportaciones: prueba de que
    // el receptor funciona y de que lo anterior no es un falso negativo.
    guard.flush();
    let counts = captured.lock().expect("lock de capturas").clone();
    assert!(
        counts.get("traces").copied().unwrap_or_default() > 0,
        "las trazas activas deben exportarse: {counts:?}"
    );
    assert_eq!(
        counts.get("metrics"),
        None,
        "sigue sin métricas: {counts:?}"
    );
    assert_eq!(counts.get("logs"), None, "sigue sin logs: {counts:?}");
}
