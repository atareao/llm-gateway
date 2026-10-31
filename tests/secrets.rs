//! Tests de integración de los secretos por entorno (Requisito 7).
//!
//! Cada test usa una variable de entorno única para evitar carreras entre
//! tests que corren en paralelo sobre el mismo proceso.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use llm_gateway::config::GatewayConfig;
use llm_gateway::proxy::{AppState, router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Genera un nombre de variable de entorno único por invocación.
fn unique_env(prefix: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// Mock que devuelve el valor de la cabecera `Authorization` recibida.
async fn spawn_auth_echo_backend() -> String {
    async fn handler(headers: HeaderMap) -> Response {
        let authorization = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        axum::Json(json!({"authorization": authorization})).into_response()
    }
    let app = Router::new().route("/v1/chat/completions", post(handler));
    spawn(app).await
}

/// Mock no-stream que devuelve (echo) el JSON recibido.
async fn spawn_echo_backend() -> String {
    async fn echo(body: Bytes) -> Response {
        let value: Value = serde_json::from_slice(&body).expect("mock: cuerpo JSON");
        axum::Json(value).into_response()
    }
    let app = Router::new().route("/v1/chat/completions", post(echo));
    spawn(app).await
}

/// Mock que responde siempre `500`.
async fn spawn_500_backend() -> String {
    async fn boom() -> Response {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": "boom"})),
        )
            .into_response()
    }
    let app = Router::new().route("/v1/chat/completions", post(boom));
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

/// `backends`: lista `(base_url, model, api_key_env)`.
async fn spawn_gateway(backends: &[(&str, &str, Option<&str>)]) -> String {
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

async fn post_chat(gateway: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("request al gateway")
}

/// Scenario: clave resuelta en tiempo de petición → `Authorization: Bearer`.
#[tokio::test]
async fn resolved_api_key_is_sent_as_bearer() {
    let var = unique_env("LLM_GW_TEST_KEY");
    let secret = "sk-test-abc123";
    // SAFETY: variable de entorno única para este test.
    unsafe { std::env::set_var(&var, secret) };

    let backend = spawn_auth_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "qwen2.5:7b-instruct", Some(&var))]).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let body: Value = response.json().await.expect("json de respuesta");
    assert_eq!(body["authorization"], json!(format!("Bearer {secret}")));
}

/// Scenario: variable ausente → ese backend se salta (failover al siguiente).
#[tokio::test]
async fn missing_env_var_skips_backend_and_fails_over() {
    let var = unique_env("LLM_GW_TEST_MISSING");
    // SAFETY: variable de entorno única, aseguramos que no existe.
    unsafe { std::env::remove_var(&var) };

    let fallback = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[
        (&dead_backend().await, "qwen2.5:7b-instruct", Some(&var)),
        (&fallback, "qwen/qwen3-8b", None),
    ])
    .await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-gateway-backend")
            .and_then(|v| v.to_str().ok()),
        Some(format!("{fallback}#qwen/qwen3-8b").as_str())
    );

    let body: Value = response.json().await.expect("json de respuesta");
    assert_eq!(body["model"], json!("qwen/qwen3-8b"));
}

/// Scenario: todas las claves ausentes → `502`.
#[tokio::test]
async fn all_env_vars_missing_returns_502() {
    let var = unique_env("LLM_GW_TEST_ALLMISSING");
    // SAFETY: variable de entorno única.
    unsafe { std::env::remove_var(&var) };

    let gateway = spawn_gateway(&[(
        &spawn_auth_echo_backend().await,
        "qwen2.5:7b-instruct",
        Some(&var),
    )])
    .await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);

    let body: Value = response.json().await.expect("json de error");
    assert_eq!(body["error"]["type"], json!("api_error"));
}

/// Best-effort: el valor de la clave NUNCA aparece en los logs de `tracing`.
#[tokio::test]
async fn secret_never_appears_in_logs() {
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    struct Guard(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Guard {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
        type Writer = Guard;
        fn make_writer(&'a self) -> Self::Writer {
            Guard(Arc::clone(&self.0))
        }
    }

    let buffer = Buffer::default();
    let _ = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_max_level(tracing::Level::TRACE)
        .try_init();

    let var = unique_env("LLM_GW_TEST_LOG");
    let secret = "sk-super-secret-xyz789";
    // SAFETY: variable de entorno única para este test.
    unsafe { std::env::set_var(&var, secret) };

    // Primario con clave definida que devuelve 500 → genera log de failover.
    let backend = spawn_500_backend().await;
    let gateway = spawn_gateway(&[(&backend, "qwen2.5:7b-instruct", Some(&var))]).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);

    let logs = String::from_utf8_lossy(&buffer.0.lock().expect("lock")).into_owned();
    assert!(
        !logs.contains(secret),
        "el secreto no debe aparecer en los logs:\n{logs}"
    );
}
