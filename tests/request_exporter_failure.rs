//! Grupo 4 del ledger de observabilidad (#39), escenario 4.3: un **fallo del
//! exportador** (aquí, un subscriber que entra en panic al registrar el evento
//! `request`) no altera la respuesta que recibe el cliente, ni en no-stream ni
//! en streaming (donde la emisión ocurre dentro del pump del cuerpo).
//!
//! Cada test usa un runtime `current_thread` con el suscriptor panicoso
//! instalado con `with_default`: todas las tareas del servidor poll-ean en el
//! hilo del test dentro del ámbito del guard, así el «exportador caído» cubre
//! toda la petición sin tocar el suscriptor global del proceso.

use axum::Json;
use axum::Router;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry;

use llm_gateway_rs::config::GatewayConfig;
use llm_gateway_rs::proxy::{AppState, REQUEST_ID_HEADER, router};

/// Respuesta no-stream con `usage`.
const CHAT_JSON: &str = r#"{
    "choices": [{"message": {"role": "assistant", "content": "hola"}, "finish_reason": "stop"}],
    "usage": {"prompt_tokens": 1000, "completion_tokens": 200}
}"#;

/// Stream SSE con contenido, `finish_reason`, chunk final con `usage` y
/// `[DONE]`.
const CHAT_SSE: &str = concat!(
    "data: ",
    r#"{"choices":[{"delta":{"content":"hol"},"finish_reason":null}]}"#,
    "\n\n",
    "data: ",
    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    "\n\n",
    "data: ",
    r#"{"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":200}}"#,
    "\n\n",
    "data: [DONE]\n\n"
);

/// Catálogo del mock con un único modelo de pago con precios conocidos.
const CATALOG_JSON: &str = r#"{
    "data": [{
        "id": "acme/flash-a",
        "context_length": 8000,
        "pricing": {"prompt": "0.000002", "completion": "0.000008"},
        "supported_parameters": ["tools", "tool_choice"]
    }]
}"#;

/// La variable de entorno de la API key, fijada una vez por binario de test.
fn ensure_api_key() {
    static SET: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    SET.get_or_init(|| unsafe {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
    });
}

/// Levanta un servidor efímero y devuelve su `http://ip:puerto`.
async fn spawn(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    format!("http://{addr}")
}

/// Mock del upstream de pago con el chat indicado (JSON o SSE).
async fn spawn_paid_mock(chat_body: &'static str, chat_content_type: &'static str) -> String {
    let app = Router::new()
        .route(
            "/models",
            get(|| async { Json(json!(serde_json::from_str::<Value>(CATALOG_JSON).expect("catálogo"))) }),
        )
        .route(
            "/chat/completions",
            post(move || async move {
                (
                    [(header::CONTENT_TYPE, chat_content_type)],
                    chat_body,
                )
                    .into_response()
            }),
        );
    spawn(app).await
}

/// Levanta el gateway con un backend de pago dinámico sobre `base_url`.
async fn spawn_gateway(base_url: &str) -> String {
    let yaml = format!(
        r#"
route:
  - name: code
    backend:
      - kind: openrouter_paid
        profile: code
        base_url: "{base_url}"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          allow: ["acme/flash-*"]
"#
    );
    let config: GatewayConfig = serde_saphyr::from_str(&yaml).expect("config de test válida");
    let state = AppState::new(config).expect("construir AppState");
    spawn(router(state)).await
}

/// Capa que **entra en panic** al registrar el evento `request` (`event_type`
/// `request`): simula un exportador caído. El resto de eventos se descartan.
#[derive(Clone)]
struct PanickingLedgerLayer;

impl<S: Subscriber> Layer<S> for PanickingLedgerLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut visitor = EventFieldsVisitor::default();
        event.record(&mut visitor);
        if visitor
            .fields
            .iter()
            .any(|field| field == "event_type=request")
        {
            panic!("exportador caído");
        }
    }
}

/// Visitante que serializa los campos del evento como `campo=valor`.
#[derive(Default)]
struct EventFieldsVisitor {
    fields: Vec<String>,
}

impl Visit for EventFieldsVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.push(format!("{}={value:?}", field.name()));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.push(format!("{}={value}", field.name()));
    }
}

/// Petición de chat al gateway con un `request_id` determinista.
async fn post_chat(gateway: &str, request_id: &str, stream: bool) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .header(REQUEST_ID_HEADER, request_id)
        .json(&json!({
            "model": "code",
            "messages": [{"role": "user", "content": "hola"}],
            "stream": stream,
        }))
        .send()
        .await
        .expect("petición al gateway")
}

/// Escenario «Fallo de emisión no afecta a la respuesta» (4.3, no-stream): con
/// el exportador en panic, la petición se atiende con normalidad.
#[tokio::test]
async fn exporter_failure_does_not_alter_non_stream_response() {
    ensure_api_key();
    let mock = spawn_paid_mock(CHAT_JSON, "application/json").await;
    let gateway = spawn_gateway(&mock).await;

    let _panic_guard = tracing::subscriber::set_default(registry().with(PanickingLedgerLayer));
    let response = post_chat(&gateway, "req-exporter-fails", false).await;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "un fallo del exportador no altera el status de la respuesta"
    );
    let body: Value = response.json().await.expect("json de respuesta");
    assert_eq!(body["choices"][0]["finish_reason"], json!("stop"));
    assert_eq!(
        body["usage"]["completion_tokens"],
        json!(200),
        "el cuerpo llega íntegro al cliente"
    );
}

/// Escenario 4.3 (streaming): el fallo del exportador ocurre **dentro del
/// pump** del stream (al agotarse); el cliente recibe el stream íntegro.
#[tokio::test]
async fn exporter_failure_does_not_alter_stream_response() {
    ensure_api_key();
    let mock = spawn_paid_mock(CHAT_SSE, "text/event-stream").await;
    let gateway = spawn_gateway(&mock).await;

    let _panic_guard = tracing::subscriber::set_default(registry().with(PanickingLedgerLayer));
    let response = post_chat(&gateway, "req-exporter-fails-stream", true).await;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "un fallo del exportador no altera el status del stream"
    );
    let body = response.text().await.expect("stream completo");
    assert!(
        body.contains("\"finish_reason\":\"stop\""),
        "los chunks de contenido llegan íntegros: {body}"
    );
    assert!(
        body.contains("\"prompt_tokens\":1000"),
        "el chunk final con usage llega íntegro: {body}"
    );
    assert!(body.contains("[DONE]"), "el stream termina con normalidad");
}
