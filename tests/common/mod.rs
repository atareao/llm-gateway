//! Helpers compartidos por los tests de integración.
//!
//! `#![allow(dead_code)]` porque cada binario de test compila este módulo
//! completo y no todos usan todos los helpers; CI ejecuta
//! `clippy --all-targets -- -D warnings`.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Respuesta de chat estilo OpenAI **con** `tool_calls` en el mensaje.
pub fn tool_call_body() -> Value {
    json!({
        "id": "chatcmpl-tool",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
}

/// Respuesta de chat estilo OpenAI **sin** `tool_calls` (solo texto).
pub fn text_body() -> Value {
    json!({
        "id": "chatcmpl-text",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hola"},
            "finish_reason": "stop"
        }]
    })
}

/// Levanta un mock que responde siempre `200` con el cuerpo JSON dado y cuenta
/// las peticiones recibidas, para comprobar si un backend llegó a contactarse.
pub async fn spawn_json_backend(body: Value) -> (String, Arc<AtomicUsize>) {
    async fn reply(State((counter, body)): State<(Arc<AtomicUsize>, Arc<Value>)>) -> Response {
        counter.fetch_add(1, Ordering::SeqCst);
        axum::Json((*body).clone()).into_response()
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state((Arc::clone(&counter), Arc::new(body)));

    (spawn(app).await, counter)
}

/// Levanta un mock axum en un puerto efímero y devuelve su `base_url` (`/v1`).
pub async fn spawn(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    format!("http://{addr}/v1")
}

/// Devuelve una `base_url` a un backend caído.
///
/// El listener se mantiene enlazado durante toda la vida del test y cierra
/// cada conexión aceptada sin responder. Así el puerto **nunca** queda libre:
/// si lo liberásemos con `drop(listener)`, otro test que corre en paralelo
/// podría enlazarlo y el "primario caído" reviviría respondiendo `404`, lo que
/// convertía `primary_down_fails_over_to_fallback` en un test flaky.
///
/// El cliente ve un error de transporte (conexión cerrada sin respuesta), que
/// es justo lo que el gateway debe tratar como backend no disponible.
pub async fn dead_backend() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => drop(stream),
                // Un error transitorio (ECONNABORTED, EMFILE…) no debe sacarnos
                // del bucle: si salimos, el listener se dropea, el puerto queda
                // libre y la carrera vuelve. Cedemos el turno para no girar en
                // vacío si el error es persistente.
                Err(_) => tokio::task::yield_now().await,
            }
        }
    });
    format!("http://{addr}/v1")
}
