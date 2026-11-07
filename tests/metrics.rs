//! Tests de integración de las métricas Prometheus y de `x-request-id`.

mod common;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use llm_gateway_rs::config::GatewayConfig;
use llm_gateway_rs::proxy::{AppState, router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

async fn spawn_echo_backend() -> String {
    async fn echo(body: Bytes) -> Response {
        let value: Value = serde_json::from_slice(&body).expect("mock: cuerpo JSON");
        axum::Json(value).into_response()
    }
    let app = Router::new().route("/v1/chat/completions", post(echo));
    spawn(app).await
}

async fn spawn_status_backend(status: StatusCode) -> String {
    async fn reply(State(status): State<StatusCode>) -> Response {
        (status, axum::Json(json!({"error": "mock"}))).into_response()
    }
    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(status);
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

/// Levanta el gateway con la ruta `mech-writer` y los backends dados.
async fn spawn_gateway(backends: &[(&str, &str)]) -> String {
    let mut yaml = String::from("route:\n  - name: mech-writer\n    backend:\n");
    for (base_url, model) in backends {
        yaml.push_str(&format!(
            "      - base_url: \"{base_url}\"\n        model: \"{model}\"\n"
        ));
    }
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

async fn get_metrics(gateway: &str) -> (StatusCode, String, String) {
    let response = reqwest::get(format!("{gateway}/metrics"))
        .await
        .expect("GET /metrics");
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = response.text().await.expect("cuerpo de /metrics");
    (status, content_type, body)
}

async fn post_chat(gateway: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("request al gateway")
}

#[tokio::test]
async fn metrics_endpoint_exposes_prometheus_text() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "m")]).await;

    let (status, content_type, body) = get_metrics(&gateway).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.starts_with("text/plain; version=0.0.4"),
        "content-type inesperado: {content_type}"
    );
    assert!(body.contains("# TYPE llm_gateway_requests_total counter"));
}

#[tokio::test]
async fn success_request_increments_counters() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;
    assert_eq!(response.status(), StatusCode::OK);

    let (_, _, body) = get_metrics(&gateway).await;
    assert!(
        body.contains(
            r#"llm_gateway_requests_total{route="mech-writer",streaming="false",outcome="success"} 1"#
        ),
        "contador de peticiones:\n{body}"
    );
    assert!(
        body.contains(r#"llm_gateway_failover_total{route="mech-writer"} 0"#),
        "failover_total:\n{body}"
    );
    assert!(
        body.contains(r#"result="success"} 1"#),
        "intento del backend:\n{body}"
    );
}

#[tokio::test]
async fn failover_increments_failover_and_attempt_results() {
    let fallback = spawn_echo_backend().await;
    let primary = common::dead_backend().await;
    let gateway = spawn_gateway(&[(&primary, "primary"), (&fallback, "fallback")]).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;
    assert_eq!(response.status(), StatusCode::OK);

    let (_, _, body) = get_metrics(&gateway).await;
    assert!(
        body.contains(r#"llm_gateway_failover_total{route="mech-writer"} 1"#),
        "failover_total:\n{body}"
    );
    assert!(
        body.contains(r#"result="transport_error"} 1"#),
        "error de transporte del primario:\n{body}"
    );
    assert!(
        body.contains(r#"result="success"} 1"#),
        "éxito del fallback:\n{body}"
    );
}

#[tokio::test]
async fn retryable_status_is_recorded() {
    let primary = spawn_status_backend(StatusCode::SERVICE_UNAVAILABLE).await;
    let fallback = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&primary, "primary"), (&fallback, "fallback")]).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;
    assert_eq!(response.status(), StatusCode::OK);

    let (_, _, body) = get_metrics(&gateway).await;
    assert!(
        body.contains(r#"result="retryable_status"} 1"#),
        "status reintentable:\n{body}"
    );
}

#[tokio::test]
async fn all_backends_failed_is_recorded() {
    let primary = common::dead_backend().await;
    let gateway = spawn_gateway(&[(&primary, "primary")]).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let (_, _, body) = get_metrics(&gateway).await;
    assert!(
        body.contains(r#"outcome="all_failed"} 1"#),
        "outcome all_failed:\n{body}"
    );
}

#[tokio::test]
async fn unknown_model_is_recorded() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"model": "no-existe", "messages": []})).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let (_, _, body) = get_metrics(&gateway).await;
    assert!(
        body.contains(r#"llm_gateway_errors_total{kind="model_not_found"} 1"#),
        "errors_total:\n{body}"
    );
}

#[tokio::test]
async fn request_id_is_echoed_when_valid() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "m")]).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .header("x-request-id", "cliente-42")
        .json(&json!({"model": "mech-writer", "messages": []}))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("cliente-42")
    );
}

#[tokio::test]
async fn request_id_is_generated_when_missing() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;
    let id = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-request-id presente");

    assert!(id.starts_with("req-"), "id generado inesperado: {id}");
}

#[tokio::test]
async fn request_id_is_replaced_when_too_long() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "m")]).await;
    let long = "a".repeat(200);

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .header("x-request-id", &long)
        .json(&json!({"model": "mech-writer", "messages": []}))
        .send()
        .await
        .expect("request al gateway");

    let id = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-request-id presente");

    assert_ne!(id, long);
    assert!(id.starts_with("req-"), "id generado inesperado: {id}");
}

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
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(futures_util::stream::iter(
                chunks,
            )))
            .expect("respuesta SSE")
    }
    let app = Router::new().route("/v1/chat/completions", post(sse));
    spawn(app).await
}

/// Levanta el gateway con `(base_url, model, api_key_env)` por backend.
async fn spawn_gateway_with_env(backends: &[(&str, &str, Option<&str>)]) -> String {
    let mut yaml = String::from("route:\n  - name: mech-writer\n    backend:\n");
    for (base_url, model, api_key_env) in backends {
        yaml.push_str(&format!(
            "      - base_url: \"{base_url}\"\n        model: \"{model}\"\n"
        ));
        if let Some(var) = api_key_env {
            yaml.push_str(&format!("        api_key_env: \"{var}\"\n"));
        }
    }
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

#[tokio::test]
async fn invalid_request_is_recorded() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"messages": []})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response.headers().get("x-request-id").is_some(),
        "falta x-request-id en el 400"
    );

    let (_, _, body) = get_metrics(&gateway).await;
    assert!(
        body.contains(r#"llm_gateway_errors_total{kind="invalid_request"} 1"#),
        "errors_total:\n{body}"
    );
}

#[tokio::test]
async fn malformed_json_gets_request_id_and_metric() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "m")]).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body("{no-es-json")
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response.headers().get("x-request-id").is_some(),
        "falta x-request-id en el rechazo del extractor"
    );

    let (_, _, body) = get_metrics(&gateway).await;
    assert!(
        body.contains(r#"llm_gateway_errors_total{kind="invalid_request"} 1"#),
        "errors_total:\n{body}"
    );
}

#[tokio::test]
async fn missing_api_key_is_recorded_without_failover() {
    let fallback = spawn_echo_backend().await;
    let missing_var = format!("GATEWAY_TEST_MISSING_{}", std::process::id());
    let gateway = spawn_gateway_with_env(&[
        (
            "http://127.0.0.1:1/v1",
            "primary",
            Some(missing_var.as_str()),
        ),
        (&fallback, "fallback", None),
    ])
    .await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;
    assert_eq!(response.status(), StatusCode::OK);

    let (_, _, body) = get_metrics(&gateway).await;
    assert!(
        body.contains(r#"result="missing_api_key"} 1"#),
        "missing_api_key:\n{body}"
    );
    assert!(
        body.contains(r#"llm_gateway_failover_total{route="mech-writer"} 0"#),
        "un backend saltado no es un intento:\n{body}"
    );
}

#[tokio::test]
async fn request_id_present_on_error_response() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"model": "no-existe", "messages": []})).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response.headers().get("x-request-id").is_some(),
        "falta x-request-id en el 404"
    );
}

#[tokio::test]
async fn streaming_request_is_recorded() {
    let backend = spawn_sse_backend().await;
    let gateway = spawn_gateway(&[(&backend, "m")]).await;

    let response = post_chat(
        &gateway,
        json!({"model": "mech-writer", "messages": [], "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.text().await.expect("cuerpo SSE");

    let (_, _, body) = get_metrics(&gateway).await;
    assert!(
        body.contains(
            r#"llm_gateway_requests_total{route="mech-writer",streaming="true",outcome="success"} 1"#
        ),
        "streaming:\n{body}"
    );
}
