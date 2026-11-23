//! Grupo 4 del ledger de observabilidad (#39): evento `gateway_requests`.
//!
//! Cada test usa un runtime `current_thread` (el de `#[tokio::test]`) con un
//! suscriptor **thread-local** (`set_default`): como todas las tareas del
//! servidor se poll-ean en el hilo del test, los logs emitidos dentro quedan
//! capturados sin tocar el suscriptor global del proceso (compartido por los
//! tests en paralelo).

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
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

/// Respuesta no-stream de ejemplo: `finish_reason` y `usage` completos
/// (1000 in / 200 out).
const CHAT_JSON: &str = r#"{
    "choices": [{"message": {"role": "assistant", "content": "hola"}, "finish_reason": "stop"}],
    "usage": {"prompt_tokens": 1000, "completion_tokens": 200}
}"#;

/// Respuesta de ejemplo **sin** `usage`: el upstream no informó tokens.
const CHAT_JSON_NO_USAGE: &str = r#"{"choices": [{"message": {"role": "assistant", "content": "hola"}, "finish_reason": "stop"}]}"#;

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

/// Stream SSE de ejemplo **sin** `usage`: contenido, `finish_reason` y `[DONE]`
/// (el upstream no informó tokens).
const CHAT_SSE_NO_USAGE: &str = concat!(
    "data: ",
    r#"{"choices":[{"delta":{"content":"hol"},"finish_reason":null}]}"#,
    "\n\n",
    "data: ",
    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    "\n\n",
    "data: [DONE]\n\n"
);

/// Catálogo del mock: un único modelo de pago con precios conocidos
/// (`0.000002`/`0.000008` USD por token → 2.0/8.0 USD/Mtok).
const CATALOG_JSON: &str = r#"{
    "data": [{
        "id": "acme/flash-a",
        "context_length": 8000,
        "pricing": {"prompt": "0.000002", "completion": "0.000008"},
        "supported_parameters": ["tools", "tool_choice"]
    }]
}"#;

/// Config del gateway con un backend de pago dinámico sobre `base_url`.
fn paid_config(base_url: &str) -> String {
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

/// Config con un backend muerto delante del backend de pago: el primer intento
/// falla (conexión rechazada) y el failover atiende la petición con el segundo.
fn failover_config(dead_base_url: &str, live_base_url: &str) -> String {
    format!(
        r#"
route:
  - name: code
    backend:
      - base_url: "{dead_base_url}"
        model: "local/dead"
      - kind: openrouter_paid
        profile: code
        base_url: "{live_base_url}"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          allow: ["acme/flash-*"]
"#
    )
}

/// Config con un único backend muerto: la petición agota la cadena.
fn all_failed_config(dead_base_url: &str) -> String {
    format!(
        r#"
route:
  - name: code
    backend:
      - base_url: "{dead_base_url}"
        model: "local/dead"
"#
    )
}

/// La variable de entorno de la API key, fijada una vez por binario de test.
/// Los backends dinámicos la exigen en la validación de la configuración.
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

/// Mock del upstream de pago: sirve el catálogo en `/v1/models` y la respuesta
/// de chat indicada en `/v1/chat/completions` (JSON o SSE según el test).
async fn spawn_paid_mock(chat_body: &'static str, chat_content_type: &'static str) -> String {
    let app = Router::new()
        .route(
            "/models",
            get(|| async {
                Json(json!(
                    serde_json::from_str::<Value>(CATALOG_JSON).expect("catálogo")
                ))
            }),
        )
        .route(
            "/chat/completions",
            post(move || async move {
                ([(header::CONTENT_TYPE, chat_content_type)], chat_body).into_response()
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

/// Captura los logs `INFO` del hilo actual (runtime `current_thread`: todas
/// las tareas del servidor poll-ean aquí) y devuelve el buffer compartido.
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

/// El log del evento `gateway_requests` de un `request_id` concreto. Filtrar
/// por `request_id` aísla el test de los eventos de otros tests en paralelo.
fn find_request_event(captured: &Arc<Mutex<Vec<String>>>, request_id: &str) -> String {
    let logs = captured.lock().expect("lock captured ledger logs").clone();
    logs.iter()
        .rev()
        .find(|log| log.contains("event_type=request") && log.contains(request_id))
        .cloned()
        .unwrap_or_else(|| {
            panic!("no se emitió gateway_requests para {request_id}; logs: {logs:?}")
        })
}

/// Escenario «Petición correcta emite evento con coste» (4.1 y 4.2): la
/// respuesta con `usage` emite un evento con los tokens, el `finish_reason`,
/// los precios del catálogo en el momento de la petición (USD/Mtok) y el
/// coste `1000·2/1e6 + 200·8/1e6 = 0.0036` USD.
#[tokio::test]
async fn request_event_logged_for_non_stream_response_with_usage_and_cost() {
    ensure_api_key();
    let mock = spawn_paid_mock(CHAT_JSON, "application/json").await;
    let gateway = spawn_gateway(&paid_config(&mock)).await;
    let (captured, _guard) = capture_ledger_logs();

    let response = post_chat(&gateway, "req-ledger-ok", false).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("json de respuesta");
    assert_eq!(
        body["choices"][0]["finish_reason"],
        json!("stop"),
        "la respuesta al cliente no se altera"
    );

    let event = find_request_event(&captured, "req-ledger-ok");
    assert!(event.contains("event_type=request"), "{event}");
    assert!(event.contains("route=code"), "{event}");
    assert!(event.contains("model_id=acme/flash-a"), "{event}");
    assert!(event.contains("streaming=false"), "{event}");
    assert!(event.contains("attempts=1"), "{event}");
    assert!(event.contains("failover=false"), "{event}");
    assert!(event.contains("outcome=success"), "{event}");
    assert!(event.contains("finish_reason=stop"), "{event}");
    assert!(event.contains("tokens_in=1000"), "{event}");
    assert!(event.contains("tokens_out=200"), "{event}");
    assert!(event.contains("pin=2"), "{event}");
    assert!(event.contains("pout=8"), "{event}");
    assert!(event.contains("cost_usd=0.0036"), "{event}");
    assert!(event.contains("latency_ms="), "{event}");
    assert!(event.contains("ts="), "{event}");
}

/// Escenario «Sin usage el coste queda desconocido» (4.1): la respuesta sin
/// `usage` emite el evento con tokens y coste desconocidos —**nunca `0`**— y
/// el resto de campos registrados igualmente.
#[tokio::test]
async fn request_event_without_usage_keeps_tokens_null_not_zero() {
    ensure_api_key();
    let mock = spawn_paid_mock(CHAT_JSON_NO_USAGE, "application/json").await;
    let gateway = spawn_gateway(&paid_config(&mock)).await;
    let (captured, _guard) = capture_ledger_logs();

    let response = post_chat(&gateway, "req-ledger-no-usage", false).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let event = find_request_event(&captured, "req-ledger-no-usage");
    assert!(event.contains("outcome=success"), "{event}");
    assert!(event.contains("model_id=acme/flash-a"), "{event}");
    assert!(event.contains("finish_reason=stop"), "{event}");
    assert!(
        !event.contains("tokens_in=0"),
        "los tokens desconocidos nunca se registran como 0: {event}"
    );
    assert!(
        !event.contains("tokens_out=0"),
        "los tokens desconocidos nunca se registran como 0: {event}"
    );
    assert!(
        !event.contains("cost_usd=0"),
        "el coste desconocido nunca se registra como 0: {event}"
    );
}

/// Escenario 4.1 (streaming): el evento se emite al **agotarse el stream** con
/// el `usage` del chunk final y el `finish_reason` de los chunks; el stream
/// llega íntegro al cliente.
#[tokio::test]
async fn request_event_for_stream_emits_usage_and_finish_reason_at_stream_end() {
    ensure_api_key();
    let mock = spawn_paid_mock(CHAT_SSE, "text/event-stream").await;
    let gateway = spawn_gateway(&paid_config(&mock)).await;
    let (captured, _guard) = capture_ledger_logs();

    let response = post_chat(&gateway, "req-ledger-stream", true).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("stream completo");
    assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
    assert!(body.contains("[DONE]"), "{body}");

    let event = find_request_event(&captured, "req-ledger-stream");
    assert!(event.contains("streaming=true"), "{event}");
    assert!(event.contains("outcome=success"), "{event}");
    assert!(event.contains("finish_reason=stop"), "{event}");
    assert!(event.contains("tokens_in=1000"), "{event}");
    assert!(event.contains("tokens_out=200"), "{event}");
    assert!(event.contains("pin=2"), "{event}");
    assert!(event.contains("pout=8"), "{event}");
    assert!(event.contains("cost_usd=0.0036"), "{event}");
    assert!(event.contains("attempts=1"), "{event}");
    assert!(event.contains("failover=false"), "{event}");
}

/// Escenario 4.1 (`attempts`/`failover`): el primer backend muerto fuerza el
/// failover; el evento registra `attempts=2`, `failover=true` y el modelo y
/// precio del backend que finalmente atendió la petición.
#[tokio::test]
async fn request_event_records_attempts_and_failover_across_backends() {
    ensure_api_key();
    let mock = spawn_paid_mock(CHAT_JSON, "application/json").await;
    let gateway = spawn_gateway(&failover_config("http://127.0.0.1:1/v1", &mock)).await;
    let (captured, _guard) = capture_ledger_logs();

    let response = post_chat(&gateway, "req-ledger-failover", false).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let event = find_request_event(&captured, "req-ledger-failover");
    assert!(event.contains("outcome=success"), "{event}");
    assert!(event.contains("attempts=2"), "{event}");
    assert!(event.contains("failover=true"), "{event}");
    assert!(event.contains("model_id=acme/flash-a"), "{event}");
    assert!(event.contains("tokens_in=1000"), "{event}");
    assert!(event.contains("cost_usd=0.0036"), "{event}");
}

/// Hueco de cobertura (streaming sin `usage`): el stream sin chunk final de
/// `usage` emite el evento con tokens y coste desconocidos —el campo no se
/// registra, nunca `0`— y el `finish_reason` de los chunks.
#[tokio::test]
async fn request_event_for_stream_without_usage_keeps_tokens_unknown() {
    ensure_api_key();
    let mock = spawn_paid_mock(CHAT_SSE_NO_USAGE, "text/event-stream").await;
    let gateway = spawn_gateway(&paid_config(&mock)).await;
    let (captured, _guard) = capture_ledger_logs();

    let response = post_chat(&gateway, "req-ledger-stream-no-usage", true).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("stream completo");
    assert!(body.contains("[DONE]"), "{body}");

    let event = find_request_event(&captured, "req-ledger-stream-no-usage");
    assert!(event.contains("streaming=true"), "{event}");
    assert!(event.contains("outcome=success"), "{event}");
    assert!(event.contains("finish_reason=stop"), "{event}");
    assert!(
        !event.contains("tokens_in="),
        "sin usage el token no se registra (nunca 0): {event}"
    );
    assert!(
        !event.contains("tokens_out="),
        "sin usage el token no se registra (nunca 0): {event}"
    );
    assert!(
        !event.contains("cost_usd="),
        "sin usage el coste no se registra (nunca 0): {event}"
    );
}

/// Hueco de cobertura (streaming + failover): el primer backend muerto fuerza
/// el failover en una petición streaming; el evento se emite al agotarse el
/// stream con `attempts=2`, `failover=true` y el `usage` del backend final.
#[tokio::test]
async fn request_event_for_stream_records_attempts_and_failover() {
    ensure_api_key();
    let mock = spawn_paid_mock(CHAT_SSE, "text/event-stream").await;
    let gateway = spawn_gateway(&failover_config("http://127.0.0.1:1/v1", &mock)).await;
    let (captured, _guard) = capture_ledger_logs();

    let response = post_chat(&gateway, "req-ledger-stream-failover", true).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("stream completo");
    assert!(body.contains("[DONE]"), "{body}");

    let event = find_request_event(&captured, "req-ledger-stream-failover");
    assert!(event.contains("streaming=true"), "{event}");
    assert!(event.contains("outcome=success"), "{event}");
    assert!(event.contains("attempts=2"), "{event}");
    assert!(event.contains("failover=true"), "{event}");
    assert!(event.contains("model_id=acme/flash-a"), "{event}");
    assert!(event.contains("finish_reason=stop"), "{event}");
    assert!(event.contains("tokens_in=1000"), "{event}");
    assert!(event.contains("tokens_out=200"), "{event}");
    assert!(event.contains("cost_usd=0.0036"), "{event}");
}

/// La petición que agota la cadena de backends también emite su evento, con
/// `outcome=all_failed`, el modelo del último intento y tokens desconocidos.
#[tokio::test]
async fn request_event_emitted_when_all_backends_fail() {
    ensure_api_key();
    let gateway = spawn_gateway(&all_failed_config("http://127.0.0.1:1/v1")).await;
    let (captured, _guard) = capture_ledger_logs();

    let response = post_chat(&gateway, "req-ledger-failed", false).await;
    assert!(
        !response.status().is_success(),
        "sin backends la petición falla: {}",
        response.status()
    );

    let event = find_request_event(&captured, "req-ledger-failed");
    assert!(event.contains("event_type=request"), "{event}");
    assert!(event.contains("route=code"), "{event}");
    assert!(event.contains("model_id=local/dead"), "{event}");
    assert!(event.contains("outcome=all_failed"), "{event}");
    assert!(event.contains("attempts=1"), "{event}");
    assert!(event.contains("failover=false"), "{event}");
    assert!(
        !event.contains("tokens_in=0") && !event.contains("tokens_out=0"),
        "sin respuesta no hay tokens que registrar (nunca 0): {event}"
    );
    assert!(
        !event.contains("cost_usd=0"),
        "sin respuesta no hay coste que registrar (nunca 0): {event}"
    );
}
