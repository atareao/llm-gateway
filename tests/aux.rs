//! Tests de integración de los endpoints auxiliares (Requisito 6):
//! `GET /v1/models` y `GET /healthz`.

use llm_gateway::config::GatewayConfig;
use llm_gateway::proxy::{AppState, router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Levanta el gateway con dos rutas lógicas (`mech-writer`, `code-helper`).
async fn spawn_gateway() -> String {
    let yaml = r#"
route:
  - name: mech-writer
    backend:
      - base_url: "http://127.0.0.1:1/v1"
        model: "qwen2.5:7b-instruct"
  - name: code-helper
    backend:
      - base_url: "http://127.0.0.1:1/v1"
        model: "qwen/qwen3-8b"
"#;
    let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");
    let state = AppState::new(config).expect("construir AppState");
    let app = router(state);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve gateway");
    });
    format!("http://{addr}")
}

/// Scenario: `GET /v1/models` lista los nombres lógicos de ruta.
#[tokio::test]
async fn models_lists_logical_route_names() {
    let gateway = spawn_gateway().await;

    let response = reqwest::Client::new()
        .get(format!("{gateway}/v1/models"))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let body: Value = response.json().await.expect("json de respuesta");
    assert_eq!(body["object"], json!("list"));
    let data = body["data"].as_array().expect("data es un array");
    assert_eq!(data.len(), 2);

    let ids: Vec<&str> = data.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(ids.contains(&"mech-writer"), "ids: {ids:?}");
    assert!(ids.contains(&"code-helper"), "ids: {ids:?}");
    assert!(data.iter().all(|m| m["object"] == json!("model")));
}

/// Scenario: `GET /healthz` → `200` con `ok`.
#[tokio::test]
async fn healthz_returns_ok() {
    let gateway = spawn_gateway().await;

    let response = reqwest::Client::new()
        .get(format!("{gateway}/healthz"))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(response.text().await.expect("cuerpo"), "ok");
}
