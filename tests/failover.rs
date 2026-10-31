//! Tests de integración del failover antes del primer token.
//!
//! Cada ruta tiene un primario y un fallback; se simulan backends caídos con
//! puertos sin nadie escuchando o mocks que devuelven `429`/`500`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use llm_gateway::config::GatewayConfig;
use llm_gateway::proxy::{AppState, backend_id, router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Mock que devuelve (echo) el JSON recibido.
async fn spawn_echo_backend() -> String {
    async fn echo(body: Bytes) -> Response {
        let value: Value = serde_json::from_slice(&body).expect("mock: cuerpo JSON");
        axum::Json(value).into_response()
    }
    let app = Router::new().route("/v1/chat/completions", post(echo));
    spawn(app).await
}

/// Mock que cuenta peticiones y responde `200`.
async fn spawn_counting_backend() -> (String, Arc<AtomicUsize>) {
    async fn reply(State(counter): State<Arc<AtomicUsize>>) -> Response {
        counter.fetch_add(1, Ordering::SeqCst);
        axum::Json(json!({"ok": true})).into_response()
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(Arc::clone(&counter));
    (spawn(app).await, counter)
}

/// Mock que responde siempre con un `status` y cuerpo fijos.
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

/// Devuelve una `base_url` a un puerto donde nadie escucha (conexión rechazada).
async fn dead_backend() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener); // liberamos el puerto: nadie escuchará
    format!("http://{addr}/v1")
}

struct Backends {
    primary: String,
    primary_model: String,
    fallback: String,
    fallback_model: String,
}

/// Levanta el gateway con la ruta `mech-writer` = [primario, fallback].
async fn spawn_gateway(b: &Backends) -> String {
    let yaml = format!(
        r#"
route:
  - name: mech-writer
    backend:
      - base_url: "{}"
        model: "{}"
      - base_url: "{}"
        model: "{}"
"#,
        b.primary, b.primary_model, b.fallback, b.fallback_model
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

fn backend_header(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("x-gateway-backend")
        .expect("cabecera x-gateway-backend presente")
        .to_str()
        .expect("valor de cabecera ASCII")
        .to_owned()
}

async fn post_chat(gateway: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("request al gateway")
}

/// Scenario: Primario caído → responde el fallback y se identifica en la cabecera.
#[tokio::test]
async fn primary_down_fails_over_to_fallback() {
    let fallback = spawn_echo_backend().await;
    let b = Backends {
        primary: dead_backend().await,
        primary_model: "qwen2.5:7b-instruct".to_owned(),
        fallback: fallback.clone(),
        fallback_model: "qwen/qwen3-8b".to_owned(),
    };
    let gateway = spawn_gateway(&b).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        backend_header(&response),
        format!("{fallback}#qwen/qwen3-8b")
    );

    let body: Value = response.json().await.expect("json de respuesta");
    assert_eq!(body["model"], json!("qwen/qwen3-8b"));
}

/// Scenario: Primario OK → NO se usa el fallback.
#[tokio::test]
async fn primary_ok_does_not_touch_fallback() {
    let primary = spawn_echo_backend().await;
    let (fallback, counter) = spawn_counting_backend().await;
    let b = Backends {
        primary: primary.clone(),
        primary_model: "qwen2.5:7b-instruct".to_owned(),
        fallback,
        fallback_model: "qwen/qwen3-8b".to_owned(),
    };
    let gateway = spawn_gateway(&b).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        backend_header(&response),
        format!("{primary}#qwen2.5:7b-instruct")
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "el fallback no debe recibir peticiones si el primario responde"
    );
}

/// Scenario: Todos los backends caídos → `502` estilo OpenAI.
#[tokio::test]
async fn all_backends_down_returns_502() {
    let b = Backends {
        primary: dead_backend().await,
        primary_model: "qwen2.5:7b-instruct".to_owned(),
        fallback: dead_backend().await,
        fallback_model: "qwen/qwen3-8b".to_owned(),
    };
    let gateway = spawn_gateway(&b).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;

    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);

    let body: Value = response.json().await.expect("json de error");
    assert_eq!(body["error"]["type"], json!("api_error"));
}

/// Scenario: `429` del primario → sí hay failover.
#[tokio::test]
async fn primary_429_fails_over() {
    let primary =
        spawn_status_backend(StatusCode::TOO_MANY_REQUESTS, json!({"error": "slow down"})).await;
    let fallback = spawn_echo_backend().await;
    let b = Backends {
        primary,
        primary_model: "qwen2.5:7b-instruct".to_owned(),
        fallback: fallback.clone(),
        fallback_model: "qwen/qwen3-8b".to_owned(),
    };
    let gateway = spawn_gateway(&b).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        backend_header(&response),
        format!("{fallback}#qwen/qwen3-8b")
    );
}

/// Scenario: `500` del primario → sí hay failover.
#[tokio::test]
async fn primary_500_fails_over() {
    let primary =
        spawn_status_backend(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "boom"})).await;
    let fallback = spawn_echo_backend().await;
    let b = Backends {
        primary,
        primary_model: "qwen2.5:7b-instruct".to_owned(),
        fallback: fallback.clone(),
        fallback_model: "qwen/qwen3-8b".to_owned(),
    };
    let gateway = spawn_gateway(&b).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        backend_header(&response),
        format!("{fallback}#qwen/qwen3-8b")
    );
}

/// Scenario: `400` del primario → NO hay failover, se devuelve tal cual.
#[tokio::test]
async fn primary_400_does_not_fail_over() {
    let error_body = json!({
        "error": {
            "message": "bad request",
            "type": "invalid_request_error",
            "code": "bad_request"
        }
    });
    let primary = spawn_status_backend(StatusCode::BAD_REQUEST, error_body.clone()).await;
    let (fallback, counter) = spawn_counting_backend().await;
    let b = Backends {
        primary,
        primary_model: "qwen2.5:7b-instruct".to_owned(),
        fallback,
        fallback_model: "qwen/qwen3-8b".to_owned(),
    };
    let gateway = spawn_gateway(&b).await;

    let response = post_chat(&gateway, json!({"model": "mech-writer", "messages": []})).await;

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json de respuesta");
    assert_eq!(body, error_body);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "un 400 no debe provocar failover"
    );
}

/// Comprobación de que el identificador de backend es el documentado.
#[test]
fn backend_id_format_is_base_url_hash_model() {
    let backend = llm_gateway::config::Backend {
        base_url: "http://localhost:11434/v1".to_owned(),
        model: "qwen2.5:7b-instruct".to_owned(),
        api_key_env: None,
        timeout_ms: None,
    };
    assert_eq!(
        backend_id(&backend),
        "http://localhost:11434/v1#qwen2.5:7b-instruct"
    );
}
