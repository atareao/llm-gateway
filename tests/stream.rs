//! Tests de integración del streaming SSE (Requisito 4).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::StreamExt;
use llm_gateway::config::GatewayConfig;
use llm_gateway::proxy::{AppState, router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Mock que emite un stream SSE por partes y termina con `data: [DONE]`.
async fn spawn_sse_backend() -> String {
    async fn sse() -> Response {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Hola\"}}]}\n\n",
            )),
            Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\" mundo\"}}]}\n\n",
            )),
            Ok(Bytes::from_static(b"data: [DONE]\n\n")),
        ];
        let stream = futures_util::stream::iter(chunks);

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .expect("construir respuesta SSE")
    }

    let app = Router::new().route("/v1/chat/completions", post(sse));
    spawn(app).await
}

/// Mock que emite un primer chunk y **mantiene el stream abierto** (nunca
/// cierra). Sirve para demostrar que el gateway no bufferiza.
async fn spawn_open_sse_backend() -> String {
    async fn sse() -> Response {
        let first = futures_util::stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Hola\"}}]}\n\n",
            ))
        });
        let pending = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(first.chain(pending)))
            .expect("construir respuesta SSE")
    }

    let app = Router::new().route("/v1/chat/completions", post(sse));
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

/// Mock SSE que **duerme** `delay` antes de responder (simula un backend que
/// acepta TCP pero tarda en emitir el primer byte).
async fn spawn_slow_sse_backend(delay: Duration) -> String {
    async fn sse(State(delay): State<Duration>) -> Response {
        tokio::time::sleep(delay).await;
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            vec![Ok(Bytes::from_static(b"data: [DONE]\n\n"))];
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .expect("construir respuesta SSE")
    }

    let app = Router::new()
        .route("/v1/chat/completions", post(sse))
        .with_state(delay);
    spawn(app).await
}

/// Mock SSE que emite un chunk, lo deja fluir, y **luego aborta** el stream.
async fn spawn_aborting_sse_backend() -> String {
    async fn sse() -> Response {
        let first = futures_util::stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Hola\"}}]}\n\n",
            ))
        });
        let aborted = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Err::<Bytes, std::io::Error>(std::io::Error::other("upstream abortado a mitad"))
        });
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(first.chain(aborted)))
            .expect("construir respuesta SSE")
    }

    let app = Router::new().route("/v1/chat/completions", post(sse));
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

/// Puerto donde nadie escucha (conexión rechazada).
async fn dead_backend() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    format!("http://{addr}/v1")
}

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

/// Como [`spawn_gateway`] pero con `timeout_ms` opcional por backend.
async fn spawn_gateway_with_timeout(backends: &[(&str, &str, Option<u64>)]) -> String {
    let mut yaml = String::from("route:\n  - name: mech-writer\n    backend:\n");
    for (base_url, model, timeout_ms) in backends {
        yaml.push_str(&format!(
            "      - base_url: \"{base_url}\"\n        model: \"{model}\"\n"
        ));
        if let Some(ms) = timeout_ms {
            yaml.push_str(&format!("        timeout_ms: {ms}\n"));
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

/// El primer chunk debe llegar **antes** de que el upstream cierre el stream:
/// prueba que el gateway reenvía sin bufferizar.
#[tokio::test]
async fn first_chunk_arrives_without_buffering() {
    let backend = spawn_open_sse_backend().await;
    let gateway = spawn_gateway(&[(&backend, "qwen2.5:7b-instruct")]).await;

    let client = reqwest::Client::new();
    let first_chunk = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let response = client
            .post(format!("{gateway}/v1/chat/completions"))
            .json(&json!({"model": "mech-writer", "stream": true, "messages": []}))
            .send()
            .await
            .expect("request al gateway");

        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let mut stream = response.bytes_stream();
        stream
            .next()
            .await
            .expect("algún chunk")
            .expect("chunk válido")
    })
    .await
    .expect("el primer chunk no debe esperar a que el upstream cierre (no buffering)");

    let text = String::from_utf8_lossy(&first_chunk);
    assert!(
        text.contains("{\"choices\":[{\"delta\":{\"content\":\"Hola\"}}]}"),
        "chunk inesperado: {text}"
    );
}

/// Scenario: Passthrough de stream → varios chunks `data:` + `data: [DONE]`.
#[tokio::test]
async fn stream_is_passed_through_chunk_by_chunk() {
    let backend = spawn_sse_backend().await;
    let gateway = spawn_gateway(&[(&backend, "qwen2.5:7b-instruct")]).await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": "mech-writer", "stream": true, "messages": []}))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        response
            .headers()
            .get("x-gateway-backend")
            .and_then(|v| v.to_str().ok()),
        Some(format!("{backend}#qwen2.5:7b-instruct").as_str())
    );

    let body = response.text().await.expect("cuerpo del stream");
    assert!(
        body.contains("data: {\"choices\":[{\"delta\":{\"content\":\"Hola\"}}]}"),
        "falta el primer chunk: {body}"
    );
    assert!(
        body.contains("data: {\"choices\":[{\"delta\":{\"content\":\" mundo\"}}]}"),
        "falta el segundo chunk: {body}"
    );
    assert!(body.contains("data: [DONE]"), "falta el terminador: {body}");
}

/// Scenario: Stream no solicitado → respuesta no-stream.
#[tokio::test]
async fn stream_not_requested_returns_non_stream() {
    let backend = spawn_echo_backend().await;
    let gateway = spawn_gateway(&[(&backend, "qwen2.5:7b-instruct")]).await;

    for body in [
        json!({"model": "mech-writer", "messages": []}),
        json!({"model": "mech-writer", "stream": false, "messages": []}),
    ] {
        let response = reqwest::Client::new()
            .post(format!("{gateway}/v1/chat/completions"))
            .json(&body)
            .send()
            .await
            .expect("request al gateway");

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        assert_eq!(
            content_type.as_deref(),
            Some("application/json"),
            "sin stream no debe ser text/event-stream"
        );
    }
}

/// Scenario: Failover en stream antes del primer byte (primario caído).
#[tokio::test]
async fn stream_fails_over_before_first_byte() {
    let fallback = spawn_sse_backend().await;
    let primary = dead_backend().await;
    let gateway = spawn_gateway(&[
        (&primary, "qwen2.5:7b-instruct"),
        (&fallback, "qwen/qwen3-8b"),
    ])
    .await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": "mech-writer", "stream": true, "messages": []}))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-gateway-backend")
            .and_then(|v| v.to_str().ok()),
        Some(format!("{fallback}#qwen/qwen3-8b").as_str())
    );

    let body = response.text().await.expect("cuerpo del stream");
    assert!(body.contains("data: [DONE]"));
}

/// Scenario: el primario acepta TCP pero tarda en emitir el primer byte → el
/// guard de *time-to-first-byte* lo da por fallido y responde el fallback.
#[tokio::test]
async fn slow_first_byte_fails_over_to_fallback() {
    let primary = spawn_slow_sse_backend(Duration::from_millis(2000)).await;
    let fallback = spawn_sse_backend().await;
    let gateway = spawn_gateway_with_timeout(&[
        (&primary, "qwen2.5:7b-instruct", Some(300)),
        (&fallback, "qwen/qwen3-8b", None),
    ])
    .await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": "mech-writer", "stream": true, "messages": []}))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-gateway-backend")
            .and_then(|v| v.to_str().ok()),
        Some(format!("{fallback}#qwen/qwen3-8b").as_str()),
        "debe responder el fallback tras vencer el time-to-first-byte del primario"
    );

    let body = response.text().await.expect("cuerpo del stream");
    assert!(body.contains("data: [DONE]"));
}

/// Scenario: corte a mitad de stream → NO se reintenta y el stream al cliente
/// termina con error (sin `data: [DONE]`).
#[tokio::test]
async fn mid_stream_cut_does_not_fail_over() {
    let primary = spawn_aborting_sse_backend().await;
    let (fallback, counter) = spawn_counting_backend().await;
    let gateway = spawn_gateway(&[
        (&primary, "qwen2.5:7b-instruct"),
        (&fallback, "qwen/qwen3-8b"),
    ])
    .await;

    let response = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": "mech-writer", "stream": true, "messages": []}))
        .send()
        .await
        .expect("request al gateway");

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let result = response.text().await;
    assert!(
        result.is_err(),
        "el stream debe terminar con error, no completo: {result:?}"
    );
    if let Ok(text) = &result {
        assert!(
            !text.contains("data: [DONE]"),
            "no debe llegar el terminador tras un corte: {text}"
        );
    }

    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "un corte a mitad de stream no debe provocar failover"
    );
}
