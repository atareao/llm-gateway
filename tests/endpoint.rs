//! Tests de integración del endpoint `POST /v1/chat/completions` (no-streaming).
//!
//! Levanta un backend simulado con axum en un puerto efímero y el gateway con
//! `axum::serve`, también en puerto efímero, y los ejercita con `reqwest`.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use llm_gateway::config::GatewayConfig;
use llm_gateway::proxy::{AppState, router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Backend simulado que devuelve en el cuerpo (echo) exactamente el JSON que
/// recibió, para poder afirmar qué vio el upstream.
///
/// Devuelve la `base_url` (incluye `/v1`) a la que apuntar en la config.
async fn spawn_echo_backend() -> String {
    async fn echo(body: Bytes) -> Response {
        let value: Value = serde_json::from_slice(&body).expect("mock: el cuerpo debe ser JSON");
        axum::Json(value).into_response()
    }

    let app = Router::new().route("/v1/chat/completions", post(echo));
    spawn(app).await
}

/// Backend simulado que responde siempre con un JSON fijo (`canned`).
async fn spawn_canned_backend(canned: Value) -> String {
    async fn reply(State(canned): State<Arc<Value>>) -> Response {
        axum::Json((*canned).clone()).into_response()
    }

    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(Arc::new(canned));
    spawn(app).await
}

/// Backend simulado que responde siempre con un `status` y un JSON fijos.
async fn spawn_status_backend(status: StatusCode, body: Value) -> String {
    async fn reply(State((status, body)): State<(StatusCode, Arc<Value>)>) -> Response {
        (status, axum::Json((*body).clone())).into_response()
    }

    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state((status, Arc::new(body)));
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

/// Levanta el gateway apuntando a `backend_url` como primario de la ruta
/// `mech-writer`, con el modelo real `qwen2.5:7b-instruct`.
async fn spawn_gateway(backend_url: &str) -> String {
    let yaml = format!(
        r#"
route:
  - name: mech-writer
    backend:
      - base_url: "{backend_url}"
        model: "qwen2.5:7b-instruct"
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

/// Scenario: Modelo lógico conocido → se reescribe `model` y se preservan los
/// campos desconocidos.
#[tokio::test]
async fn known_model_is_rewritten_and_unknown_fields_are_preserved() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&backend).await;

    let sent = json!({
        "model": "mech-writer",
        "messages": [{"role": "user", "content": "hola"}],
        "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}],
        "tool_choice": "auto",
        "temperature": 0.3,
        "response_format": {"type": "json_object"},
        "x_custom": {"future": true}
    });

    let received: Value = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&sent)
        .send()
        .await
        .expect("request al gateway")
        .json()
        .await
        .expect("json de respuesta");

    // El `model` lógico se sustituye por el del backend primario.
    assert_eq!(received["model"], json!("qwen2.5:7b-instruct"));

    // El resto de campos viajan intactos, incluido el campo futuro.
    assert_eq!(received["tools"], sent["tools"]);
    assert_eq!(received["tool_choice"], sent["tool_choice"]);
    assert_eq!(received["temperature"], sent["temperature"]);
    assert_eq!(received["response_format"], sent["response_format"]);
    assert_eq!(received["x_custom"], sent["x_custom"]);
    assert_eq!(received["messages"], sent["messages"]);

    // Y no se cuela ningún campo extra.
    let mut expected = sent.clone();
    expected["model"] = json!("qwen2.5:7b-instruct");
    assert_eq!(received, expected);
}

/// Scenario: Modelo desconocido → `404` con `code == "model_not_found"`.
#[tokio::test]
async fn unknown_model_returns_404_model_not_found() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&backend).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": "foo", "messages": []}))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    let body: Value = response.json().await.expect("json de error");
    assert_eq!(body["error"]["code"], json!("model_not_found"));
    assert_eq!(body["error"]["type"], json!("invalid_request_error"));
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("foo")),
        "el mensaje debe mencionar el modelo pedido: {body}"
    );
}

/// `model` ausente → `400` `invalid_request_error` (no `404`).
#[tokio::test]
async fn missing_model_returns_400_invalid_request() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&backend).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"messages": []}))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    let body: Value = response.json().await.expect("json de error");
    assert_eq!(body["error"]["type"], json!("invalid_request_error"));
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| { m.contains("model") })
    );
}

/// `model` no-string → `400` `invalid_request_error`.
#[tokio::test]
async fn non_string_model_returns_400_invalid_request() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&backend).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": 123, "messages": []}))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    let body: Value = response.json().await.expect("json de error");
    assert_eq!(body["error"]["type"], json!("invalid_request_error"));
}

/// Un `4xx` del upstream se propaga tal cual (status y cuerpo), sin failover.
#[tokio::test]
async fn upstream_4xx_is_passed_through_verbatim() {
    let error_body = json!({
        "error": {
            "message": "bad request upstream",
            "type": "invalid_request_error",
            "code": "bad_request"
        }
    });
    let backend = spawn_status_backend(StatusCode::BAD_REQUEST, error_body.clone()).await;
    let gateway = spawn_gateway(&backend).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": "mech-writer", "messages": []}))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    let body: Value = response.json().await.expect("json de respuesta");
    assert_eq!(body, error_body);
}

/// Scenario: Preserva `tool_calls` en la respuesta del backend.
#[tokio::test]
async fn tool_calls_in_response_are_passed_through_intact() {
    let canned = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "qwen2.5:7b-instruct",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "f", "arguments": "{\"x\":1}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}
    });
    let backend = spawn_canned_backend(canned.clone()).await;
    let gateway = spawn_gateway(&backend).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": "mech-writer", "messages": []}))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let body: Value = response.json().await.expect("json de respuesta");
    assert_eq!(body, canned);
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        json!("f")
    );
}
