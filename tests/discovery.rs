//! Tests de integración del descubrimiento del catálogo free de OpenRouter.
//!
//! Se usa un servidor HTTP simulado con `axum` (igual que el resto de la suite)
//! en vez de llamar a OpenRouter real.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use llm_gateway_rs::config::GatewayConfig;
use llm_gateway_rs::discovery::{CatalogCache, Registry, eligible_models, models_url};
use llm_gateway_rs::metrics::Metrics;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Config mínima con un backend dinámico, para construir `Metrics`.
fn config() -> GatewayConfig {
    let yaml = r#"
route:
  - name: code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "http://127.0.0.1:1/v1"
        api_key_env: "OPENROUTER_API_KEY"
"#;
    serde_saphyr::from_str(yaml).expect("config de test válida")
}

fn metrics() -> Arc<Metrics> {
    Arc::new(Metrics::new(&config()))
}

/// Catálogo con un modelo free con tool-calling, uno de pago y uno free sin
/// tool-calling.
fn catalog() -> Value {
    json!({
        "data": [
            {
                "id": "deepseek/deepseek-r1:free",
                "name": "DeepSeek R1",
                "context_length": 64000,
                "supported_parameters": ["tools", "tool_choice", "temperature"],
                "pricing": {"prompt": "0", "completion": "0"}
            },
            {
                "id": "meta-llama/llama-3.3-70b-instruct",
                "name": "Llama 3.3 70B",
                "context_length": 131072,
                "supported_parameters": ["tools", "tool_choice"],
                "pricing": {"prompt": "0.0000001", "completion": "0.0000002"}
            },
            {
                "id": "some/free-no-tools:free",
                "name": "Free sin tools",
                "context_length": 32000,
                "supported_parameters": ["temperature"],
                "pricing": {"prompt": "0", "completion": "0"}
            }
        ]
    })
}

/// Servidor que responde siempre `200` con `body` en `/v1/models`.
async fn spawn_models_server(body: Value) -> String {
    async fn handler(State(body): State<Arc<Value>>) -> axum::Json<Value> {
        axum::Json((*body).clone())
    }
    let app = Router::new()
        .route("/v1/models", get(handler))
        .with_state(Arc::new(body));
    spawn(app).await
}

/// Servidor que responde la n-ésima entrada de `responses`: `Some` → `200` con
/// el JSON; `None` → `500`. Al agotarse, repite la última.
async fn spawn_sequence_server(responses: Vec<Option<Value>>) -> String {
    async fn handler(State(state): State<Arc<(Vec<Option<Value>>, AtomicUsize)>>) -> Response {
        let (responses, counter) = &*state;
        let idx = counter.fetch_add(1, Ordering::SeqCst);
        let entry = &responses[idx.min(responses.len() - 1)];
        match entry {
            Some(body) => axum::Json(body.clone()).into_response(),
            None => (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
        }
    }
    let app = Router::new()
        .route("/v1/models", get(handler))
        .with_state(Arc::new((responses, AtomicUsize::new(0))));
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

fn cache(base_url: &str, ttl: Duration) -> Arc<CatalogCache> {
    Arc::new(CatalogCache::new(
        reqwest::Client::new(),
        base_url.to_owned(),
        ttl,
        metrics(),
    ))
}

/// Scenario: Descubrimiento inicial → consulta `/models`, puebla la caché y
/// resuelve con los modelos elegibles.
#[tokio::test]
async fn initial_discovery_populates_cache_with_eligible_models() {
    let base = spawn_models_server(catalog()).await;
    let cache = cache(&base, Duration::from_secs(300));

    let models = cache.ensure().await.expect("la caché debe poblarse");
    let eligible = eligible_models(&models);
    let ids: Vec<&str> = eligible.iter().map(|m| m.id.as_str()).collect();

    assert_eq!(ids, vec!["deepseek/deepseek-r1:free"]);
}

/// Scenario: Exclusión de modelos de pago.
#[tokio::test]
async fn paid_models_are_excluded() {
    let base = spawn_models_server(catalog()).await;
    let cache = cache(&base, Duration::from_secs(300));

    let models = cache.ensure().await.expect("la caché debe poblarse");
    let ids: Vec<String> = eligible_models(&models).into_iter().map(|m| m.id).collect();

    assert!(
        !ids.iter().any(|id| id.contains("llama-3.3-70b")),
        "un modelo con pricing no nulo no debe ser elegible, ids: {ids:?}"
    );
}

/// Scenario: Exclusión de modelos sin tool-calling.
#[tokio::test]
async fn models_without_tool_calling_are_excluded() {
    let base = spawn_models_server(catalog()).await;
    let cache = cache(&base, Duration::from_secs(300));

    let models = cache.ensure().await.expect("la caché debe poblarse");
    let ids: Vec<String> = eligible_models(&models).into_iter().map(|m| m.id).collect();

    assert!(
        !ids.iter().any(|id| id.contains("free-no-tools")),
        "un modelo sin tools/tool_choice no debe ser elegible, ids: {ids:?}"
    );
}

/// Scenario: Fallo de red con caché válida → se conserva la última caché.
#[tokio::test]
async fn network_failure_keeps_last_valid_cache() {
    // 1ª llamada: catálogo OK. 2ª en adelante: 500.
    let base = spawn_sequence_server(vec![Some(catalog()), None]).await;
    let cache = cache(&base, Duration::from_secs(300));

    let first = cache.ensure().await.expect("primera carga");
    assert_eq!(first.len(), 3);

    let result = cache.refresh().await;
    assert!(result.is_err(), "el refresco debe fallar con 500");

    let kept = cache.cached().expect("la caché válida debe conservarse");
    assert_eq!(kept.len(), 3, "el fallo no debe vaciar la caché");
}

/// Scenario: TTL vencido en segundo plano → responde con la caché existente y
/// refresca en segundo plano sin bloquear.
#[tokio::test]
async fn expired_ttl_returns_stale_and_refreshes_in_background() {
    let first = json!({"data": [{
        "id": "a/first:free", "context_length": 1000,
        "supported_parameters": ["tools", "tool_choice"],
        "pricing": {"prompt": "0", "completion": "0"}
    }]});
    let second = json!({"data": [{
        "id": "b/second:free", "context_length": 2000,
        "supported_parameters": ["tools", "tool_choice"],
        "pricing": {"prompt": "0", "completion": "0"}
    }]});
    let base = spawn_sequence_server(vec![Some(first), Some(second)]).await;
    let cache = cache(&base, Duration::from_millis(50));

    let initial = cache.ensure().await.expect("primera carga");
    assert_eq!(initial[0].id, "a/first:free");

    tokio::time::sleep(Duration::from_millis(90)).await;

    // Stale-while-revalidate: devuelve la caché vieja sin bloquear en el fetch.
    let stale = cache.ensure().await.expect("caché existente");
    assert_eq!(
        stale[0].id, "a/first:free",
        "con TTL vencido debe responder la caché existente sin bloquear"
    );

    // El refresco en segundo plano acaba poblando la caché nueva.
    let mut updated = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Some(models) = cache.cached()
            && models[0].id == "b/second:free"
        {
            updated = true;
            break;
        }
    }
    assert!(
        updated,
        "el refresco en segundo plano debe actualizar la caché"
    );
}

/// `models_url` une bien las barras.
#[test]
fn models_url_joins_slashes() {
    assert_eq!(
        models_url("https://openrouter.ai/api/v1"),
        "https://openrouter.ai/api/v1/models"
    );
    assert_eq!(
        models_url("https://openrouter.ai/api/v1/"),
        "https://openrouter.ai/api/v1/models"
    );
}

/// Tarea 2.3: el refresco de arranque puebla la caché en segundo plano.
#[tokio::test]
async fn spawn_refresh_populates_cache_in_background() {
    let base = spawn_models_server(catalog()).await;
    let yaml = format!(
        r#"
route:
  - name: code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 60000
          blacklist_cooldown_ms: 60000
"#
    );
    let config: GatewayConfig = serde_saphyr::from_str(&yaml).expect("config de test válida");
    let registry = Registry::new(&config, reqwest::Client::new(), metrics());
    let handles = registry.spawn_refresh();

    let cache = registry.catalog(&base).expect("catálogo registrado");
    let mut populated = false;
    for _ in 0..50 {
        if cache.cached().is_some() {
            populated = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for handle in &handles {
        handle.abort();
    }
    assert!(populated, "el refresco de arranque debe poblar la caché");
}
