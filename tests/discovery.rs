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

/// Config con un backend dinámico sobre `base_url`, para construir `Metrics` y
/// un `Registry` que ejecute el descubrimiento real contra el mock.
fn config_for(base_url: &str) -> GatewayConfig {
    let yaml = format!(
        r#"
route:
  - name: code
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base_url}"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 60000
"#
    );
    serde_saphyr::from_str(&yaml).expect("config de test válida")
}

/// Valor de la serie `name` para `base_url` (y `profile`/`result` opcional) en
/// el render de Prometheus.
fn series_value(
    text: &str,
    name: &str,
    base_url: &str,
    extra: Option<(&str, &str)>,
) -> Option<u64> {
    let line = text.lines().find(|line| {
        line.starts_with(name)
            && line.contains(&format!(r#"base_url="{base_url}""#))
            && extra.is_none_or(|(key, value)| line.contains(&format!(r#"{key}="{value}""#)))
    })?;
    line.rsplit_once(' ')?.1.parse().ok()
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

/// Caché con un timeout de fetch corto, para tests que no deben colgarse.
fn cache_with_timeout(base_url: &str, ttl: Duration, timeout: Duration) -> Arc<CatalogCache> {
    Arc::new(
        CatalogCache::new(reqwest::Client::new(), base_url.to_owned(), ttl, metrics())
            .with_timeout(timeout),
    )
}

/// Respuesta guionizada de un servidor simulado.
enum Reply {
    /// `200` con este JSON.
    Json(Value),
    /// `200` con este JSON tras `Duration`.
    JsonAfter(Duration, Value),
    /// Nunca responde a tiempo (duerme mucho más que cualquier timeout).
    Hang,
}

/// Servidor que atiende `/v1/models` según `replies` (n-ésima entrada, repite la
/// última al agotarse) y cuenta las peticiones recibidas.
async fn spawn_scripted_server(replies: Vec<Reply>) -> (String, Arc<AtomicUsize>) {
    async fn handler(State(state): State<Arc<(Vec<Reply>, Arc<AtomicUsize>)>>) -> Response {
        let (replies, counter) = &*state;
        let idx = counter.fetch_add(1, Ordering::SeqCst);
        match &replies[idx.min(replies.len() - 1)] {
            Reply::Json(body) => axum::Json(body.clone()).into_response(),
            Reply::JsonAfter(delay, body) => {
                tokio::time::sleep(*delay).await;
                axum::Json(body.clone()).into_response()
            }
            Reply::Hang => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                (StatusCode::OK, "late").into_response()
            }
        }
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/models", get(handler))
        .with_state(Arc::new((replies, Arc::clone(&counter))));
    let base = spawn(app).await;
    (base, counter)
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

/// M1: el fetch del catálogo aplica un timeout total y no se cuelga cuando el
/// upstream acepta la conexión pero no responde.
#[tokio::test]
async fn catalog_fetch_times_out_and_does_not_hang() {
    let (base, _count) = spawn_scripted_server(vec![Reply::Hang]).await;
    let cache = cache_with_timeout(&base, Duration::from_secs(300), Duration::from_millis(150));

    let outcome = tokio::time::timeout(Duration::from_secs(5), cache.refresh()).await;
    let result = outcome.expect("refresh debe respetar el timeout, no colgarse");
    assert!(
        result.is_err(),
        "un upstream que no responde debe dar error de timeout"
    );
}

/// M1: con TTL vencido y upstream lento, `ensure` responde con la caché
/// existente sin bloquearse (stale-while-error).
#[tokio::test]
async fn ensure_with_expired_ttl_does_not_hang_on_slow_upstream() {
    let (base, _count) = spawn_scripted_server(vec![Reply::Json(catalog()), Reply::Hang]).await;
    let cache = cache_with_timeout(&base, Duration::from_millis(50), Duration::from_millis(150));

    let first = cache.ensure().await.expect("primera carga");
    assert_eq!(first.len(), 3);

    // Vence el TTL para que `ensure` dispare un refresco en segundo plano.
    tokio::time::sleep(Duration::from_millis(80)).await;

    let stale = tokio::time::timeout(Duration::from_secs(5), cache.ensure())
        .await
        .expect("ensure no debe colgarse")
        .expect("debe conservar la caché");
    assert_eq!(
        stale.len(),
        3,
        "un upstream lento no debe vaciar la caché válida"
    );
}

/// M2: dos refrescos concurrentes no duplican el fetch del catálogo.
#[tokio::test]
async fn concurrent_refreshes_fetch_once() {
    let (base, count) =
        spawn_scripted_server(vec![Reply::JsonAfter(Duration::from_millis(80), catalog())]).await;
    let cache = cache(&base, Duration::from_secs(300));

    let (first, second) = tokio::join!(cache.refresh_guarded(), cache.refresh_guarded());

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "solo debe haber un GET /models"
    );
    assert!(first || second, "al menos uno debe haber refrescado");
}

/// M2: N peticiones en arranque en frío comparten un único fetch.
#[tokio::test]
async fn concurrent_cold_start_ensure_fetches_once() {
    let (base, count) =
        spawn_scripted_server(vec![Reply::JsonAfter(Duration::from_millis(80), catalog())]).await;
    let cache = cache(&base, Duration::from_secs(300));

    let (a, b) = tokio::join!(cache.ensure(), cache.ensure());

    assert_eq!(count.load(Ordering::SeqCst), 1, "un solo fetch en frío");
    assert!(
        a.is_some() && b.is_some(),
        "ambas peticiones deben recibir el catálogo"
    );
}

/// Scenario: Métricas tras descubrimiento → el descubrimiento real contra
/// `/models` publica el total del catálogo, los elegibles por perfil y el
/// refresco con éxito.
#[tokio::test]
async fn real_discovery_records_catalog_and_profile_metrics() {
    let base = spawn_models_server(catalog()).await;
    let config = config_for(&base);
    let metrics = Arc::new(Metrics::new(&config));
    let registry = Registry::new(&config, reqwest::Client::new(), Arc::clone(&metrics));

    // Descubrimiento real contra el mock: `refresh` puebla la caché y publica el
    // total del catálogo; `expand` clasifica el perfil `code` y publica sus
    // elegibles. El catálogo tiene 3 modelos y solo uno es free con tools.
    let cache = registry.catalog(&base).expect("caché del mock");
    cache
        .refresh()
        .await
        .expect("el refresco contra el mock debe funcionar");
    let candidates = registry.expand(0, 0).await;
    assert_eq!(
        candidates,
        vec!["deepseek/deepseek-r1:free".to_owned()],
        "el perfil code debe resolver el modelo free con tool-calling"
    );

    let text = metrics.render();
    assert_eq!(
        series_value(&text, "llm_gateway_discovery_models", &base, None),
        Some(3),
        "el total del catálogo debe reflejarse en el gauge:\n{text}"
    );
    assert_eq!(
        series_value(
            &text,
            "llm_gateway_discovery_profile_models",
            &base,
            Some(("profile", "code"))
        ),
        Some(1),
        "los elegibles por perfil deben reflejarse en el gauge:\n{text}"
    );
    assert_eq!(
        series_value(
            &text,
            "llm_gateway_discovery_refresh_total",
            &base,
            Some(("result", "success"))
        ),
        Some(1),
        "el refresco real debe contarse con result=\"success\":\n{text}"
    );
}

/// Scenario: Fallo de refresco → un `/models` que responde 500 incrementa el
/// contador de refrescos con `result="failure"`.
#[tokio::test]
async fn failed_refresh_increments_failure_counter() {
    let base = spawn_sequence_server(vec![None]).await;
    let metrics = metrics();
    let cache = CatalogCache::new(
        reqwest::Client::new(),
        base.clone(),
        Duration::from_secs(300),
        Arc::clone(&metrics),
    );

    let result = cache.refresh().await;
    assert!(result.is_err(), "el refresco contra un 500 debe fallar");

    let text = metrics.render();
    assert_eq!(
        series_value(
            &text,
            "llm_gateway_discovery_refresh_total",
            &base,
            Some(("result", "failure"))
        ),
        Some(1),
        "el fallo real debe contarse con result=\"failure\":\n{text}"
    );
}
