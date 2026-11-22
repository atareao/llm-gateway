//! Tests de integración del enrutado dinámico `openrouter_free`.
//!
//! Se levanta un mock de OpenRouter que sirve a la vez `GET /v1/models` (catálogo)
//! y `POST /v1/chat/completions` (comportamiento por modelo), y el gateway real
//! sobre un puerto efímero.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use llm_gateway_rs::config::GatewayConfig;
use llm_gateway_rs::proxy::{AppState, router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Variable de entorno con la API key de prueba.
const KEY_ENV: &str = "OPENROUTER_FREE_ROUTING_TEST_KEY";

/// Comportamiento de un modelo en el mock de chat.
#[derive(Clone)]
struct Behaviour {
    status: StatusCode,
    retry_after: Option<String>,
}

impl Behaviour {
    fn status(status: StatusCode) -> Self {
        Self {
            status,
            retry_after: None,
        }
    }
}

/// Estado del mock de OpenRouter: catálogo + comportamiento y contadores por modelo.
struct OpenRouterMock {
    catalog: Value,
    behaviours: Mutex<HashMap<String, Behaviour>>,
    counts: Mutex<HashMap<String, usize>>,
    default: Behaviour,
}

impl OpenRouterMock {
    fn new(catalog: Value) -> Self {
        Self {
            catalog,
            behaviours: Mutex::new(HashMap::new()),
            counts: Mutex::new(HashMap::new()),
            default: Behaviour::status(StatusCode::OK),
        }
    }

    fn set(&self, model: &str, status: StatusCode) {
        self.behaviours
            .lock()
            .expect("lock behaviours")
            .insert(model.to_owned(), Behaviour::status(status));
    }

    fn set_retry_after(&self, model: &str, status: StatusCode, seconds: u64) {
        self.behaviours.lock().expect("lock behaviours").insert(
            model.to_owned(),
            Behaviour {
                status,
                retry_after: Some(seconds.to_string()),
            },
        );
    }

    /// Fija un `Retry-After` con un valor literal (p. ej. una fecha HTTP).
    fn set_retry_after_value(&self, model: &str, status: StatusCode, value: &str) {
        self.behaviours.lock().expect("lock behaviours").insert(
            model.to_owned(),
            Behaviour {
                status,
                retry_after: Some(value.to_owned()),
            },
        );
    }

    fn count(&self, model: &str) -> usize {
        *self
            .counts
            .lock()
            .expect("lock counts")
            .get(model)
            .unwrap_or(&0)
    }

    fn behaviour(&self, model: &str) -> Behaviour {
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
            .unwrap_or_else(|| self.default.clone())
    }
}

async fn models_handler(State(mock): State<Arc<OpenRouterMock>>) -> axum::Json<Value> {
    axum::Json(mock.catalog.clone())
}

async fn chat_handler(State(mock): State<Arc<OpenRouterMock>>, body: Bytes) -> Response {
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
    if let Some(value) = &behaviour.retry_after
        && let Ok(value) = HeaderValue::from_str(value)
    {
        headers.insert(header::RETRY_AFTER, value);
    }

    (
        behaviour.status,
        headers,
        axum::Json(json!({"model": model})),
    )
        .into_response()
}

/// Levanta el mock y devuelve su `base_url` (incluye `/v1`).
async fn spawn_openrouter(catalog: Value) -> (String, Arc<OpenRouterMock>) {
    let mock = Arc::new(OpenRouterMock::new(catalog));
    let app = Router::new()
        .route("/v1/models", get(models_handler))
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

/// Catálogo con dos modelos free elegibles (mismo tier y contexto → orden por id).
fn two_model_catalog() -> Value {
    json!({"data": [
        {"id": "dyn/aa:free", "name": "AA", "context_length": 1000,
         "supported_parameters": ["tools", "tool_choice"], "pricing": {"prompt": "0", "completion": "0"}},
        {"id": "dyn/bb:free", "name": "BB", "context_length": 1000,
         "supported_parameters": ["tools", "tool_choice"], "pricing": {"prompt": "0", "completion": "0"}}
    ]})
}

/// Catálogo con un único modelo free elegible.
fn one_model_catalog() -> Value {
    json!({"data": [
        {"id": "dyn/aa:free", "name": "AA", "context_length": 1000,
         "supported_parameters": ["tools", "tool_choice"], "pricing": {"prompt": "0", "completion": "0"}}
    ]})
}

/// Catálogo sin modelos elegibles (solo un modelo de pago).
fn no_eligible_catalog() -> Value {
    json!({"data": [
        {"id": "paid/only", "name": "Paid", "context_length": 1000,
         "supported_parameters": ["tools", "tool_choice"], "pricing": {"prompt": "0.1", "completion": "0.1"}}
    ]})
}

/// Levanta el gateway con la `yaml` dada (con `{base}` y `{key_env}` sustituidos).
async fn spawn_gateway(yaml: &str, base: &str, cooldown_ms: u64) -> String {
    // SAFETY: nombre de variable de test; el valor no es sensible.
    unsafe { std::env::set_var(KEY_ENV, "test-key") };
    let yaml = yaml
        .replace("{base}", base)
        .replace("{key_env}", KEY_ENV)
        .replace("{cooldown}", &cooldown_ms.to_string());
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

/// POST a `/v1/chat/completions` pidiendo la ruta `model`.
async fn chat(gateway: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("request al gateway")
}

fn backend_header(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("x-gateway-backend")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// Scenario: Expansión en posición → los modelos dinámicos aparecen entre el
/// primer y el último backend estático.
#[tokio::test]
async fn dynamic_expands_between_static_backends() {
    let (base, mock) = spawn_openrouter(two_model_catalog()).await;
    mock.set("static/first", StatusCode::INTERNAL_SERVER_ERROR);
    mock.set("static/last", StatusCode::OK);

    let yaml = r#"
route:
  - name: free-code
    backend:
      - base_url: "{base}"
        model: "static/first"
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: {cooldown}
      - base_url: "{base}"
        model: "static/last"
"#;
    let gateway = spawn_gateway(yaml, &base, 60_000).await;

    let response = chat(&gateway, "free-code").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let served = backend_header(&response);
    assert!(
        served.contains("dyn/aa:free"),
        "debe servir un modelo dinámico, got: {served}"
    );
    assert_eq!(
        mock.count("static/last"),
        0,
        "el último backend estático no debe contactarse si el dinámico responde"
    );
}

/// Scenario: Rotación round-robin → el primer modelo rota entre peticiones.
#[tokio::test]
async fn round_robin_rotates_between_requests() {
    let (base, _mock) = spawn_openrouter(two_model_catalog()).await;
    let yaml = r#"
route:
  - name: free-code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: {cooldown}
"#;
    let gateway = spawn_gateway(yaml, &base, 60_000).await;

    let first = backend_header(&chat(&gateway, "free-code").await);
    let second = backend_header(&chat(&gateway, "free-code").await);

    assert!(first.contains("dyn/aa:free"), "1ª petición: {first}");
    assert!(second.contains("dyn/bb:free"), "2ª petición: {second}");
}

/// Scenario: 429 marca el modelo en blacklist → no se reintenta en la siguiente.
#[tokio::test]
async fn a_429_blacklists_the_model() {
    let (base, mock) = spawn_openrouter(two_model_catalog()).await;
    mock.set("dyn/aa:free", StatusCode::TOO_MANY_REQUESTS);
    mock.set("dyn/bb:free", StatusCode::OK);

    let yaml = r#"
route:
  - name: free-code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: {cooldown}
"#;
    let gateway = spawn_gateway(yaml, &base, 60_000).await;

    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::OK
    );

    assert_eq!(
        mock.count("dyn/aa:free"),
        1,
        "el modelo en blacklist no debe reintentarse"
    );
    assert_eq!(mock.count("dyn/bb:free"), 2);
}

/// Scenario: 5xx marca el modelo en blacklist.
#[tokio::test]
async fn a_5xx_blacklists_the_model() {
    let (base, mock) = spawn_openrouter(two_model_catalog()).await;
    mock.set("dyn/aa:free", StatusCode::SERVICE_UNAVAILABLE);
    mock.set("dyn/bb:free", StatusCode::OK);

    let yaml = r#"
route:
  - name: free-code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: {cooldown}
"#;
    let gateway = spawn_gateway(yaml, &base, 60_000).await;

    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::OK
    );

    assert_eq!(
        mock.count("dyn/aa:free"),
        1,
        "un 503 debe marcar el modelo en blacklist"
    );
}

/// Scenario: Expiración del cooldown → el modelo vuelve a ser elegible.
#[tokio::test]
async fn blacklist_cooldown_expires() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    mock.set("dyn/aa:free", StatusCode::TOO_MANY_REQUESTS);

    let yaml = r#"
route:
  - name: free-code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 50
"#;
    let gateway = spawn_gateway(yaml, &base, 50).await;

    // 1ª petición: único modelo elegible, responde 429 y entra en blacklist.
    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::BAD_GATEWAY
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    // 2ª petición: el cooldown venció y el modelo vuelve a intentarse.
    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::BAD_GATEWAY
    );

    assert_eq!(
        mock.count("dyn/aa:free"),
        2,
        "tras expirar el cooldown el modelo vuelve a intentarse"
    );
}

/// Scenario: Retry-After respetado → el modelo queda en blacklist al menos ese tiempo.
#[tokio::test]
async fn retry_after_extends_the_cooldown() {
    let (base, mock) = spawn_openrouter(two_model_catalog()).await;
    mock.set_retry_after("dyn/aa:free", StatusCode::TOO_MANY_REQUESTS, 30);
    mock.set("dyn/bb:free", StatusCode::OK);

    // Cooldown configurado pequeño; el Retry-After (30 s) debe mandar.
    let yaml = r#"
route:
  - name: free-code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 50
"#;
    let gateway = spawn_gateway(yaml, &base, 50).await;

    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::OK
    );

    assert_eq!(
        mock.count("dyn/aa:free"),
        1,
        "con Retry-After el modelo sigue en blacklist pasados 200 ms"
    );
}

/// A1: un `Retry-After` con fecha HTTP ya pasada no fuerza la blacklist.
#[tokio::test]
async fn past_http_date_retry_after_does_not_blacklist() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    // 200 (no reintentable) + `Retry-After` en fecha pasada: no debe marcar.
    mock.set_retry_after_value(
        "dyn/aa:free",
        StatusCode::OK,
        "Wed, 21 Oct 2015 07:28:00 GMT",
    );

    let yaml = r#"
route:
  - name: free-code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: {cooldown}
"#;
    let gateway = spawn_gateway(yaml, &base, 60_000).await;

    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        mock.count("dyn/aa:free"),
        2,
        "una fecha pasada no debe blacklistear el modelo"
    );
}

/// A1: un `Retry-After` con fecha HTTP futura sí marca el modelo.
#[tokio::test]
async fn future_http_date_retry_after_blacklists() {
    let (base, mock) = spawn_openrouter(one_model_catalog()).await;
    mock.set_retry_after_value(
        "dyn/aa:free",
        StatusCode::OK,
        "Wed, 21 Oct 2099 07:28:00 GMT",
    );

    let yaml = r#"
route:
  - name: free-code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: {cooldown}
"#;
    let gateway = spawn_gateway(yaml, &base, 50).await;

    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::OK
    );
    // Muy por encima del cooldown configurado (50 ms): sigue en blacklist.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        mock.count("dyn/aa:free"),
        1,
        "con Retry-After futuro el modelo queda en blacklist"
    );
}

/// Scenario: Free agotados → fallback al backend de pago declarado.
#[tokio::test]
async fn falls_back_to_paid_backend_when_free_fails() {
    let (base, mock) = spawn_openrouter(two_model_catalog()).await;
    mock.set("dyn/aa:free", StatusCode::TOO_MANY_REQUESTS);
    mock.set("dyn/bb:free", StatusCode::INTERNAL_SERVER_ERROR);
    mock.set("paid/model", StatusCode::OK);

    let yaml = r#"
route:
  - name: free-code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: {cooldown}
      - base_url: "{base}"
        model: "paid/model"
        api_key_env: "{key_env}"
"#;
    let gateway = spawn_gateway(yaml, &base, 60_000).await;

    let response = chat(&gateway, "free-code").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(
        backend_header(&response).contains("paid/model"),
        "debe servirse con el backend de pago, got: {}",
        backend_header(&response)
    );
}

/// Scenario: Ruta sin backends utilizables → `AllBackendsFailed`.
#[tokio::test]
async fn all_backends_failed_when_no_candidates() {
    let (base, _mock) = spawn_openrouter(no_eligible_catalog()).await;
    let yaml = r#"
route:
  - name: free-code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: {cooldown}
"#;
    let gateway = spawn_gateway(yaml, &base, 60_000).await;

    let response = chat(&gateway, "free-code").await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    let body: Value = response.json().await.expect("json de error");
    assert_eq!(body["error"]["code"], json!("upstream_error"));
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("free-code")),
        "el error debe nombrar la ruta: {body}"
    );
}

/// Scenario: `list_models` refleja los modelos dinámicos elegibles.
#[tokio::test]
async fn list_models_includes_dynamic_models() {
    let (base, _mock) = spawn_openrouter(two_model_catalog()).await;
    let yaml = r#"
route:
  - name: free-code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "{key_env}"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: {cooldown}
"#;
    let gateway = spawn_gateway(yaml, &base, 60_000).await;

    // Una petición previa puebla la caché del catálogo.
    assert_eq!(
        chat(&gateway, "free-code").await.status(),
        reqwest::StatusCode::OK
    );

    let body: Value = reqwest::Client::new()
        .get(format!("{gateway}/v1/models"))
        .send()
        .await
        .expect("request a /v1/models")
        .json()
        .await
        .expect("json");

    let ids: Vec<&str> = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(ids.contains(&"free-code"), "ids: {ids:?}");
    assert!(ids.contains(&"dyn/aa:free"), "ids: {ids:?}");
    assert!(ids.contains(&"dyn/bb:free"), "ids: {ids:?}");
}
