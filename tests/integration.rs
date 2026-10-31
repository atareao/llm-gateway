//! Test de integración end-to-end (Requisito 16): una config con 2 rutas y
//! backends mock, ejercitando `/v1/models`, failover y stream sobre el
//! `router(state)` real servido en un puerto efímero.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use llm_gateway::config::GatewayConfig;
use llm_gateway::proxy::{AppState, router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

async fn spawn_sse_backend() -> String {
    async fn sse() -> Response {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Hola\"}}]}\n\n",
            )),
            Ok(Bytes::from_static(b"data: [DONE]\n\n")),
        ];
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .expect("respuesta SSE")
    }
    let app = Router::new().route("/v1/chat/completions", post(sse));
    spawn(app).await
}

async fn spawn_echo_backend() -> String {
    async fn echo(body: Bytes) -> Response {
        let value: Value = serde_json::from_slice(&body).expect("mock: cuerpo JSON");
        axum::Json(value).into_response()
    }
    let app = Router::new().route("/v1/chat/completions", post(echo));
    spawn(app).await
}

async fn spawn(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    format!("http://{addr}/v1")
}

async fn dead_backend() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    format!("http://{addr}/v1")
}

/// Levanta el gateway real con dos rutas: `mech-writer` (primario caído +
/// fallback OK) y `code-helper` (backend SSE).
async fn spawn_gateway(fallback: &str, sse: &str) -> String {
    let primary = dead_backend().await;
    let yaml = format!(
        r#"
route:
  - name: mech-writer
    backend:
      - base_url: "{primary}"
        model: "qwen2.5:7b-instruct"
      - base_url: "{fallback}"
        model: "qwen/qwen3-8b"
  - name: code-helper
    backend:
      - base_url: "{sse}"
        model: "qwen2.5-coder:7b-instruct"
"#
    );
    let config: GatewayConfig = serde_saphyr::from_str(&yaml).expect("config de test válida");
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

/// End-to-end: modelos, failover y stream sobre la misma instancia.
#[tokio::test]
async fn end_to_end_models_failover_and_stream() {
    let fallback = spawn_echo_backend().await;
    let sse = spawn_sse_backend().await;
    let gateway = spawn_gateway(&fallback, &sse).await;
    let client = reqwest::Client::new();

    // 1) /v1/models lista ambas rutas.
    let models: Value = client
        .get(format!("{gateway}/v1/models"))
        .send()
        .await
        .expect("models")
        .json()
        .await
        .expect("json");
    let ids: Vec<&str> = models["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(ids.contains(&"mech-writer"), "ids: {ids:?}");
    assert!(ids.contains(&"code-helper"), "ids: {ids:?}");

    // 2) Failover: el primario está caído → responde el fallback.
    let failover = client
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": "mech-writer", "messages": []}))
        .send()
        .await
        .expect("failover");
    assert_eq!(failover.status(), reqwest::StatusCode::OK);
    assert_eq!(
        failover
            .headers()
            .get("x-gateway-backend")
            .and_then(|v| v.to_str().ok()),
        Some(format!("{fallback}#qwen/qwen3-8b").as_str())
    );
    let failover_body: Value = failover.json().await.expect("json");
    assert_eq!(failover_body["model"], json!("qwen/qwen3-8b"));

    // 3) Stream: la ruta code-helper emite SSE.
    let streamed = client
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": "code-helper", "stream": true, "messages": []}))
        .send()
        .await
        .expect("stream");
    assert_eq!(streamed.status(), reqwest::StatusCode::OK);
    assert_eq!(
        streamed
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let stream_body = streamed.text().await.expect("cuerpo del stream");
    assert!(
        stream_body.contains("data: [DONE]"),
        "stream: {stream_body}"
    );
}
