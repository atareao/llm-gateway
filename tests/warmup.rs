//! Tests de integración del warmup de backends (keep-alive, Requisito 1).
//!
//! Fase RED (TDD): este fichero referencia `llm_gateway::warmup`
//! (`warmup_once` / `spawn_warmup`), que todavía **no** existe. Es esperable que
//! falle en compilación hasta GREEN.
//!
//! Patrón: mock axum en puerto efímero que **cuenta y captura** cada petición
//! (método, ruta, cabeceras y cuerpo JSON), siguiendo `tests/stream.rs` y
//! `tests/metrics.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use llm_gateway::config::GatewayConfig;
use llm_gateway::proxy::{AppState, backend_id, router};
use llm_gateway::warmup::{spawn_warmup, warmup_once};
use serde_json::{Value, json};
use tokio::net::TcpListener;

mod common;

/// Petición observada por el mock de backend.
#[derive(Clone, Debug)]
struct Captured {
    /// Método HTTP (p. ej. `POST`).
    method: String,
    /// Ruta observada (p. ej. `/v1/chat/completions`).
    path: String,
    /// Cabecera `Authorization` si vino.
    authorization: Option<String>,
    /// Cuerpo JSON parseado (o `Null` si no era JSON).
    body: Value,
}

/// Mock OpenAI-compatible que responde `200` y **captura** las peticiones.
///
/// Devuelve la `base_url` (incluye `/v1`) y el registro compartido de peticiones.
async fn spawn_capturing_backend() -> (String, Arc<Mutex<Vec<Captured>>>) {
    async fn capture(
        State(requests): State<Arc<Mutex<Vec<Captured>>>>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let authorization = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        requests.lock().expect("lock del mock").push(Captured {
            method: method.to_string(),
            path: uri.path().to_owned(),
            authorization,
            body: parsed,
        });
        (StatusCode::OK, axum::Json(json!({"ok": true}))).into_response()
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/v1/chat/completions", post(capture))
        .with_state(Arc::clone(&requests));
    (spawn(app).await, requests)
}

/// Sirve `app` en un puerto efímero y devuelve una `base_url` con sufijo `/v1`.
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

/// Levanta el gateway con el YAML dado y devuelve `(url, AppState)`.
///
/// Se conserva el `AppState` para poder invocar `warmup_once` directamente.
async fn spawn_gateway(yaml: &str) -> (String, AppState) {
    let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");
    let state = AppState::new(config).expect("construir AppState");
    let app = router(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve gateway");
    });
    (format!("http://{addr}"), state)
}

/// Construye el YAML de una ruta `mech-writer` con un único backend.
fn config_yaml(
    base_url: &str,
    model: &str,
    warmup_interval_ms: Option<u64>,
    api_key_env: Option<&str>,
) -> String {
    let mut backend = format!("      - base_url: \"{base_url}\"\n        model: \"{model}\"\n");
    if let Some(var) = api_key_env {
        backend.push_str(&format!("        api_key_env: \"{var}\"\n"));
    }
    if let Some(interval_ms) = warmup_interval_ms {
        backend.push_str(&format!(
            "        warmup:\n          interval_ms: {interval_ms}\n"
        ));
    }
    format!("route:\n  - name: mech-writer\n    backend:\n{backend}")
}

/// `GET /metrics` del gateway como texto Prometheus.
async fn get_metrics(gateway: &str) -> String {
    reqwest::get(format!("{gateway}/metrics"))
        .await
        .expect("GET /metrics")
        .text()
        .await
        .expect("cuerpo de /metrics")
}

/// `true` si `metrics` expone alguna **serie** (línea de muestra, no las
/// directivas `# HELP`/`# TYPE`, que se emiten siempre) con ese nombre.
fn has_series(metrics: &str, name: &str) -> bool {
    let sample_prefix = format!("{name}{{");
    metrics.lines().any(|line| line.starts_with(&sample_prefix))
}

/// Extrae el valor numérico de la primera serie que empiece por `series`
/// (p. ej. `llm_gateway_warmup_total{route="mech-writer",...result="failure"}`).
fn metric_value(metrics: &str, series: &str) -> Option<f64> {
    metrics
        .lines()
        .find(|line| line.starts_with(series))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}

/// Construye el YAML de una ruta `mech-writer` con **dos** backends, ambos con
/// `warmup` de `warmup_interval_ms`.
fn config_yaml_two_backends(first_url: &str, second_url: &str, warmup_interval_ms: u64) -> String {
    format!(
        "route:\n  - name: mech-writer\n    backend:\n      - base_url: \"{first_url}\"\n        model: \"primary\"\n        warmup:\n          interval_ms: {warmup_interval_ms}\n      - base_url: \"{second_url}\"\n        model: \"secondary\"\n        warmup:\n          interval_ms: {warmup_interval_ms}\n"
    )
}

/// Scenario 1.2: backend sin `warmup` → 0 peticiones de warmup.
#[tokio::test]
async fn backend_without_warmup_receives_no_requests() {
    let (backend, requests) = spawn_capturing_backend().await;
    let yaml = config_yaml(&backend, "qwen2.5:7b-instruct", None, None);
    let (gateway, state) = spawn_gateway(&yaml).await;

    let handles = spawn_warmup(state);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let seen = requests.lock().expect("lock del mock").len();
    assert_eq!(
        seen, 0,
        "un backend sin bloque `warmup` no debe recibir ninguna petición"
    );

    let body = get_metrics(&gateway).await;
    assert!(
        !has_series(&body, "llm_gateway_backend_warm"),
        "un backend sin warmup no debe exponer la serie backend_warm:\n{body}"
    );
    assert!(
        !has_series(&body, "llm_gateway_warmup_total"),
        "un backend sin warmup no debe exponer la serie warmup_total:\n{body}"
    );
    drop(handles);
}

/// Scenario 1.3: warmup con éxito → petición mínima correcta + métricas.
#[tokio::test]
async fn warmup_once_success_sends_minimal_request_and_records_metrics() {
    let (backend, requests) = spawn_capturing_backend().await;
    let yaml = config_yaml(&backend, "qwen2.5:7b-instruct", Some(240_000), None);
    let (gateway, state) = spawn_gateway(&yaml).await;

    let ok = warmup_once(&state, 0, 0).await;
    assert!(ok, "un warmup contra un backend 200 debe devolver true");

    let id = backend_id(&state.config.route[0].backend[0]);
    {
        let captured = requests.lock().expect("lock del mock");
        assert_eq!(captured.len(), 1, "el mock debe ver exactamente un warmup");
        let request = &captured[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/chat/completions");
        assert_eq!(request.body["model"], json!("qwen2.5:7b-instruct"));
        assert_eq!(
            request.body["messages"],
            json!([{"role": "user", "content": "ping"}]),
            "el warmup debe enviar un único mensaje user con \"ping\""
        );
        assert_eq!(request.body["max_tokens"], json!(1));
        assert_ne!(
            request.body.get("stream"),
            Some(&json!(true)),
            "el warmup no debe pedir `stream: true`"
        );
    }

    let body = get_metrics(&gateway).await;
    assert!(
        body.contains(&format!(
            r#"llm_gateway_backend_warm{{route="mech-writer",backend="{id}"}} 1"#
        )),
        "gauge backend_warm esperado a 1:\n{body}"
    );
    assert!(
        body.contains(&format!(
            r#"llm_gateway_warmup_total{{route="mech-writer",backend="{id}",result="success"}} 1"#
        )),
        "counter warmup_total success esperado a 1:\n{body}"
    );
}

/// Scenario 1.4: warmup contra backend caído → `false` + métricas de fallo.
#[tokio::test]
async fn warmup_once_failure_records_failure_metrics() {
    let dead = common::dead_backend().await;
    let yaml = config_yaml(&dead, "primary", Some(240_000), None);
    let (gateway, state) = spawn_gateway(&yaml).await;

    let ok = warmup_once(&state, 0, 0).await;
    assert!(!ok, "un warmup contra un backend caído debe devolver false");

    let id = backend_id(&state.config.route[0].backend[0]);
    let body = get_metrics(&gateway).await;
    assert!(
        body.contains(&format!(
            r#"llm_gateway_backend_warm{{route="mech-writer",backend="{id}"}} 0"#
        )),
        "gauge backend_warm debe valer 0:\n{body}"
    );
    assert!(
        body.contains(&format!(
            r#"llm_gateway_warmup_total{{route="mech-writer",backend="{id}",result="failure"}} 1"#
        )),
        "counter warmup_total failure esperado a 1:\n{body}"
    );
}

/// Scenario 1.5: heartbeat periódico → ≥2 warmups en ~200 ms.
#[tokio::test]
async fn spawn_warmup_repeats_on_interval() {
    let (backend, requests) = spawn_capturing_backend().await;
    let yaml = config_yaml(&backend, "qwen2.5:7b-instruct", Some(50), None);
    let (_gateway, state) = spawn_gateway(&yaml).await;

    let handles = spawn_warmup(state);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let seen = requests.lock().expect("lock del mock").len();
    assert!(
        seen >= 2,
        "se esperaban ≥2 warmups en ~200 ms con interval_ms=50, hubo {seen}"
    );
    drop(handles);
}

/// Scenario 1.7: un warmup fallido no interrumpe el keep-alive ni afecta a un
/// backend sano de la misma instancia/ruta.
#[tokio::test]
async fn failed_warmup_keeps_retrying_and_does_not_affect_healthy_backend() {
    let dead = common::dead_backend().await;
    let (healthy, healthy_requests) = spawn_capturing_backend().await;
    let yaml = config_yaml_two_backends(&dead, &healthy, 50);
    let (gateway, state) = spawn_gateway(&yaml).await;

    let handles = spawn_warmup(state.clone());

    tokio::time::sleep(Duration::from_millis(200)).await;

    let dead_id = backend_id(&state.config.route[0].backend[0]);
    let healthy_id = backend_id(&state.config.route[0].backend[1]);

    let body = get_metrics(&gateway).await;

    // El backend caído repite: ≥2 fallos registrados en ~200 ms con interval_ms=50.
    let failure_series = format!(
        r#"llm_gateway_warmup_total{{route="mech-writer",backend="{dead_id}",result="failure"}}"#
    );
    let failures = metric_value(&body, &failure_series)
        .expect("serie warmup_total failure del backend caído esperada");
    assert!(
        failures >= 2.0,
        "un warmup fallido no debe abortar el bucle: se esperaban ≥2 fallos, hubo {failures}\n{body}"
    );

    // El backend sano sigue recibiendo sus warmups con normalidad.
    let healthy_seen = healthy_requests.lock().expect("lock del mock").len();
    assert!(
        healthy_seen >= 2,
        "el backend sano no debe verse afectado: se esperaban ≥2 warmups, hubo {healthy_seen}"
    );
    let success_series = format!(
        r#"llm_gateway_warmup_total{{route="mech-writer",backend="{healthy_id}",result="success"}}"#
    );
    let healthy_ok = metric_value(&body, &success_series)
        .expect("serie warmup_total success del backend sano esperada");
    assert!(
        healthy_ok >= 2.0,
        "el backend sano debe registrar ≥2 warmups con éxito, hubo {healthy_ok}\n{body}"
    );

    drop(handles);
}

/// Scenario 1.6a: `api_key_env` resuelto → el mock recibe `Bearer <valor>`.
#[tokio::test]
async fn warmup_sends_bearer_token_from_env() {
    let var = format!("GATEWAY_TEST_WARMUP_KEY_OK_{}", std::process::id());
    let value = "s3cr3t-warmup-key";
    // SAFETY: nombre de variable único para este test; ningún otro test toca esta
    // variable. El binario de test es un proceso aparte.
    unsafe { std::env::set_var(&var, value) };

    let (backend, requests) = spawn_capturing_backend().await;
    let yaml = config_yaml(&backend, "qwen2.5:7b-instruct", Some(240_000), Some(&var));
    let (_gateway, state) = spawn_gateway(&yaml).await;

    let ok = warmup_once(&state, 0, 0).await;

    let seen = requests.lock().expect("lock del mock").clone();
    unsafe { std::env::remove_var(&var) };

    assert!(ok, "el warmup con api key resuelta debe devolver true");
    assert_eq!(seen.len(), 1, "el mock debe ver un warmup");
    let expected = format!("Bearer {value}");
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some(expected.as_str()),
        "el warmup debe reenviar el Bearer con el valor de la variable"
    );
}

/// Scenario 1.6b: variable de entorno ausente → failure sin petición HTTP.
#[tokio::test]
async fn warmup_missing_env_fails_without_http_request() {
    let var = format!("GATEWAY_TEST_WARMUP_KEY_MISSING_{}", std::process::id());
    // SAFETY: por si el entorno del runner la trajese definida; nombre único.
    unsafe { std::env::remove_var(&var) };

    let (backend, requests) = spawn_capturing_backend().await;
    let yaml = config_yaml(&backend, "qwen2.5:7b-instruct", Some(240_000), Some(&var));
    let (gateway, state) = spawn_gateway(&yaml).await;

    let ok = warmup_once(&state, 0, 0).await;
    assert!(!ok, "sin la variable de entorno el warmup debe fallar");

    let id = backend_id(&state.config.route[0].backend[0]);
    let body = get_metrics(&gateway).await;
    assert!(
        body.contains(&format!(
            r#"llm_gateway_warmup_total{{route="mech-writer",backend="{id}",result="failure"}} 1"#
        )),
        "counter warmup_total failure esperado a 1:\n{body}"
    );

    assert_eq!(
        requests.lock().expect("lock del mock").len(),
        0,
        "sin api key NO debe emitirse ninguna petición HTTP"
    );
}
