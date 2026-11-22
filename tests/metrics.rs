//! Tests de integración de las métricas Prometheus y de `x-request-id`.
//!
//! Los instrumentos viven en el `MeterProvider` **global** del proceso (fuente
//! única para `/metrics` y la exportación OTLP), así que todos los tests de este
//! binario comparten registro. Para que no interfieran entre sí, cada test usa
//! un **nombre de ruta propio** y busca sus series por etiquetas, sin depender
//! del orden en que el `PrometheusExporter` las emite.

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
    common::spawn(app).await
}

async fn spawn_status_backend(status: StatusCode) -> String {
    async fn reply(State(status): State<StatusCode>) -> Response {
        (status, axum::Json(json!({"error": "mock"}))).into_response()
    }
    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(status);
    common::spawn(app).await
}

/// Levanta el gateway con la ruta `route` y los backends dados.
async fn spawn_gateway(route: &str, backends: &[(&str, &str)]) -> String {
    let mut yaml = format!("route:\n  - name: {route}\n    backend:\n");
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

/// Etiqueta `route="<route>"`, para buscar series de una ruta concreta.
fn route_label(route: &str) -> String {
    format!(r#"route="{route}""#)
}

/// Etiqueta `backend="<base_url>#<model>"`, para distinguir backends de una
/// misma ruta (cada uno pre-registra sus propias series en cero).
fn backend_label(base_url: &str, model: &str) -> String {
    format!(r#"backend="{base_url}#{model}""#)
}

/// Valor de la primera **serie** (línea de muestra, no `# HELP`/`# TYPE`) que
/// contiene *todos* los `needles`; `None` si no existe.
fn series_value<'a>(body: &'a str, needles: &[&str]) -> Option<&'a str> {
    body.lines()
        .find(|line| !line.starts_with('#') && needles.iter().all(|n| line.contains(n)))
        .and_then(|line| line.rsplit(' ').next())
}

/// Valor numérico de [`series_value`].
fn series_number(body: &str, needles: &[&str]) -> Option<f64> {
    series_value(body, needles).and_then(|value| value.parse().ok())
}

#[tokio::test]
async fn metrics_endpoint_exposes_prometheus_text() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway("route-scrape", &[(&backend, "m")]).await;

    let (status, content_type, body) = get_metrics(&gateway).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.starts_with("text/plain; version=0.0.4"),
        "content-type inesperado: {content_type}"
    );
    assert!(body.contains("# TYPE llm_gateway_requests_total counter"));
}

/// El pre-registro del gauge de blacklist no depende de que haya backends
/// dinámicos: con una config solo estática, el primer scrape ya publica el
/// gauge a `0` por la rama de producción de `Metrics::new` (este binario
/// compila la lib sin `cfg(test)`).
#[tokio::test]
async fn discovery_blacklist_gauge_preregistered_without_dynamic_backends() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway("route-preregistro-blacklist", &[(&backend, "m")]).await;

    let (status, content_type, body) = get_metrics(&gateway).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.starts_with("text/plain; version=0.0.4"),
        "content-type inesperado: {content_type}"
    );
    assert_eq!(
        series_value(&body, &["llm_gateway_discovery_blacklist"]),
        Some("0"),
        "el scrape inicial debe publicar llm_gateway_discovery_blacklist 0"
    );
}

/// Sin bloque `observability` no se inicializa ningún exportador OTLP y
/// `/metrics` sigue sirviendo el contrato actual (`text/plain; version=0.0.4` y
/// series `llm_gateway_*`).
///
/// Este test vive aquí, y no en `tests/telemetry.rs`, porque el `TelemetryGuard`
/// de allí hace `shutdown` del `MeterProvider` global al terminar, lo que
/// dejaría el registro Prometheus vacío para los demás tests del mismo binario.
#[tokio::test]
async fn metrics_endpoint_unchanged_without_observability() {
    let route = "route-sin-observabilidad";
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"model": route, "messages": []})).await;
    assert_eq!(response.status(), StatusCode::OK);

    let (status, content_type, body) = get_metrics(&gateway).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.starts_with("text/plain; version=0.0.4"),
        "content-type inesperado: {content_type}"
    );
    assert!(body.contains("# TYPE llm_gateway_requests_total counter"));
    assert_eq!(
        series_value(
            &body,
            &[
                "llm_gateway_requests_total",
                &route_label(route),
                r#"outcome="success""#
            ]
        ),
        Some("1"),
        "contador de peticiones sin observabilidad:\n{body}"
    );
}

#[tokio::test]
async fn success_request_increments_counters() {
    let route = "ok-success";
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"model": route, "messages": []})).await;
    assert_eq!(response.status(), StatusCode::OK);

    let (_, _, body) = get_metrics(&gateway).await;
    let route = route_label(route);
    assert_eq!(
        series_value(
            &body,
            &[
                "llm_gateway_requests_total",
                &route,
                r#"streaming="false""#,
                r#"outcome="success""#
            ]
        ),
        Some("1"),
        "contador de peticiones:\n{body}"
    );
    assert_eq!(
        series_value(&body, &["llm_gateway_failover_total", &route]),
        Some("0"),
        "failover_total:\n{body}"
    );
    assert_eq!(
        series_value(
            &body,
            &[
                "llm_gateway_backend_attempts_total",
                &route,
                r#"result="success""#
            ]
        ),
        Some("1"),
        "intento del backend:\n{body}"
    );
}

#[tokio::test]
async fn failover_increments_failover_and_attempt_results() {
    let route = "ok-failover";
    let fallback = spawn_echo_backend().await;
    let primary = common::dead_backend().await;
    let gateway = spawn_gateway(route, &[(&primary, "primary"), (&fallback, "fallback")]).await;

    let response = post_chat(&gateway, json!({"model": route, "messages": []})).await;
    assert_eq!(response.status(), StatusCode::OK);

    let (_, _, body) = get_metrics(&gateway).await;
    let route = route_label(route);
    let primary = backend_label(&primary, "primary");
    let fallback = backend_label(&fallback, "fallback");
    assert_eq!(
        series_value(&body, &["llm_gateway_failover_total", &route]),
        Some("1"),
        "failover_total:\n{body}"
    );
    assert_eq!(
        series_value(
            &body,
            &[
                "llm_gateway_backend_attempts_total",
                &primary,
                r#"result="transport_error""#
            ]
        ),
        Some("1"),
        "error de transporte del primario:\n{body}"
    );
    assert_eq!(
        series_value(
            &body,
            &[
                "llm_gateway_backend_attempts_total",
                &fallback,
                r#"result="success""#
            ]
        ),
        Some("1"),
        "éxito del fallback:\n{body}"
    );
}

#[tokio::test]
async fn retryable_status_is_recorded() {
    let route = "ok-retryable";
    let primary = spawn_status_backend(StatusCode::SERVICE_UNAVAILABLE).await;
    let fallback = spawn_echo_backend().await;
    let gateway = spawn_gateway(route, &[(&primary, "primary"), (&fallback, "fallback")]).await;

    let response = post_chat(&gateway, json!({"model": route, "messages": []})).await;
    assert_eq!(response.status(), StatusCode::OK);

    let (_, _, body) = get_metrics(&gateway).await;
    let primary_label = backend_label(&primary, "primary");
    assert_eq!(
        series_value(
            &body,
            &[
                "llm_gateway_backend_attempts_total",
                &primary_label,
                r#"result="retryable_status""#
            ]
        ),
        Some("1"),
        "status reintentable:\n{body}"
    );
}

#[tokio::test]
async fn all_backends_failed_is_recorded() {
    let route = "ok-all-failed";
    let primary = common::dead_backend().await;
    let gateway = spawn_gateway(route, &[(&primary, "primary")]).await;

    let response = post_chat(&gateway, json!({"model": route, "messages": []})).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let (_, _, body) = get_metrics(&gateway).await;
    let route = route_label(route);
    assert_eq!(
        series_value(
            &body,
            &[
                "llm_gateway_requests_total",
                &route,
                r#"outcome="all_failed""#
            ]
        ),
        Some("1"),
        "outcome all_failed:\n{body}"
    );
}

#[tokio::test]
async fn unknown_model_is_recorded() {
    let route = "ok-unknown-model";
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"model": "no-existe", "messages": []})).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let (_, _, body) = get_metrics(&gateway).await;
    // `llm_gateway_errors_total` es global (sin etiqueta de ruta): otros tests de
    // este binario también pueden incrementar el mismo `kind`.
    assert!(
        series_number(
            &body,
            &["llm_gateway_errors_total", r#"kind="model_not_found""#]
        )
        .is_some_and(|value| value >= 1.0),
        "errors_total:\n{body}"
    );
}

#[tokio::test]
async fn request_id_is_echoed_when_valid() {
    let route = "req-id";
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .header("x-request-id", "cliente-42")
        .json(&json!({"model": route, "messages": []}))
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
    let route = "req-id-gen";
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"model": route, "messages": []})).await;
    let id = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-request-id presente");

    assert!(id.starts_with("req-"), "id generado inesperado: {id}");
}

#[tokio::test]
async fn request_id_is_replaced_when_too_long() {
    let route = "req-id-long";
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;
    let long = "a".repeat(200);

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .header("x-request-id", &long)
        .json(&json!({"model": route, "messages": []}))
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
    common::spawn(app).await
}

/// Levanta el gateway con la ruta `route` y `(base_url, model, api_key_env)`
/// por backend.
async fn spawn_gateway_with_env(route: &str, backends: &[(&str, &str, Option<&str>)]) -> String {
    let mut yaml = format!("route:\n  - name: {route}\n    backend:\n");
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
    let route = "ok-invalid";
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"messages": []})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response.headers().get("x-request-id").is_some(),
        "falta x-request-id en el 400"
    );

    let (_, _, body) = get_metrics(&gateway).await;
    // Serie global compartida con otros tests del binario: basta con ≥ 1.
    assert!(
        series_number(
            &body,
            &["llm_gateway_errors_total", r#"kind="invalid_request""#]
        )
        .is_some_and(|value| value >= 1.0),
        "errors_total:\n{body}"
    );
}

#[tokio::test]
async fn malformed_json_gets_request_id_and_metric() {
    let route = "ok-malformed";
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;

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
        series_number(
            &body,
            &["llm_gateway_errors_total", r#"kind="invalid_request""#]
        )
        .is_some_and(|value| value >= 1.0),
        "errors_total:\n{body}"
    );
}

#[tokio::test]
async fn missing_api_key_is_recorded_without_failover() {
    let route = "ok-missing-key";
    let fallback = spawn_echo_backend().await;
    let missing_var = format!("GATEWAY_TEST_MISSING_{}", std::process::id());
    let gateway = spawn_gateway_with_env(
        route,
        &[
            (
                "http://127.0.0.1:1/v1",
                "primary",
                Some(missing_var.as_str()),
            ),
            (&fallback, "fallback", None),
        ],
    )
    .await;

    let response = post_chat(&gateway, json!({"model": route, "messages": []})).await;
    assert_eq!(response.status(), StatusCode::OK);

    let (_, _, body) = get_metrics(&gateway).await;
    let label = route_label(route);
    let primary = backend_label("http://127.0.0.1:1/v1", "primary");
    assert_eq!(
        series_value(
            &body,
            &[
                "llm_gateway_backend_attempts_total",
                &primary,
                r#"result="missing_api_key""#
            ]
        ),
        Some("1"),
        "missing_api_key:\n{body}"
    );
    assert_eq!(
        series_value(&body, &["llm_gateway_failover_total", &label]),
        Some("0"),
        "un backend saltado no es un intento:\n{body}"
    );
}

#[tokio::test]
async fn request_id_present_on_error_response() {
    let route = "req-id-error";
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;

    let response = post_chat(&gateway, json!({"model": "no-existe", "messages": []})).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response.headers().get("x-request-id").is_some(),
        "falta x-request-id en el 404"
    );
}

#[tokio::test]
async fn streaming_request_is_recorded() {
    let route = "ok-streaming";
    let backend = spawn_sse_backend().await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;

    let response = post_chat(
        &gateway,
        json!({"model": route, "messages": [], "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = response.text().await.expect("cuerpo SSE");

    let (_, _, body) = get_metrics(&gateway).await;
    let route = route_label(route);
    assert_eq!(
        series_value(
            &body,
            &[
                "llm_gateway_requests_total",
                &route,
                r#"streaming="true""#,
                r#"outcome="success""#
            ]
        ),
        Some("1"),
        "streaming:\n{body}"
    );
}

/// Scenario: una respuesta `200` sin `tool_calls` cuando la petición las exige
/// registra un intento `no_tool_call`.
#[tokio::test]
async fn no_tool_call_attempt_is_recorded() {
    let route = "ok-no-tool-call";
    let (backend, _hits) = common::spawn_json_backend(common::text_body()).await;
    let gateway = spawn_gateway(route, &[(&backend, "m")]).await;

    let response = post_chat(
        &gateway,
        json!({
            "model": route,
            "messages": [],
            "tools": [{"type": "function", "function": {"name": "get_weather"}}],
            "tool_choice": "required"
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let (_, _, body) = get_metrics(&gateway).await;
    assert_eq!(
        series_value(
            &body,
            &[
                "llm_gateway_backend_attempts_total",
                &route_label(route),
                r#"result="no_tool_call""#
            ]
        ),
        Some("1"),
        "intento sin tool_calls:\n{body}"
    );
}
