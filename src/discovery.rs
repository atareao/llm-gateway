//! Descubrimiento del catálogo free de OpenRouter.
//!
//! Un [`CatalogCache`] consulta `GET {base_url}/models` (endpoint público, sin
//! autenticación), filtra los modelos **gratuitos** que además exponen
//! **tool-calling** y guarda el catálogo con un TTL. El refresco es
//! *stale-while-revalidate*: con una caché válida nunca se bloquea la petición;
//! si el TTL ha vencido se devuelve la copia existente y se dispara un refresco
//! en segundo plano. Un fallo de red conserva la última caché válida.
//!
//! El filtrado de elegibilidad se expone en [`eligible_models`] para que la
//! clasificación por perfil ([`crate::classify`]) trabaje sobre modelos ya
//! depurados.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::config::{DEFAULT_REFRESH_INTERVAL_MS, GatewayConfig};
use crate::metrics::Metrics;

/// Entrada del catálogo `GET /models` de OpenRouter.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    /// Identificador del modelo (p. ej. `deepseek/deepseek-r1:free`).
    pub id: String,
    /// Nombre legible (opcional).
    #[serde(default)]
    pub name: Option<String>,
    /// Descripción del modelo (opcional).
    #[serde(default)]
    pub description: Option<String>,
    /// Longitud de contexto en tokens (opcional).
    #[serde(default)]
    pub context_length: Option<u32>,
    /// Precios por token; `0` en `prompt` y `completion` indica modelo free.
    #[serde(default)]
    pub pricing: Option<Pricing>,
    /// Parámetros soportados por el modelo; se exige `tools` y `tool_choice`.
    #[serde(default)]
    pub supported_parameters: Option<Vec<String>>,
}

/// Precios de un modelo. Los valores llegan como cadena o número según el
/// proveedor, así que se conservan como [`Value`] y se comparan con cero.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pricing {
    /// Precio de prompt por token.
    #[serde(default)]
    pub prompt: Option<Value>,
    /// Precio de completion por token.
    #[serde(default)]
    pub completion: Option<Value>,
}

/// Cuerpo de `GET /models`.
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

/// Error del descubrimiento del catálogo.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// Fallo de transporte (conexión, timeout, lectura).
    #[error("error de transporte consultando el catálogo: {0}")]
    Transport(#[source] reqwest::Error),
    /// El catálogo respondió un status no exitoso.
    #[error("el catálogo respondió con status {0}")]
    Status(u16),
    /// No se pudo decodificar el cuerpo del catálogo.
    #[error("no se pudo decodificar el catálogo: {0}")]
    Decode(#[source] reqwest::Error),
}

/// URL del endpoint de catálogo a partir de la `base_url` (que incluye `/v1`).
pub fn models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// `true` si el valor de pricing es exactamente cero (cadena o número).
fn is_zero(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(number)) => number.as_f64() == Some(0.0),
        Some(Value::String(text)) => text.trim().parse::<f64>() == Ok(0.0),
        _ => false,
    }
}

/// `true` si el modelo es gratuito: identificador con sufijo `:free` o precios
/// `prompt` y `completion` iguales a cero.
pub fn is_free(model: &ModelEntry) -> bool {
    if model.id.ends_with(":free") {
        return true;
    }
    match &model.pricing {
        Some(pricing) => is_zero(pricing.prompt.as_ref()) && is_zero(pricing.completion.as_ref()),
        None => false,
    }
}

/// `true` si el modelo expone tool-calling (`supported_parameters` contiene
/// `tools` y `tool_choice`).
pub fn supports_tool_calling(model: &ModelEntry) -> bool {
    model.supported_parameters.as_ref().is_some_and(|params| {
        let has = |needle: &str| params.iter().any(|param| param == needle);
        has("tools") && has("tool_choice")
    })
}

/// `true` si el modelo es elegible: gratuito y con tool-calling.
pub fn is_eligible(model: &ModelEntry) -> bool {
    is_free(model) && supports_tool_calling(model)
}

/// Filtra un catálogo a los modelos elegibles (free + tool-calling).
pub fn eligible_models(models: &[ModelEntry]) -> Vec<ModelEntry> {
    models
        .iter()
        .filter(|model| is_eligible(model))
        .cloned()
        .collect()
}

/// Consulta `GET {base_url}/models` y devuelve el catálogo completo.
///
/// # Errors
/// Devuelve [`DiscoveryError`] si falla el transporte, el status no es exitoso
/// o el cuerpo no decodifica.
pub async fn fetch_catalog(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<Vec<ModelEntry>, DiscoveryError> {
    let response = client
        .get(models_url(base_url))
        .send()
        .await
        .map_err(DiscoveryError::Transport)?;

    let status = response.status();
    if !status.is_success() {
        return Err(DiscoveryError::Status(status.as_u16()));
    }

    let body = response
        .json::<ModelsResponse>()
        .await
        .map_err(DiscoveryError::Decode)?;
    Ok(body.data)
}

/// Instantánea de la caché: catálogo y momento de captura.
struct Snapshot {
    models: Arc<Vec<ModelEntry>>,
    fetched_at: Instant,
}

/// Caché del catálogo con TTL y refresco *stale-while-revalidate*.
pub struct CatalogCache {
    client: reqwest::Client,
    base_url: String,
    ttl: Duration,
    state: RwLock<Option<Snapshot>>,
    refreshing: AtomicBool,
    metrics: Arc<Metrics>,
}

impl CatalogCache {
    /// Construye una caché vacía para `base_url` con el TTL indicado.
    pub fn new(
        client: reqwest::Client,
        base_url: String,
        ttl: Duration,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            client,
            base_url,
            ttl,
            state: RwLock::new(None),
            refreshing: AtomicBool::new(false),
            metrics,
        }
    }

    /// Devuelve el catálogo cacheado (aunque esté vencido), o `None`.
    pub fn cached(&self) -> Option<Arc<Vec<ModelEntry>>> {
        self.snapshot().map(|snapshot| snapshot.models)
    }

    /// TTL configurado de la caché (usado como periodo de refresco).
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// `base_url` asociada a esta caché.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Instantánea interna (catálogo + instante de captura).
    fn snapshot(&self) -> Option<Snapshot> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|snapshot| Snapshot {
                models: Arc::clone(&snapshot.models),
                fetched_at: snapshot.fetched_at,
            })
    }

    /// Devuelve el catálogo para atender una petición.
    ///
    /// - Sin caché: refresca de forma **síncrona** y devuelve el resultado.
    /// - Con caché fresca: devuelve la copia.
    /// - Con caché vencida: devuelve la copia existente y dispara un refresco
    ///   en segundo plano **sin bloquear**.
    pub async fn ensure(self: &Arc<Self>) -> Option<Arc<Vec<ModelEntry>>> {
        match self.snapshot() {
            Some(snapshot) => {
                if snapshot.fetched_at.elapsed() >= self.ttl {
                    self.spawn_refresh();
                }
                Some(snapshot.models)
            }
            None => {
                self.refresh().await.ok()?;
                self.cached()
            }
        }
    }

    /// Refresca el catálogo desde el upstream.
    ///
    /// En caso de éxito sustituye la caché y registra métricas; en caso de fallo
    /// **conserva** la caché previa (stale-while-error) y registra el fallo.
    ///
    /// # Errors
    /// Propaga el [`DiscoveryError`] del fetch.
    pub async fn refresh(&self) -> Result<(), DiscoveryError> {
        match fetch_catalog(&self.client, &self.base_url).await {
            Ok(models) => {
                let total = models.len();
                let eligible = models.iter().filter(|model| is_eligible(model)).count();
                let snapshot = Snapshot {
                    models: Arc::new(models),
                    fetched_at: Instant::now(),
                };
                *self
                    .state
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
                self.metrics
                    .record_discovery_catalog(&self.base_url, total, eligible);
                self.metrics.record_discovery_refresh(&self.base_url, true);
                Ok(())
            }
            Err(err) => {
                self.metrics.record_discovery_refresh(&self.base_url, false);
                Err(err)
            }
        }
    }

    /// Lanza un refresco en segundo plano si no hay otro en vuelo.
    fn spawn_refresh(self: &Arc<Self>) {
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = this.refresh().await {
                tracing::debug!(
                    base_url = %this.base_url,
                    error = %err,
                    "refresco del catálogo en segundo plano fallido; se conserva la caché"
                );
            }
            this.refreshing.store(false, Ordering::Release);
        });
    }
}

/// Registro de cachés de catálogo, una por `base_url` dinámica.
///
/// Agrupa el descubrimiento de todos los backends `openrouter_free` de la
/// configuración en un único `/models` por `base_url` (los perfiles comparten
/// catálogo).
pub struct Registry {
    catalogs: HashMap<String, Arc<CatalogCache>>,
}

impl Registry {
    /// Construye el registro a partir de la configuración y el cliente HTTP.
    pub fn new(config: &GatewayConfig, client: reqwest::Client, metrics: Arc<Metrics>) -> Self {
        let mut catalogs: HashMap<String, Arc<CatalogCache>> = HashMap::new();
        for route in &config.route {
            for backend in &route.backend {
                if !backend.is_dynamic() {
                    continue;
                }
                let ttl = backend
                    .discovery
                    .as_ref()
                    .map(|discovery| discovery.refresh_interval())
                    .unwrap_or(Duration::from_millis(DEFAULT_REFRESH_INTERVAL_MS));
                catalogs.entry(backend.base_url.clone()).or_insert_with(|| {
                    Arc::new(CatalogCache::new(
                        client.clone(),
                        backend.base_url.clone(),
                        ttl,
                        Arc::clone(&metrics),
                    ))
                });
            }
        }
        Self { catalogs }
    }

    /// Caché de catálogo asociada a una `base_url`, si existe.
    pub fn catalog(&self, base_url: &str) -> Option<&Arc<CatalogCache>> {
        self.catalogs.get(base_url)
    }

    /// Lanza una tarea de refresco por cada catálogo, siguiendo el patrón de
    /// [`crate::warmup::spawn_warmup`]: primer tick inmediato y después cada TTL,
    /// con [`MissedTickBehavior::Delay`]. Un refresco fallido nunca aborta el
    /// bucle (la caché previa se conserva).
    ///
    /// Devuelve un `JoinHandle` por catálogo; el llamante decide qué hacer con
    /// ellos (`main.rs` los aborta en el cierre ordenado).
    pub fn spawn_refresh(&self) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        for cache in self.catalogs.values() {
            let interval = cache.ttl();
            if interval.is_zero() {
                continue;
            }
            let cache = Arc::clone(cache);
            handles.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    if let Err(err) = cache.refresh().await {
                        tracing::debug!(
                            base_url = %cache.base_url(),
                            error = %err,
                            "refresco del catálogo fallido; se conserva la caché"
                        );
                    }
                }
            }));
        }
        handles
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, params: &[&str], prompt: Value, completion: Value) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            name: None,
            description: None,
            context_length: None,
            pricing: Some(Pricing {
                prompt: Some(prompt),
                completion: Some(completion),
            }),
            supported_parameters: Some(params.iter().map(|p| (*p).to_owned()).collect()),
        }
    }

    #[test]
    fn free_by_suffix_is_free_regardless_of_pricing() {
        let model = entry("x/y:free", &[], json!("0.1"), json!("0.1"));
        assert!(is_free(&model));
    }

    #[test]
    fn zero_pricing_is_free_for_string_and_number() {
        assert!(is_free(&entry("a", &[], json!("0"), json!("0"))));
        assert!(is_free(&entry("b", &[], json!(0), json!(0.0))));
        assert!(!is_free(&entry("c", &[], json!("0"), json!("0.0001"))));
    }

    #[test]
    fn tool_calling_requires_both_parameters() {
        assert!(supports_tool_calling(&entry(
            "a",
            &["tools", "tool_choice"],
            json!("0"),
            json!("0")
        )));
        assert!(!supports_tool_calling(&entry(
            "b",
            &["tools"],
            json!("0"),
            json!("0")
        )));
        assert!(!supports_tool_calling(&entry(
            "c",
            &["tool_choice"],
            json!("0"),
            json!("0")
        )));
        assert!(!supports_tool_calling(&entry(
            "d",
            &[],
            json!("0"),
            json!("0")
        )));
    }
}
