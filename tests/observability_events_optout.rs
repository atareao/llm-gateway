//! Grupo 6 del ledger de observabilidad (#39): opt-out `observability.events`.
//!
//! Con `observability.events: false` el gateway NO emite ninguno de los tres
//! eventos del ledger (`catalog`, `selection`, `request`), ni siquiera en una
//! petición completa contra un backend de pago (que recién trae el catálogo,
//! expande candidatos y completa la petición: las tres vías de emisión).
//!
//! Cada test usa un runtime `current_thread` con un suscriptor thread-local
//! (`set_default`): todas las tareas del servidor se poll-ean en el hilo del
//! test y quedan capturadas sin tocar el suscriptor global del proceso.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::Request;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry;

use llm_gateway_rs::config::GatewayConfig;
use llm_gateway_rs::proxy::{AppState, REQUEST_ID_HEADER, router};

/// Respuesta no-stream de ejemplo con `usage` y `finish_reason` completos.
const CHAT_JSON: &str = r#"{
    "choices": [{"message": {"role": "assistant", "content": "hola"}, "finish_reason": "stop"}],
    "usage": {"prompt_tokens": 1000, "completion_tokens": 200}
}"#;

/// Stream SSE de ejemplo: contenido, `finish_reason`, chunk final con `usage`
/// y `[DONE]`.
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

/// Catálogo del mock: un único modelo de pago con precios conocidos.
const CATALOG_JSON: &str = r#"{
    "data": [{
        "id": "acme/flash-a",
        "context_length": 8000,
        "pricing": {"prompt": "0.000002", "completion": "0.000008"},
        "supported_parameters": ["tools", "tool_choice"]
    }]
}"#;

/// Config de pago dinámico con el opt-out: `observability.events: false`.
fn paid_config_events_false(base_url: &str) -> String {
    format!(
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
observability:
  events: false
"#
    )
}

/// Misma config de pago SIN el bloque `observability`: default `true`
/// (control positivo del test de opt-out).
fn paid_config_default(base_url: &str) -> String {
    format!(
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
    )
}

/// La variable de entorno de la API key, fijada una vez por binario de test.
fn ensure_api_key() {
    static SET: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    SET.get_or_init(|| unsafe {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
    });
}

/// Levanta un servidor efímero y devuelve su `http://ip:puerto`.
async fn spawn(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    format!("http://{addr}")
}

/// Mock del upstream: catálogo en `/models` y chat en `/chat/completions` que
/// responde JSON o SSE según pida la petición (cubre las dos vías de emisión
/// del evento `request`: respuesta completa y `RequestSink` del stream).
async fn spawn_paid_mock() -> String {
    let app = Router::new()
        .route(
            "/models",
            get(|| async {
                Json(json!(serde_json::from_str::<Value>(CATALOG_JSON).expect("catálogo")))
            }),
        )
        .route(
            "/chat/completions",
            post(|request: Request| async move {
                let body = axum::body::to_bytes(
                    request.into_body(),
                    usize::try_from(1024 * 1024).expect("límite razonable"),
                )
                .await
                .expect("cuerpo de la petición");
                let value: Value = serde_json::from_slice(&body).expect("JSON de la petición");
                let streaming = value.get("stream").and_then(Value::as_bool).unwrap_or(false);
                if streaming {
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        CHAT_SSE,
                    )
                        .into_response()
                } else {
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        CHAT_JSON,
                    )
                        .into_response()
                }
            }),
        );
    spawn(app).await
}

/// Levanta el gateway con la configuración indicada.
async fn spawn_gateway(yaml: &str) -> String {
    let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");
    let state = AppState::new(config).expect("construir AppState");
    spawn(router(state)).await
}

/// Capa que captura los campos de los logs `INFO` del hilo actual.
#[derive(Clone)]
struct LedgerLayer {
    events: Arc<Mutex<Vec<String>>>,
}

impl<S: Subscriber> Layer<S> for LedgerLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        if event.metadata().level() != &Level::INFO {
            return;
        }
        let mut visitor = EventFieldsVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("lock captured ledger logs")
            .push(visitor.fields.join(" "));
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

/// Captura los logs `INFO` del hilo actual y devuelve el buffer compartido.
fn capture_ledger_logs() -> (Arc<Mutex<Vec<String>>>, tracing::subscriber::DefaultGuard) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = registry().with(LedgerLayer {
        events: Arc::clone(&events),
    });
    (events, tracing::subscriber::set_default(subscriber))
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

/// Escenario «Emisión desactivada»: con `observability.events: false` una
/// petición completa (catálogo frío → selección → respuesta, y su gemela en
/// stream) no emite NINGUNO de los tres eventos.
#[tokio::test]
async fn events_false_emits_no_catalog_selection_nor_request_events() {
    ensure_api_key();
    let mock = spawn_paid_mock().await;
    let gateway = spawn_gateway(&paid_config_events_false(&mock)).await;
    let (captured, _guard) = capture_ledger_logs();

    // Vía 1: respuesta completa (emisión en el handler).
    let response = post_chat(&gateway, "req-optout-nonstream", false).await;
    assert!(
        response.status().is_success(),
        "la petición non-stream atiende: {}",
        response.status()
    );

    // Vía 2: streaming (emisión en `RequestSink::complete`).
    let response = post_chat(&gateway, "req-optout-stream", true).await;
    assert!(
        response.status().is_success(),
        "la petición stream atiende: {}",
        response.status()
    );
    let body = response.text().await.expect("cuerpo del stream");
    assert!(body.contains("[DONE]"), "el stream llega al centinela: {body}");

    let logs = captured.lock().expect("lock captured ledger logs").clone();
    assert!(
        logs.iter().all(|log| !log.contains("event_type=")),
        "con observability.events=false no se emite ningún evento del ledger; logs: {logs:?}"
    );
}

/// Control positivo: la misma petición SIN el bloque `observability` emite los
/// tres eventos (garantiza que la captura del test de opt-out no es vacía por
/// construcción).
#[tokio::test]
async fn default_config_emits_catalog_selection_and_request_events() {
    ensure_api_key();
    let mock = spawn_paid_mock().await;
    let gateway = spawn_gateway(&paid_config_default(&mock)).await;
    let (captured, _guard) = capture_ledger_logs();

    let response = post_chat(&gateway, "req-default-nonstream", false).await;
    assert!(
        response.status().is_success(),
        "la petición atiende: {}",
        response.status()
    );

    let logs = captured.lock().expect("lock captured ledger logs").clone();
    for event_type in ["catalog", "selection", "request"] {
        assert!(
            logs.iter()
                .any(|log| log.contains(&format!("event_type={event_type}"))),
            "el evento {event_type} se emite con la config por defecto; logs: {logs:?}"
        );
    }
}
