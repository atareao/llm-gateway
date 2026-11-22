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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::classify::classify;
use crate::config::{ClassifyOverrides, DEFAULT_REFRESH_INTERVAL_MS, GatewayConfig, Profile};
use crate::metrics::Metrics;

/// Timeout total (conexión + respuesta + cuerpo) de la consulta del catálogo.
///
/// El cliente HTTP compartido solo fija `connect_timeout`; sin un timeout total,
/// un upstream que acepta la conexión y no responde dejaría colgada la rama
/// síncrona de [`CatalogCache::ensure`] (y el ticker de refresco).
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);

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
/// Aplica `timeout` como límite total de la petición (conexión, respuesta y
/// lectura del cuerpo).
///
/// # Errors
/// Devuelve [`DiscoveryError`] si falla el transporte (incluido el timeout), el
/// status no es exitoso o el cuerpo no decodifica.
pub async fn fetch_catalog(
    client: &reqwest::Client,
    base_url: &str,
    timeout: Duration,
) -> Result<Vec<ModelEntry>, DiscoveryError> {
    let response = client
        .get(models_url(base_url))
        .timeout(timeout)
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
    timeout: Duration,
    state: RwLock<Option<Snapshot>>,
    refreshing: AtomicBool,
    metrics: Arc<Metrics>,
}

impl CatalogCache {
    /// Construye una caché vacía para `base_url` con el TTL indicado.
    ///
    /// El fetch del catálogo usa [`CATALOG_TIMEOUT`] como timeout total.
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
            timeout: CATALOG_TIMEOUT,
            state: RwLock::new(None),
            refreshing: AtomicBool::new(false),
            metrics,
        }
    }

    /// Sobrescribe el timeout total del fetch del catálogo.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
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
        match fetch_catalog(&self.client, &self.base_url, self.timeout).await {
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

/// Blacklist global de modelos dinámicos.
///
/// Un modelo entra en la blacklist durante un cooldown cuando su petición
/// devuelve `429`, un `5xx` o un `Retry-After`. La blacklist es **global**: se
/// comparte entre todas las rutas, porque un `429` es por combinación
/// modelo+clave.
pub struct Blacklist {
    entries: RwLock<HashMap<String, Instant>>,
    metrics: Arc<Metrics>,
}

impl Blacklist {
    /// Crea una blacklist vacía.
    pub fn new(metrics: Arc<Metrics>) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            metrics,
        }
    }

    /// Marca `model` como no elegible durante `cooldown`.
    pub fn mark(&self, model: &str, cooldown: Duration) {
        let until = Instant::now() + cooldown;
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(model.to_owned(), until);
        self.metrics.set_blacklist_size(self.len());
    }

    /// `true` si `model` está actualmente en blacklist (purgando expirados).
    pub fn is_blocked(&self, model: &str) -> bool {
        let mut entries = self.write();
        purge_expired(&mut entries);
        entries.contains_key(model)
    }

    /// Número de modelos actualmente en blacklist (purgando expirados).
    pub fn len(&self) -> usize {
        let mut entries = self.write();
        purge_expired(&mut entries);
        entries.len()
    }

    /// `true` si la blacklist está vacía.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Devuelve los modelos de `models` que **no** están en blacklist.
    pub fn retain_eligible(&self, models: &[String]) -> Vec<String> {
        let mut entries = self.write();
        purge_expired(&mut entries);
        models
            .iter()
            .filter(|model| !entries.contains_key(*model))
            .cloned()
            .collect()
    }

    /// Toma el lock de escritura, ignorando un posible envenenamiento.
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Instant>> {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Elimina de `entries` las marcas cuyo cooldown ya venció.
fn purge_expired(entries: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    entries.retain(|_, until| *until > now);
}

/// Backend dinámico registrado, con su estado de rotación.
struct DynamicBackend {
    route: usize,
    backend: usize,
    profile: Profile,
    base_url: String,
    min_context: Option<u32>,
    cooldown: Duration,
    overrides: Option<ClassifyOverrides>,
    rotation: AtomicUsize,
}

/// Registro de cachés de catálogo, una por `base_url` dinámica.
///
/// Agrupa el descubrimiento de todos los backends `openrouter_free` de la
/// configuración en un único `/models` por `base_url` (los perfiles comparten
/// catálogo) y mantiene la blacklist global y el estado de rotación.
pub struct Registry {
    catalogs: HashMap<String, Arc<CatalogCache>>,
    dynamic: Vec<DynamicBackend>,
    blacklist: Arc<Blacklist>,
    metrics: Arc<Metrics>,
}

impl Registry {
    /// Construye el registro a partir de la configuración y el cliente HTTP.
    pub fn new(config: &GatewayConfig, client: reqwest::Client, metrics: Arc<Metrics>) -> Self {
        let mut catalogs: HashMap<String, Arc<CatalogCache>> = HashMap::new();
        let mut dynamic = Vec::new();

        for (route_idx, route) in config.route.iter().enumerate() {
            for (backend_idx, backend) in route.backend.iter().enumerate() {
                if !backend.is_dynamic() {
                    continue;
                }
                let discovery = backend.discovery.as_ref();
                let ttl = discovery
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
                dynamic.push(DynamicBackend {
                    route: route_idx,
                    backend: backend_idx,
                    profile: backend.profile(),
                    base_url: backend.base_url.clone(),
                    min_context: discovery.and_then(|discovery| discovery.min_context),
                    cooldown: discovery
                        .map(|discovery| discovery.blacklist_cooldown())
                        .unwrap_or(Duration::from_millis(
                            crate::config::DEFAULT_BLACKLIST_COOLDOWN_MS,
                        )),
                    overrides: discovery.and_then(|discovery| discovery.overrides.clone()),
                    rotation: AtomicUsize::new(0),
                });
            }
        }

        Self {
            catalogs,
            dynamic,
            blacklist: Arc::new(Blacklist::new(Arc::clone(&metrics))),
            metrics,
        }
    }

    /// Caché de catálogo asociada a una `base_url`, si existe.
    pub fn catalog(&self, base_url: &str) -> Option<&Arc<CatalogCache>> {
        self.catalogs.get(base_url)
    }

    /// Blacklist global compartida.
    pub fn blacklist(&self) -> &Arc<Blacklist> {
        &self.blacklist
    }

    /// Backend dinámico registrado en `(route, backend)`, si lo hay.
    fn dynamic(&self, route: usize, backend: usize) -> Option<&DynamicBackend> {
        self.dynamic
            .iter()
            .find(|entry| entry.route == route && entry.backend == backend)
    }

    /// Cooldown configurado para el backend dinámico `(route, backend)`.
    pub fn cooldown(&self, route: usize, backend: usize) -> Option<Duration> {
        self.dynamic(route, backend).map(|entry| entry.cooldown)
    }

    /// Expande el backend dinámico `(route, backend)` a los modelos concretos
    /// elegibles de su perfil, en orden de preferencia, excluyendo los modelos en
    /// blacklist y aplicando rotación round-robin entre peticiones sucesivas.
    ///
    /// Devuelve una lista vacía si el backend no es dinámico, no hay catálogo o
    /// el perfil no aporta candidatos.
    pub async fn expand(&self, route: usize, backend: usize) -> Vec<String> {
        let Some(entry) = self.dynamic(route, backend) else {
            return Vec::new();
        };
        let Some(cache) = self.catalogs.get(&entry.base_url) else {
            return Vec::new();
        };
        let Some(catalog) = cache.ensure().await else {
            return Vec::new();
        };

        let eligible = eligible_models(&catalog);
        let ordered = classify(
            &eligible,
            entry.profile,
            entry.min_context,
            entry.overrides.as_ref(),
        );
        self.metrics
            .record_discovery_profile(&entry.base_url, entry.profile, ordered.len());

        let mut candidates = self.blacklist.retain_eligible(&ordered);
        if candidates.is_empty() {
            return candidates;
        }
        let offset = entry.rotation.fetch_add(1, Ordering::Relaxed) % candidates.len();
        candidates.rotate_left(offset);
        candidates
    }

    /// Identificadores de modelos dinámicos elegibles (para `/v1/models`),
    /// deduplicados, sin rotación ni filtrado por blacklist.
    pub async fn eligible_model_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for entry in &self.dynamic {
            let Some(cache) = self.catalogs.get(&entry.base_url) else {
                continue;
            };
            let Some(catalog) = cache.ensure().await else {
                continue;
            };
            let eligible = eligible_models(&catalog);
            for id in classify(
                &eligible,
                entry.profile,
                entry.min_context,
                entry.overrides.as_ref(),
            ) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        ids
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

    /// `Metrics` mínimo para construir una `Blacklist` en tests.
    fn metrics() -> Arc<Metrics> {
        let yaml = r#"
route:
  - name: blacklist-test
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "http://127.0.0.1:1/v1"
        api_key_env: "KEY"
"#;
        let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");
        Arc::new(Metrics::new(&config))
    }

    #[test]
    fn blacklist_marks_and_expires() {
        let blacklist = Blacklist::new(metrics());
        assert!(!blacklist.is_blocked("m"));
        assert!(blacklist.is_empty());

        blacklist.mark("m", Duration::from_millis(50));
        assert!(blacklist.is_blocked("m"));
        assert_eq!(blacklist.len(), 1);

        std::thread::sleep(Duration::from_millis(90));
        assert!(
            !blacklist.is_blocked("m"),
            "el cooldown vencido debe dejar de bloquear"
        );
        assert!(blacklist.is_empty());
    }

    #[test]
    fn blacklist_retain_eligible_filters_blocked() {
        let blacklist = Blacklist::new(metrics());
        blacklist.mark("b", Duration::from_secs(60));
        let models = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(blacklist.retain_eligible(&models), vec!["a", "c"]);
    }

    #[test]
    fn blacklist_is_shared_through_arc() {
        let blacklist = Arc::new(Blacklist::new(metrics()));
        let other = Arc::clone(&blacklist);
        blacklist.mark("m", Duration::from_secs(60));
        assert!(other.is_blocked("m"), "la blacklist es global/compartida");
    }
}
