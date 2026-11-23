//! Tests de integración del gate global de cuota diaria free de OpenRouter.
//!
//! Se levanta un mock que sirve `GET /v1/models` (catálogo), `POST
//! /v1/chat/completions` (comportamiento por modelo) y `GET /v1/key` (cuota
//! diaria free), más el gateway real sobre un puerto efímero.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use llm_gateway_rs::config::GatewayConfig;
use llm_gateway_rs::proxy::{AppState, router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Variable de entorno con la API key de prueba.
const KEY_ENV: &str = "OPENROUTER_QUOTA_GATE_TEST_KEY";

/// Respuesta configurada para `POST /v1/chat/completions` de un modelo.
#[derive(Clone)]
struct ChatBehaviour {
    status: StatusCode,
    body: Value,
    /// Cabeceras extra (`X-RateLimit-*`).
    headers: Vec<(String, String)>,
}

impl ChatBehaviour {
    fn ok() -> Self {
        Self {
            status: StatusCode::OK,
            body: json!({"choices": [{"message": {"role": "assistant", "content": "hola"}}]}),
            headers: Vec::new(),
        }
    }

    fn rate_limited(body: Value, headers: &[(&str, String)]) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            body,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
        }
    }
}

/// Respuesta configurada para `GET /v1/key`.
#[derive(Clone)]
struct KeyBehaviour {
    status: StatusCode,
    body: Value,
}

impl KeyBehaviour {
    fn remaining(remaining: u64) -> Self {
        Self {
            status: StatusCode::OK,
            body: json!({"data": {"free_model_daily_requests": {"used": 750, "limit": 1000, "remaining": remaining}}}),
        }
    }

    fn failing() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: json!({"error": "boom"}),
        }
    }
}

/// Estado del mock.
struct Mock {
    catalog: Value,
    key: Mutex<KeyBehaviour>,
    key_requests: AtomicUsize,
    behaviours: Mutex<HashMap<String, ChatBehaviour>>,
    counts: Mutex<HashMap<String, usize>>,
}

impl Mock {
    fn new(catalog: Value) -> Self {
        Self {
            catalog,
            key: Mutex::new(KeyBehaviour::remaining(1000)),
            key_requests: AtomicUsize::new(0),
            behaviours: Mutex::new(HashMap::new()),
            counts: Mutex::new(HashMap::new()),
        }
    }

    fn set(&self, model: &str, behaviour: ChatBehaviour) {
        self.behaviours
            .lock()
            .expect("lock behaviours")
            .insert(model.to_owned(), behaviour);
    }

    fn set_key(&self, behaviour: KeyBehaviour) {
        *self.key.lock().expect("lock key") = behaviour;
    }

    fn key_requests(&self) -> usize {
        self.key_requests.load(Ordering::SeqCst)
    }

    fn count(&self, model: &str) -> usize {
        *self
            .counts
            .lock()
            .expect("lock counts")
            .get(model)
            .unwrap_or(&0)
    }

    fn behaviour(&self, model: &str) -> ChatBehaviour {
        *self
            .counts
            .lock()
            .expect("lock counts")
            .entry(model.to_owned())
            .or_insert(0) += 1;
        self.behaviours
            .lock()
            .expect("lock behaviours")
            .get(model)
            .cloned()
            .unwrap_or_else(ChatBehaviour::ok)
    }
}

async fn models_handler(State(mock): State<Arc<Mock>>) -> Response {
    axum::Json(mock.catalog.clone()).into_response()
}

async fn key_handler(State(mock): State<Arc<Mock>>) -> Response {
    mock.key_requests.fetch_add(1, Ordering::SeqCst);
    let behaviour = mock.key.lock().expect("lock key").clone();
    (behaviour.status, axum::Json(behaviour.body)).into_response()
}

async fn chat_handler(State(mock): State<Arc<Mock>>, body: Bytes) -> Response {
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let behaviour = mock.behaviour(&model);

    let mut headers = HeaderMap::new();
    for (name, value) in &behaviour.headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }

    (behaviour.status, headers, axum::Json(behaviour.body)).into_response()
}

/// Levanta el mock y devuelve su `base_url` (incluye `/v1`).
async fn spawn_openrouter(catalog: Value) -> (String, Arc<Mock>) {
    let mock = Arc::new(Mock::new(catalog));
    let app = Router::new()
        .route("/v1/models", get(models_handler))
        .route("/v1/key", get(key_handler))
        .route("/v1/chat/completions", post(chat_handler))
        .with_state(Arc::clone(&mock));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock");
    });
    (format!("http://{addr}/v1"), mock)
}

/// Fija la API key de prueba una sola vez por proceso.
fn ensure_test_api_key() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        // SAFETY: escritura única por proceso antes de construir cualquier
        // `AppState`; el resto de tests pasa antes por `ensure_test_api_key`.
        unsafe { std::env::set_var(KEY_ENV, "test-key") };
    });
}

/// Gateway ya montado, con los handles del sondeo de cuota.
struct Gateway {
    url: String,
    _poll: Vec<tokio::task::JoinHandle<()>>,
}

/// Levanta el gateway con la `yaml` dada (`{base}` y `{key_env}` sustituidos) y
/// arranca el sondeo de cuota.
async fn spawn_gateway(yaml: &str, base: &str) -> Gateway {
    spawn_gateway_subs(yaml, &[("{base}", base)]).await
}

/// Levanta el gateway sustituyendo cada `{placeholder}` por su valor y arranca
/// el sondeo de cuota.
async fn spawn_gateway_subs(yaml: &str, subs: &[(&str, &str)]) -> Gateway {
    ensure_test_api_key();
    let mut yaml = yaml.replace("{key_env}", KEY_ENV);
    for (placeholder, value) in subs {
        yaml = yaml.replace(placeholder, value);
    }
    let config: GatewayConfig = serde_saphyr::from_str(&yaml).expect("config de test válida");
    let state = AppState::new(config).expect("construir AppState");
    let poll = state.discovery.spawn_quota_poll();
    let app = router(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve gateway");
    });
    Gateway {
        url: format!("http://{addr}"),
        _poll: poll,
    }
}

async fn chat(gateway: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("request al gateway")
}

async fn metrics(gateway: &str) -> String {
    reqwest::get(format!("{gateway}/metrics"))
        .await
        .expect("GET /metrics")
        .text()
        .await
        .expect("cuerpo de /metrics")
}

fn backend_header(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("x-gateway-backend")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// Catálogo con un único modelo free elegible.
fn one_model_catalog() -> Value {
    json!({"data": [
        {"id": "dyn/aa:free", "name": "AA", "context_length": 1000,
         "supported_parameters": ["tools", "tool_choice"], "pricing": {"prompt": "0", "completion": "0"}}
    ]})
}

/// Catálogo con un único modelo free elegible con el `id` dado.
fn single_free_catalog(id: &str) -> Value {
    json!({"data": [
        {"id": id, "name": id, "context_length": 1000,
         "supported_parameters": ["tools", "tool_choice"], "pricing": {"prompt": "0", "completion": "0"}}
    ]})
}

/// Catálogo con dos modelos free elegibles (mismo tier y contexto → orden por id).
fn two_model_catalog() -> Value {
    json!({"data": [
        {"id": "dyn/aa:free", "name": "AA", "context_length": 1000,
         "supported_parameters": ["tools", "tool_choice"], "pricing": {"prompt": "0", "completion": "0"}},
        {"id": "dyn/bb:free", "name": "BB", "context_length": 1000,
         "supported_parameters": ["tools", "tool_choice"], "pricing": {"prompt": "0", "completion": "0"}}
    ]})
}

/// Milisegundos epoch UTC dentro de `delta`.
fn epoch_ms_in(delta: Duration) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("reloj posterior a 1970");
    u64::try_from((now + delta).as_millis()).expect("epoch en ms cabe en u64")
}

/// `true` si la serie `name` con la etiqueta `key=value` aparece en `text`.
fn has_series(text: &str, name: &str, key: &str, value: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with(name) && line.contains(&format!(r#"{key}="{value}""#)))
}

/// Valor de la serie `name` con la etiqueta `key=value`, si existe.
fn series_value(text: &str, name: &str, key: &str, value: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.starts_with(name) && line.contains(&format!(r#"{key}="{value}""#)))
        .and_then(|line| line.rsplit_once(' '))
        .and_then(|(_, value)| value.trim().parse().ok())
}

/// Espera hasta que `condition` se cumpla (con un tope de ~2 s).
async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(condition(), "la condición no se cumplió en el plazo");
}

/// Espera hasta que el gauge de cuota se haya observado para `base`.
///
/// El sondeo registra el gauge y abre el gate sin `await` intermedio, así que
/// observar la serie garantiza que el gate ya está abierto.
async fn wait_for_gauge(gateway: &str, base: &str) {
    for _ in 0..200 {
        let text = metrics(gateway).await;
        if has_series(&text, "llm_gateway_free_quota_remaining", "base_url", base) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("el gauge de cuota no se observó en el plazo");
}

/// Scenario: 429 de cuota diaria abre el gate y las siguientes peticiones usan
/// el backend de pago sin ningún intento free.
#[tokio::test]
async fn daily_429_opens_gate_and_skips_free() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    // El sondeo no debe reabrir/cerrar el gate: falla y conserva el estado, de
    // modo que el gate quede en manos del `429` diario.
    mock.set_key(KeyBehaviour::failing());
    mock.set(
        "dyn/aa:free",
        ChatBehaviour::rate_limited(
            json!({"error": {"message": "quota", "metadata": {"limit_source": "openrouter_free_tier_daily"}}}),
            &[
                ("x-ratelimit-remaining", "0".to_owned()),
                (
                    "x-ratelimit-reset",
                    epoch_ms_in(Duration::from_secs(6 * 3600)).to_string(),
                ),
            ],
        ),
    );

    let yaml = r#"
route:
  - name: quota-daily-gate
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
      - base_url: "{base}"
        model: "paid/model"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    let first = chat(&gateway.url, "quota-daily-gate").await;
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    assert!(
        backend_header(&first).contains("paid/model"),
        "el 429 diario debe caer al backend de pago, got: {}",
        backend_header(&first)
    );

    let second = chat(&gateway.url, "quota-daily-gate").await;
    assert_eq!(second.status(), reqwest::StatusCode::OK);
    assert!(
        backend_header(&second).contains("paid/model"),
        "con el gate abierto no debe proponerse ningún free, got: {}",
        backend_header(&second)
    );
    assert_eq!(
        mock.count("dyn/aa:free"),
        1,
        "el modelo free solo debe intentarse una vez"
    );

    let text = metrics(&gateway.url).await;
    assert_eq!(
        series_value(
            &text,
            "llm_gateway_free_quota_skips_total",
            "route",
            "quota-daily-gate"
        ),
        Some(1),
        "la segunda petición debe contar un salto del tier free:\n{text}"
    );
}

/// Scenario: 429 de minuto no abre el gate (sigue sirviendo free).
#[tokio::test]
async fn minute_429_does_not_open_gate() {
    let (base, mock) = spawn_openrouter(two_model_catalog()).await;
    mock.set(
        "dyn/aa:free",
        ChatBehaviour::rate_limited(
            json!({"error": {"metadata": {"limit_source": "openrouter_free_tier_per_minute"}}}),
            &[("x-ratelimit-remaining", "0".to_owned())],
        ),
    );

    let yaml = r#"
route:
  - name: quota-minute-gate
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
      - base_url: "{base}"
        model: "paid/model"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    let first = chat(&gateway.url, "quota-minute-gate").await;
    assert!(
        backend_header(&first).contains("dyn/bb:free"),
        "el 429 de minuto solo blacklistea su modelo, got: {}",
        backend_header(&first)
    );

    let second = chat(&gateway.url, "quota-minute-gate").await;
    assert!(
        backend_header(&second).contains("dyn/bb:free"),
        "el gate debe seguir cerrado, got: {}",
        backend_header(&second)
    );
    assert_eq!(
        mock.count("paid/model"),
        0,
        "con el gate cerrado no debe contactarse el backend de pago"
    );
}

/// Scenario: 429 de proveedor no abre el gate.
#[tokio::test]
async fn provider_429_does_not_open_gate() {
    let (base, mock) = spawn_openrouter(two_model_catalog()).await;
    mock.set(
        "dyn/aa:free",
        ChatBehaviour::rate_limited(json!({"error": {"metadata": {"provider_code": 429}}}), &[]),
    );

    let yaml = r#"
route:
  - name: quota-provider-gate
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
      - base_url: "{base}"
        model: "paid/model"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    let first = chat(&gateway.url, "quota-provider-gate").await;
    assert!(
        backend_header(&first).contains("dyn/bb:free"),
        "un 429 de proveedor no debe abrir el gate, got: {}",
        backend_header(&first)
    );
    let second = chat(&gateway.url, "quota-provider-gate").await;
    assert_eq!(
        mock.count("paid/model"),
        0,
        "el gate debe permanecer cerrado, got: {}",
        backend_header(&second)
    );
}

/// Scenario: `respect_daily_quota: false` desactiva el gate.
#[tokio::test]
async fn respect_daily_quota_false_disables_gate() {
    let (base, mock) = spawn_openrouter(two_model_catalog()).await;
    mock.set(
        "dyn/aa:free",
        ChatBehaviour::rate_limited(
            json!({"error": {"metadata": {"limit_source": "openrouter_free_tier_daily"}}}),
            &[
                ("x-ratelimit-remaining", "0".to_owned()),
                (
                    "x-ratelimit-reset",
                    epoch_ms_in(Duration::from_secs(6 * 3600)).to_string(),
                ),
            ],
        ),
    );

    let yaml = r#"
route:
  - name: quota-disabled
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          respect_daily_quota: false
      - base_url: "{base}"
        model: "paid/model"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    let first = chat(&gateway.url, "quota-disabled").await;
    assert!(
        backend_header(&first).contains("dyn/bb:free"),
        "sin gate debe seguir sirviendo free, got: {}",
        backend_header(&first)
    );
    let second = chat(&gateway.url, "quota-disabled").await;
    assert!(
        backend_header(&second).contains("dyn/bb:free"),
        "el gate está desactivado, got: {}",
        backend_header(&second)
    );
    assert_eq!(
        mock.count("paid/model"),
        0,
        "no debe contactarse el backend de pago"
    );
}

/// Scenario: el sondeo proactivo abre el gate sin 429.
#[tokio::test]
async fn proactive_poll_opens_gate() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    mock.set_key(KeyBehaviour::remaining(0));

    let yaml = r#"
route:
  - name: quota-poll-open
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          quota_poll_interval_ms: 50
      - base_url: "{base}"
        model: "paid/model"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    wait_for_gauge(&gateway.url, &base).await;
    let response = chat(&gateway.url, "quota-poll-open").await;
    assert!(
        backend_header(&response).contains("paid/model"),
        "el sondeo debe haber abierto el gate, got: {}",
        backend_header(&response)
    );
    assert_eq!(
        mock.count("dyn/aa:free"),
        0,
        "no debe emitirse ningún intento free tras el sondeo"
    );
}

/// Scenario: sondeo periódico con `remaining > 0` mantiene el gate cerrado.
#[tokio::test]
async fn periodic_poll_keeps_gate_closed() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    mock.set_key(KeyBehaviour::remaining(500));

    let yaml = r#"
route:
  - name: quota-poll-periodic
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          quota_poll_interval_ms: 50
      - base_url: "{base}"
        model: "paid/model"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    wait_until(|| mock.key_requests() >= 2).await;
    assert!(
        mock.key_requests() >= 2,
        "el sondeo debe repetirse: {} llamadas",
        mock.key_requests()
    );

    let response = chat(&gateway.url, "quota-poll-periodic").await;
    assert!(
        backend_header(&response).contains("dyn/aa:free"),
        "con cuota disponible debe seguir sirviendo free, got: {}",
        backend_header(&response)
    );

    wait_for_gauge(&gateway.url, &base).await;
    let text = metrics(&gateway.url).await;
    assert!(
        has_series(&text, "llm_gateway_free_quota_remaining", "base_url", &base),
        "el gauge debe reflejar la cuota observada:\n{text}"
    );
}

/// Scenario: un fallo del sondeo conserva el estado del gate.
#[tokio::test]
async fn poll_failure_keeps_state() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    mock.set_key(KeyBehaviour::failing());

    let yaml = r#"
route:
  - name: quota-poll-failure
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          quota_poll_interval_ms: 50
      - base_url: "{base}"
        model: "paid/model"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    wait_until(|| mock.key_requests() >= 2).await;
    let response = chat(&gateway.url, "quota-poll-failure").await;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "un fallo del sondeo no debe afectar al cliente"
    );
    assert!(
        backend_header(&response).contains("dyn/aa:free"),
        "el gate debe seguir cerrado, got: {}",
        backend_header(&response)
    );
}

/// Scenario: una sola consulta a `/key` por `base_url` y ciclo.
#[tokio::test]
async fn one_key_call_per_base_url() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    mock.set_key(KeyBehaviour::remaining(500));

    let yaml = r#"
route:
  - name: quota-shared-a
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          quota_poll_interval_ms: 60000
  - name: quota-shared-b
    backend:
      - kind: openrouter_free
        profile: writer
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          quota_poll_interval_ms: 60000
  - name: quota-shared-c
    backend:
      - kind: openrouter_free
        profile: architect
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          quota_poll_interval_ms: 60000
"#;
    let _gateway = spawn_gateway(yaml, &base).await;

    wait_until(|| mock.key_requests() >= 1).await;
    // Margen para descartar llamadas duplicadas del mismo ciclo.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        mock.key_requests(),
        1,
        "tres backends con la misma base_url comparten una única llamada a /key"
    );
}

/// Scenario: sin sondeo ni observación no aparece la serie del gauge.
#[tokio::test]
async fn no_gauge_without_observation() {
    let (base, _mock) = spawn_openrouter(one_model_catalog()).await;

    let yaml = r#"
route:
  - name: quota-no-gauge
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          respect_daily_quota: false
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    let text = metrics(&gateway.url).await;
    assert!(
        !has_series(&text, "llm_gateway_free_quota_remaining", "base_url", &base),
        "sin observación no debe emitirse el gauge:\n{text}"
    );
}

/// Catálogo mixto: un modelo free elegible, dos de pago curados (uno más barato
/// que el otro) y un modelo de pago barato fuera de la lista curada.
fn mixed_catalog() -> Value {
    let paid = |id: &str, prompt: &str, completion: &str| {
        json!({
            "id": id,
            "name": id,
            "context_length": 131_072,
            "supported_parameters": ["tools", "tool_choice"],
            "pricing": {"prompt": prompt, "completion": completion}
        })
    };
    json!({"data": [
        {"id": "dyn/aa:free", "name": "AA", "context_length": 1000,
         "supported_parameters": ["tools", "tool_choice"], "pricing": {"prompt": "0", "completion": "0"}},
        paid("z-ai/glm-5.3-flash", "0.0625", "0.03125"),
        paid("qwen/qwen3.7-flash", "0.25", "0.125"),
        paid("cheap/not-allowed", "0.001", "0.001")
    ]})
}

/// Scenario 6.3: con la cuota agotada simulada, la ruta `code` responde desde el
/// backend de pago sin ningún intento free y con el modelo más barato de la
/// lista curada; el contador de saltos del tier free incrementa.
#[tokio::test]
async fn exhausted_quota_serves_cheapest_curated_paid_model() {
    let (base, mock) = spawn_openrouter(mixed_catalog()).await;
    mock.set_key(KeyBehaviour::remaining(0));

    let yaml = r#"
route:
  - name: code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          quota_poll_interval_ms: 50
      - kind: openrouter_paid
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          min_context: 32768
          allow:
            - "deepseek/deepseek-v4.1-flash*"
            - "z-ai/glm-5.*-flash"
            - "qwen/qwen3.7-flash"
            - "tencent/hy4-flash"
            - "inclusionai/ling-3.1-flash"
            - "nvidia/nemotron-3.5-lightning"
            - "minimax/minimax-m3.*-flash"
      - base_url: "{base}"
        model: "static/final-resort"
        api_key_env: "{key_env}"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    wait_for_gauge(&gateway.url, &base).await;
    let response = chat(&gateway.url, "code").await;

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let backend = backend_header(&response);
    assert!(
        backend.contains("z-ai/glm-5.3-flash"),
        "debe servirse el modelo curado más barato, got: {backend}"
    );
    assert!(
        !backend.contains("static/final-resort"),
        "no debe caer al `static` final: {backend}"
    );
    assert_eq!(
        mock.count("dyn/aa:free"),
        0,
        "con el gate abierto no debe emitirse ningún intento free"
    );
    assert_eq!(
        mock.count("cheap/not-allowed"),
        0,
        "un modelo fuera de la lista curada nunca se propone"
    );

    let text = metrics(&gateway.url).await;
    assert_eq!(
        series_value(&text, "llm_gateway_free_quota_skips_total", "route", "code"),
        Some(1),
        "el salto del tier free debe contarse una vez:\n{text}"
    );
}

/// Cabeceras de un `429` diario con reset lejano.
fn daily_headers() -> Vec<(&'static str, String)> {
    vec![
        ("x-ratelimit-remaining", "0".to_owned()),
        (
            "x-ratelimit-reset",
            epoch_ms_in(Duration::from_secs(6 * 3600)).to_string(),
        ),
    ]
}

/// Scenario 1: un `429` diario de un backend free con `respect_daily_quota:
/// false` NO abre el gate, ni siquiera cuando otro backend free de la misma
/// ruta sí lo respeta.
#[tokio::test]
async fn daily_429_from_non_respecting_backend_does_not_open_gate() {
    let (base_a, mock_a) = spawn_openrouter(single_free_catalog("dyn/aa:free")).await;
    mock_a.set(
        "dyn/aa:free",
        ChatBehaviour::rate_limited(
            json!({"error": {"metadata": {"limit_source": "openrouter_free_tier_daily"}}}),
            &daily_headers(),
        ),
    );
    let (base_b, mock_b) = spawn_openrouter(single_free_catalog("dyn/bb:free")).await;
    // El sondeo del backend que sí respeta no debe alterar el gate.
    mock_b.set_key(KeyBehaviour::failing());

    let yaml = r#"
route:
  - name: quota-mixed-respect
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base_a}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          respect_daily_quota: false
      - kind: openrouter_free
        profile: code
        base_url: "{base_b}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
      - base_url: "{base_b}"
        model: "paid/model"
        api_key_env: "{key_env}"
"#;
    let gateway = spawn_gateway_subs(yaml, &[("{base_a}", &base_a), ("{base_b}", &base_b)]).await;

    let first = chat(&gateway.url, "quota-mixed-respect").await;
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    assert!(
        backend_header(&first).contains("dyn/bb:free"),
        "el 429 del backend que no respeta no abre el gate, got: {}",
        backend_header(&first)
    );

    let second = chat(&gateway.url, "quota-mixed-respect").await;
    assert!(
        backend_header(&second).contains("dyn/bb:free"),
        "el gate no debe abrirse: la segunda petición se sirve free, got: {}",
        backend_header(&second)
    );
    assert_eq!(
        mock_b.count("paid/model"),
        0,
        "no debe contactarse el respaldo de pago"
    );
}

/// Scenario: un `429` diario sin `X-RateLimit-Reset` lleva el gate hasta el
/// próximo `00:00 UTC` (vía `detect_daily_quota`, no solo el unit del deadline).
#[tokio::test]
async fn daily_429_without_reset_falls_back_to_utc_midnight() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    mock.set_key(KeyBehaviour::failing());
    mock.set(
        "dyn/aa:free",
        ChatBehaviour::rate_limited(
            json!({"error": {"metadata": {"limit_source": "openrouter_free_tier_daily"}}}),
            &[],
        ),
    );

    let yaml = r#"
route:
  - name: quota-no-reset
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
      - base_url: "{base}"
        model: "paid/model"
        api_key_env: "{key_env}"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    let first = chat(&gateway.url, "quota-no-reset").await;
    assert!(
        backend_header(&first).contains("paid/model"),
        "sin reset, el 429 diario abre el gate hasta 00:00 UTC, got: {}",
        backend_header(&first)
    );
    let second = chat(&gateway.url, "quota-no-reset").await;
    assert!(
        backend_header(&second).contains("paid/model"),
        "el gate sigue abierto sin cabecera de reset, got: {}",
        backend_header(&second)
    );
}

/// Scenario: heurística end-to-end — un `429` sin `limit_source` pero con
/// `X-RateLimit-Remaining: 0` y reset lejano abre el gate.
#[tokio::test]
async fn remaining_zero_with_far_reset_opens_gate_via_heuristic() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    mock.set_key(KeyBehaviour::failing());
    mock.set(
        "dyn/aa:free",
        ChatBehaviour::rate_limited(json!({"error": {"message": "slow down"}}), &daily_headers()),
    );

    let yaml = r#"
route:
  - name: quota-heuristic
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
      - base_url: "{base}"
        model: "paid/model"
        api_key_env: "{key_env}"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    let first = chat(&gateway.url, "quota-heuristic").await;
    assert!(
        backend_header(&first).contains("paid/model"),
        "la heurística de cabeceras debe abrir el gate, got: {}",
        backend_header(&first)
    );
    let second = chat(&gateway.url, "quota-heuristic").await;
    assert!(
        backend_header(&second).contains("paid/model"),
        "el gate sigue abierto, got: {}",
        backend_header(&second)
    );
}

/// Scenario: el gate es global — abrirlo desde una ruta hace que otra ruta con
/// `base_url` distinta deje de proponer modelos free.
#[tokio::test]
async fn gate_is_global_across_routes_and_base_urls() {
    let (base_a, mock_a) = spawn_openrouter(single_free_catalog("dyn/aa:free")).await;
    mock_a.set_key(KeyBehaviour::failing());
    mock_a.set(
        "dyn/aa:free",
        ChatBehaviour::rate_limited(
            json!({"error": {"metadata": {"limit_source": "openrouter_free_tier_daily"}}}),
            &daily_headers(),
        ),
    );
    let (base_b, mock_b) = spawn_openrouter(single_free_catalog("dyn/bb:free")).await;
    mock_b.set_key(KeyBehaviour::failing());

    let yaml = r#"
route:
  - name: quota-global-a
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base_a}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
      - base_url: "{base_a}"
        model: "paid/model"
        api_key_env: "{key_env}"
  - name: quota-global-b
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base_b}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
      - base_url: "{base_b}"
        model: "paid/model"
        api_key_env: "{key_env}"
"#;
    let gateway = spawn_gateway_subs(yaml, &[("{base_a}", &base_a), ("{base_b}", &base_b)]).await;

    let first = chat(&gateway.url, "quota-global-a").await;
    assert!(
        backend_header(&first).contains("paid/model"),
        "el 429 diario de la ruta A abre el gate, got: {}",
        backend_header(&first)
    );

    let second = chat(&gateway.url, "quota-global-b").await;
    assert!(
        backend_header(&second).contains("paid/model"),
        "la ruta B debe saltar el free con el gate global abierto, got: {}",
        backend_header(&second)
    );
    assert_eq!(
        mock_b.count("dyn/bb:free"),
        0,
        "la ruta B no debe intentar ningún modelo free"
    );
}

/// Scenario: un sondeo que observa `remaining > 0` cierra el gate abierto.
#[tokio::test]
async fn poll_with_remaining_closes_gate() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    mock.set_key(KeyBehaviour::remaining(0));

    let yaml = r#"
route:
  - name: quota-poll-close
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          quota_poll_interval_ms: 50
      - base_url: "{base}"
        model: "paid/model"
        api_key_env: "{key_env}"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    wait_for_gauge(&gateway.url, &base).await;
    let skipped = chat(&gateway.url, "quota-poll-close").await;
    assert!(
        backend_header(&skipped).contains("paid/model"),
        "el sondeo con remaining=0 debe haber abierto el gate, got: {}",
        backend_header(&skipped)
    );

    // El sondeo vuelve a observar cuota disponible: debe cerrar el gate.
    mock.set_key(KeyBehaviour::remaining(100));
    let mut served_free = false;
    for _ in 0..300 {
        let response = chat(&gateway.url, "quota-poll-close").await;
        if backend_header(&response).contains("dyn/aa:free") {
            served_free = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        served_free,
        "un sondeo con remaining > 0 debe cerrar el gate y volver a servir free"
    );
}

/// Scenario: dos backends free en la misma ruta cuentan un solo salto.
#[tokio::test]
async fn two_free_backends_count_one_skip() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    mock.set_key(KeyBehaviour::remaining(0));

    let yaml = r#"
route:
  - name: quota-two-free
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          quota_poll_interval_ms: 50
      - kind: openrouter_free
        profile: writer
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
          quota_poll_interval_ms: 50
      - base_url: "{base}"
        model: "paid/model"
        api_key_env: "{key_env}"
"#;
    let gateway = spawn_gateway(yaml, &base).await;

    wait_for_gauge(&gateway.url, &base).await;
    let response = chat(&gateway.url, "quota-two-free").await;
    assert!(
        backend_header(&response).contains("paid/model"),
        "con el gate abierto la ruta cae al respaldo de pago, got: {}",
        backend_header(&response)
    );
    assert_eq!(
        mock.count("dyn/aa:free"),
        0,
        "no debe emitirse ningún intento free"
    );

    let text = metrics(&gateway.url).await;
    assert_eq!(
        series_value(
            &text,
            "llm_gateway_free_quota_skips_total",
            "route",
            "quota-two-free"
        ),
        Some(1),
        "dos backends free en la misma ruta cuentan un único salto:\n{text}"
    );
}
