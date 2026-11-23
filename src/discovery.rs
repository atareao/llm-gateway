//! Descubrimiento del catálogo de OpenRouter (tier free y tier de pago).
//!
//! Un [`CatalogCache`] consulta `GET {base_url}/models` (endpoint público, sin
//! autenticación), filtra los modelos **gratuitos** que además exponen
//! **tool-calling** y guarda el catálogo con un TTL. El refresco es
//! *stale-while-revalidate*: con una caché válida nunca se bloquea la petición;
//! si el TTL ha vencido se devuelve la copia existente y se dispara un refresco
//! en segundo plano. Un fallo de red conserva la última caché válida.
//!
//! El mismo catálogo (una caché por `base_url`) sirve al tier de pago
//! (`openrouter_paid`), que filtra por la lista curada `discovery.allow`, ordena
//! por precio y mantiene el modelo elegido con histéresis (ver
//! [`Registry::expand`]).
//!
//! El filtrado de elegibilidad se expone en [`eligible_models`] para que la
//! clasificación por perfil ([`crate::classify`]) trabaje sobre modelos ya
//! depurados.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::classify::{
    COMPLETION_WEIGHT, ClassifyContext, PROMPT_WEIGHT, PaidContext, blend_price, classify,
    classify_paid, matches_allow,
};
use crate::config::{ClassifyOverrides, DEFAULT_REFRESH_INTERVAL_MS, GatewayConfig, Profile};
use crate::metrics::Metrics;

/// Timeout total (conexión + respuesta + cuerpo) de la consulta del catálogo.
///
/// El cliente HTTP compartido solo fija `connect_timeout`; sin un timeout total,
/// un upstream que acepta la conexión y no responde dejaría colgada la rama
/// síncrona de [`CatalogCache::ensure`] (y el ticker de refresco).
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout total de la consulta de cuota (`GET {base_url}/key`).
pub const QUOTA_TIMEOUT: Duration = Duration::from_secs(10);

/// Cota máxima del gate global de cuota free (24 h).
///
/// Evita que un `X-RateLimit-Reset` absurdo deje el tier free bloqueado más de
/// lo razonable; el daño máximo es servir pago durante el resto del día.
pub const MAX_QUOTA_GATE: Duration = Duration::from_secs(24 * 60 * 60);

/// Milisegundos de un día (resolución del reset diario de OpenRouter).
const SECS_PER_DAY: u64 = 24 * 60 * 60;

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

/// URL del endpoint de clave a partir de la `base_url` (que incluye `/v1`).
pub fn key_url(base_url: &str) -> String {
    format!("{}/key", base_url.trim_end_matches('/'))
}

/// Extrae `free_model_daily_requests.remaining` del cuerpo de `GET /key`.
///
/// Acepta el bloque en la raíz o anidado bajo `data`, y el valor como número o
/// cadena. Un `remaining` negativo se normaliza a `0`.
pub fn free_quota_remaining(body: &Value) -> Option<u64> {
    let data = body.get("data").unwrap_or(body);
    let remaining = data.get("free_model_daily_requests")?.get("remaining")?;
    let value = match remaining {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }?;
    Some(value.max(0) as u64)
}

/// Consulta `GET {base_url}/key` y devuelve la cuota free restante observada.
///
/// Si el cuerpo no expone `free_model_daily_requests.remaining`, devuelve
/// `Ok(None)`. No consume cuota de modelos free.
///
/// # Errors
/// Devuelve [`DiscoveryError`] si falla el transporte (incluido el timeout), el
/// status no es exitoso o el cuerpo no decodifica.
pub async fn fetch_free_quota(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<Option<u64>, DiscoveryError> {
    let mut request = client.get(key_url(base_url)).timeout(timeout);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }

    let response = request.send().await.map_err(DiscoveryError::Transport)?;
    let status = response.status();
    if !status.is_success() {
        return Err(DiscoveryError::Status(status.as_u16()));
    }

    let body = response
        .json::<Value>()
        .await
        .map_err(DiscoveryError::Decode)?;
    Ok(free_quota_remaining(&body))
}

/// Convierte un epoch UTC en milisegundos a un [`Instant`] futuro.
///
/// Devuelve `None` si el instante ya pasó: un reset en el pasado no debe abrir
/// el gate.
pub fn epoch_ms_to_instant(epoch_ms: u64) -> Option<Instant> {
    let target = UNIX_EPOCH + Duration::from_millis(epoch_ms);
    let delta = target.duration_since(SystemTime::now()).ok()?;
    if delta.is_zero() {
        return None;
    }
    Some(Instant::now() + delta)
}

/// Próximo `00:00 UTC` como [`Instant`].
pub fn next_utc_midnight() -> Instant {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let delta = SECS_PER_DAY - (secs % SECS_PER_DAY);
    Instant::now() + Duration::from_secs(delta)
}

/// Deadline T del gate de cuota free.
///
/// Usa `reset_epoch_ms` (epoch en ms) si es futuro; si falta, el siguiente
/// `00:00 UTC`. Un reset en el pasado devuelve `None` (no abre el gate). El
/// resultado se acota a [`MAX_QUOTA_GATE`].
pub fn quota_deadline(reset_epoch_ms: Option<u64>) -> Option<Instant> {
    let now = Instant::now();
    let deadline = match reset_epoch_ms {
        Some(ms) => epoch_ms_to_instant(ms)?,
        None => next_utc_midnight(),
    };
    if deadline <= now {
        return None;
    }
    Some(deadline.min(now + MAX_QUOTA_GATE))
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
    /// Serializa los refrescos: el ticker y el refresco on-demand lo consultan
    /// para no lanzar fetches duplicados concurrentes.
    refresh_lock: tokio::sync::Mutex<()>,
    /// Último snapshot de precios por `model_id`, para el dedup del evento
    /// `gateway_catalog`.
    prices: PriceTracker,
    /// Número de refrescos lanzados en segundo plano; solo para tests.
    #[cfg(test)]
    spawns: AtomicUsize,
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
            refresh_lock: tokio::sync::Mutex::new(()),
            prices: PriceTracker::new(),
            #[cfg(test)]
            spawns: AtomicUsize::new(0),
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

    /// Precarga la caché con `models` como si se acabaran de descargar.
    ///
    /// Solo para tests: permite ejercitar la expansión sin red.
    #[cfg(test)]
    pub(crate) fn seed(&self, models: Vec<ModelEntry>) {
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Snapshot {
            models: Arc::new(models),
            fetched_at: Instant::now(),
        });
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
                // Arranque en frío: N peticiones concurrentes comparten un único
                // fetch (se serializan con `refresh_lock` y la segunda reutiliza
                // la caché ya poblada).
                let _guard = self.refresh_lock.lock().await;
                if let Some(snapshot) = self.snapshot() {
                    return Some(snapshot.models);
                }
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
                let events = self.prices.observe(&models);
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
                emit_catalog_events(&events);
                Ok(())
            }
            Err(err) => {
                self.metrics.record_discovery_refresh(&self.base_url, false);
                Err(err)
            }
        }
    }

    /// Refresca el catálogo si no hay otro refresco en vuelo.
    ///
    /// Devuelve `true` si este llamante realizó el refresco y `false` si lo
    /// omitió porque ya había uno en curso. Es el punto de entrada que comparten
    /// el ticker de [`Registry::spawn_refresh`] y el refresco on-demand, de modo
    /// que nunca se solapan dos fetches de `/models`.
    pub async fn refresh_guarded(&self) -> bool {
        let Ok(_guard) = self.refresh_lock.try_lock() else {
            return false;
        };
        if let Err(err) = self.refresh().await {
            tracing::debug!(
                base_url = %self.base_url,
                error = %err,
                "refresco del catálogo fallido; se conserva la caché"
            );
        }
        true
    }

    /// Lanza un refresco en segundo plano si no hay otro en vuelo.
    ///
    /// Fast-path con `try_lock`: si el lock está ocupado (ya hay un refresco en
    /// curso, p. ej. el ticker o un `ensure` anterior) no se crea una tarea
    /// nueva; el refresco en vuelo ya cubrirá el TTL vencido.
    fn spawn_refresh(self: &Arc<Self>) {
        if self.refresh_lock.try_lock().is_err() {
            return;
        }
        #[cfg(test)]
        self.spawns.fetch_add(1, Ordering::Relaxed);
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.refresh_guarded().await;
        });
    }

    /// Refrescos en segundo plano lanzados (solo para tests).
    #[cfg(test)]
    fn spawn_count(&self) -> usize {
        self.spawns.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Evento de catálogo (`gateway_catalog`)
// ---------------------------------------------------------------------------

/// Factor de normalización de precios: el catálogo los trae en USD por token y
/// el evento se expresa en **USD por millón de tokens**.
const TOKENS_PER_MILLION: f64 = 1_000_000.0;

/// Precio de un modelo en el último snapshot observado (USD por token).
#[derive(Debug, Clone, Copy)]
struct TrackedPrice {
    /// Precio de entrada por token.
    pin: f64,
    /// Precio de salida por token.
    pout: f64,
    /// Longitud de contexto declarada en ese snapshot.
    context_length: Option<u32>,
}

/// Naturaleza del cambio de un modelo respecto al último snapshot del catálogo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogChangeKind {
    /// El modelo aparece por primera vez en el catálogo observado.
    New,
    /// El `blend` del modelo subió respecto al snapshot anterior.
    PriceUp,
    /// El `blend` del modelo bajó respecto al snapshot anterior.
    PriceDown,
    /// El modelo desapareció del catálogo.
    Removed,
}

impl std::fmt::Display for CatalogChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::New => "new",
            Self::PriceUp => "price_up",
            Self::PriceDown => "price_down",
            Self::Removed => "removed",
        };
        f.write_str(text)
    }
}

/// Evento estructurado `gateway_catalog`: un cambio de precio o de presencia
/// de un modelo, emitido solo cuando algo cambia (dedup contra el último
/// snapshot, D2 del change).
///
/// Los precios (`pin`, `pout`, `blend`, `prev_pin`, `prev_pout`) se normalizan
/// del catálogo (USD por token) a **USD por millón de tokens**; `blend` es
/// `0.75 · pin + 0.25 · pout`, la misma ponderación con la que
/// [`crate::classify::blend_price`] ordena candidatos.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEvent {
    /// Tipo de evento del ledger: siempre `catalog`.
    pub event_type: &'static str,
    /// Momento de la observación, en ms epoch.
    pub ts: u64,
    /// Identificador del modelo (p. ej. `acme/flash-a`).
    pub model_id: String,
    /// Organización proveedora: prefijo de `model_id` antes de `/`.
    pub org: String,
    /// Precio de entrada en USD/Mtok; nulo en un modelo `removed`.
    pub pin: Option<f64>,
    /// Precio de salida en USD/Mtok; nulo en un modelo `removed`.
    pub pout: Option<f64>,
    /// `0.75 · pin + 0.25 · pout` en USD/Mtok; nulo en un modelo `removed`.
    pub blend: Option<f64>,
    /// Longitud de contexto declarada, si se conoce.
    pub context_length: Option<u32>,
    /// Naturaleza del cambio.
    pub change_kind: CatalogChangeKind,
    /// Precio de entrada previo, si el modelo ya estaba en el snapshot anterior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_pin: Option<f64>,
    /// Precio de salida previo, si el modelo ya estaba en el snapshot anterior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_pout: Option<f64>,
}

/// Último snapshot de precios por `model_id` y detector de cambios (D2).
///
/// Deduplicación en memoria del evento `gateway_catalog`: un precio idéntico
/// entre refrescos consecutivos no vuelve a emitir. Tras reiniciar, el
/// snapshot completo se reemite como `new`, lo que es barato y esperado.
#[derive(Debug, Default)]
pub struct PriceTracker {
    /// Último precio observado por identificador de modelo.
    last: RwLock<HashMap<String, TrackedPrice>>,
}

impl PriceTracker {
    /// Crea un tracker sin snapshot previo: la primera observación emite `new`
    /// para cada modelo con precio.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Observa un catálogo recién refrescado y devuelve los eventos de cambio.
    ///
    /// Emite un evento por modelo que aparece (`new`), desaparece (`removed`)
    /// o cambia de precio (`price_up`/`price_down`); nada para los que repiten
    /// precio. Los modelos presentes sin precio parseable no alteran el último
    /// snapshot conocido ni emiten: «desconocido» no es «gratis».
    pub fn observe(&self, catalog: &[ModelEntry]) -> Vec<CatalogEvent> {
        let mut last = self
            .last
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (events, next) = diff_prices(&last, catalog, now_ms());
        *last = next;
        events
    }
}

/// Emite los eventos de catálogo como logs estructurados con `event_type`
/// `catalog` (D5): `tracing` los envía a OpenObserve vía el stack OTLP de
/// [`crate::telemetry`]. Es el punto de extensión para cambiar de transport
/// sin tocar la detección de cambios.
fn emit_catalog_events(events: &[CatalogEvent]) {
    for event in events {
        tracing::info!(
            event_type = event.event_type,
            ts = event.ts,
            model_id = %event.model_id,
            org = %event.org,
            pin = event.pin,
            pout = event.pout,
            blend = event.blend,
            context_length = event.context_length,
            change_kind = %event.change_kind,
            prev_pin = event.prev_pin,
            prev_pout = event.prev_pout,
            "gateway_catalog"
        );
    }
}

/// Compara `catalog` contra el snapshot `last` y produce los eventos junto con
/// el nuevo snapshot. Función pura para poder testear cada transición.
fn diff_prices(
    last: &HashMap<String, TrackedPrice>,
    catalog: &[ModelEntry],
    ts: u64,
) -> (Vec<CatalogEvent>, HashMap<String, TrackedPrice>) {
    let mut events = Vec::new();
    let mut next: HashMap<String, TrackedPrice> = HashMap::with_capacity(catalog.len());
    for model in catalog {
        let id = model.id.as_str();
        let Some((pin, pout)) = price_per_token(model) else {
            // Presente sin precio parseable: «desconocido» no es «gratis». Se
            // retiene el último precio conocido sin emitir, para que un precio
            // ilegible no produzca una baja falsa ni una oscilación
            // `removed`→`new` al reaparecer el precio.
            if let Some(prev) = last.get(id) {
                next.insert(id.to_owned(), *prev);
            }
            continue;
        };
        let context_length = model.context_length;
        let (change_kind, prev_pin, prev_pout) = match last.get(id) {
            None => (CatalogChangeKind::New, None, None),
            Some(prev) if prev.pin == pin && prev.pout == pout => {
                next.insert(
                    id.to_owned(),
                    TrackedPrice {
                        pin,
                        pout,
                        context_length,
                    },
                );
                continue;
            }
            Some(prev) => (
                change_kind_of(prev, pin, pout),
                Some(prev.pin),
                Some(prev.pout),
            ),
        };
        next.insert(
            id.to_owned(),
            TrackedPrice {
                pin,
                pout,
                context_length,
            },
        );
        events.push(catalog_event(
            id,
            ts,
            change_kind,
            Some((pin, pout)),
            context_length,
            prev_pin.zip(prev_pout),
        ));
    }
    // Con la retención anterior, `next` contiene exactamente los ids presentes
    // en el catálogo (con precio parseable o retenidos): `removed` solo
    // alcanza a los ids realmente ausentes del catálogo.
    let mut removed: Vec<(&String, &TrackedPrice)> = last
        .iter()
        .filter(|(id, _)| !next.contains_key(id.as_str()))
        .collect();
    removed.sort_unstable_by_key(|(id, _)| *id);
    for (id, prev) in removed {
        events.push(catalog_event(
            id,
            ts,
            CatalogChangeKind::Removed,
            None,
            prev.context_length,
            Some((prev.pin, prev.pout)),
        ));
    }
    (events, next)
}

/// Construye un evento `gateway_catalog` con los precios normalizados a
/// USD/Mtok. `current` es el precio nuevo (`None` en una baja) y `prev` el del
/// snapshot anterior (`None` en un alta).
fn catalog_event(
    model_id: &str,
    ts: u64,
    change_kind: CatalogChangeKind,
    current: Option<(f64, f64)>,
    context_length: Option<u32>,
    prev: Option<(f64, f64)>,
) -> CatalogEvent {
    let to_mtok = |price: (f64, f64)| (TOKENS_PER_MILLION * price.0, TOKENS_PER_MILLION * price.1);
    let current = current.map(to_mtok);
    let prev = prev.map(to_mtok);
    CatalogEvent {
        event_type: "catalog",
        ts,
        model_id: model_id.to_owned(),
        org: org_of(model_id).to_owned(),
        pin: current.map(|(pin, _)| pin),
        pout: current.map(|(_, pout)| pout),
        blend: current.map(|(pin, pout)| blend_parts(pin, pout)),
        context_length,
        change_kind,
        prev_pin: prev.map(|(pin, _)| pin),
        prev_pout: prev.map(|(_, pout)| pout),
    }
}

// ---------------------------------------------------------------------------
// Evento de selección (`gateway_selection`)
// ---------------------------------------------------------------------------

/// Naturaleza del cambio de selección de un `(route, backend)` de pago.
///
/// Vocabulario del contrato `gateway_selection` (delta del change
/// `add-observability-ledger`): `initial`, `cheaper`, `failover`, `removed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionReason {
    /// Primera elección observada para el `(route, backend)`.
    Initial,
    /// Tras `reselect_interval_ms`, el más barato mejoró al menos
    /// `min_price_delta` y releva al pegajoso.
    Cheaper,
    /// El modelo pegajoso quedó en blacklist y deja de ser elegible.
    Failover,
    /// El modelo pegajoso desapareció del catálogo.
    Removed,
}

impl std::fmt::Display for SelectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Initial => "initial",
            Self::Cheaper => "cheaper",
            Self::Failover => "failover",
            Self::Removed => "removed",
        };
        f.write_str(text)
    }
}

/// Evento estructurado `gateway_selection`: un cambio del modelo elegido de un
/// `(route, backend)` de pago, emitido solo cuando el modelo cambia (D2).
///
/// Los precios (`pin`, `pout`) se normalizan del catálogo (USD por token) a
/// **USD por millón de tokens**, igual que [`CatalogEvent`], y son los del
/// modelo **recién elegido**.
#[derive(Debug, Clone, Serialize)]
pub struct SelectionEvent {
    /// Tipo de evento del ledger: siempre `selection`.
    pub event_type: &'static str,
    /// Momento de la elección, en ms epoch.
    pub ts: u64,
    /// Nombre de la ruta (`route.name` de la configuración).
    pub route: String,
    /// `base_url` del backend de pago.
    pub backend: String,
    /// Identificador del modelo recién elegido.
    pub model_id: String,
    /// Naturaleza del cambio.
    pub reason: SelectionReason,
    /// Precio de entrada del modelo elegido, en USD/Mtok.
    pub pin: Option<f64>,
    /// Precio de salida del modelo elegido, en USD/Mtok.
    pub pout: Option<f64>,
    /// Modelo elegido antes del cambio, si lo había.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_model_id: Option<String>,
}

/// Determina la razón del cambio de selección entre el modelo elegido antes
/// (`prev`) y el elegido ahora (`chosen`). Función pura para poder testear
/// cada transición.
///
/// Devuelve `None` cuando no hay cambio: un mismo modelo no emite evento. La
/// elegibilidad previa se juzga contra `catalog` (presencia real en el
/// catálogo) y `candidates` (elegibles tras blacklist): el pegajoso que sigue
/// en el catálogo pero no entre los candidatos cayó en blacklist
/// (`failover`); el que ya no está en el catálogo es una baja (`removed`).
fn selection_reason(
    prev: Option<&str>,
    chosen: &str,
    catalog: &[ModelEntry],
    candidates: &[String],
) -> Option<SelectionReason> {
    let _ = (prev, chosen, catalog, candidates);
    None
}

/// Construye el evento `gateway_selection` de un cambio de selección, con los
/// precios del modelo recién elegido normalizados a USD/Mtok. `None` cuando no
/// hay cambio de modelo.
fn selection_event(
    route: &str,
    backend: &str,
    prev: Option<&str>,
    chosen: &str,
    catalog: &[ModelEntry],
    candidates: &[String],
    ts: u64,
) -> Option<SelectionEvent> {
    let _ = (route, backend, prev, chosen, catalog, candidates, ts);
    None
}

/// Emite los eventos de selección como logs estructurados con `event_type`
/// `selection` (D5): `tracing` los envía a OpenObserve vía el stack OTLP de
/// [`crate::telemetry`]. Es el punto de extensión para cambiar de transport
/// sin tocar la detección de cambios.
fn emit_selection_events(_events: &[SelectionEvent]) {}

/// Clasifica un cambio de precio por la variación del `blend`; si empata (sube
/// una pata y baja la otra en la misma proporción), decide el lado del `pin`.
fn change_kind_of(prev: &TrackedPrice, pin: f64, pout: f64) -> CatalogChangeKind {
    let blend = blend_parts(pin, pout);
    let prev_blend = blend_parts(prev.pin, prev.pout);
    if blend > prev_blend {
        CatalogChangeKind::PriceUp
    } else if blend < prev_blend {
        CatalogChangeKind::PriceDown
    } else if pin > prev.pin {
        CatalogChangeKind::PriceUp
    } else {
        CatalogChangeKind::PriceDown
    }
}

/// `blend = 0.75 · pin + 0.25 · pout`, la ponderación de
/// [`crate::classify::blend_price`] aplicada a precios numéricos.
fn blend_parts(pin: f64, pout: f64) -> f64 {
    PROMPT_WEIGHT * pin + COMPLETION_WEIGHT * pout
}

/// Precios por token (`prompt`, `completion`) de un modelo, o `None` si falta
/// alguno, no es numérico o no es finito. A diferencia de
/// `classify::price_value`, un valor ausente no cuenta como `0`.
fn price_per_token(model: &ModelEntry) -> Option<(f64, f64)> {
    let pricing = model.pricing.as_ref()?;
    Some((
        price_field(pricing.prompt.as_ref())?,
        price_field(pricing.completion.as_ref())?,
    ))
}

/// Parsea un campo de precio (cadena o número) a `f64`. Los valores no finitos
/// (`NaN`, `inf`) se tratan como no parseables: contaminarían el `blend` (y un
/// `blend` `NaN` no empata consigo mismo, rompiendo la deduplicación de D2).
fn price_field(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(number)) => number.as_f64().filter(|v| v.is_finite()),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}

/// Organización de un identificador de modelo: el prefijo antes de `/`
/// (`acme/flash-a` → `acme`). Sin barra, el propio identificador.
fn org_of(model_id: &str) -> &str {
    model_id.split_once('/').map_or(model_id, |(org, _)| org)
}

/// Instante actual en ms epoch (`0` si el reloj del sistema va detrás del epoch).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
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
        let mut entries = self.write();
        entries.insert(model.to_owned(), until);
        self.purge_and_report(&mut entries);
    }

    /// `true` si `model` está actualmente en blacklist (purgando expirados).
    pub fn is_blocked(&self, model: &str) -> bool {
        let mut entries = self.write();
        self.purge_and_report(&mut entries);
        entries.contains_key(model)
    }

    /// Número de modelos actualmente en blacklist (purgando expirados).
    pub fn len(&self) -> usize {
        let mut entries = self.write();
        self.purge_and_report(&mut entries)
    }

    /// `true` si la blacklist está vacía.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Devuelve los modelos de `models` que **no** están en blacklist.
    pub fn retain_eligible(&self, models: &[String]) -> Vec<String> {
        let mut entries = self.write();
        self.purge_and_report(&mut entries);
        models
            .iter()
            .filter(|model| !entries.contains_key(*model))
            .cloned()
            .collect()
    }

    /// Purga los cooldowns vencidos y refleja el tamaño resultante en el gauge.
    ///
    /// Se invoca en cada acceso, de modo que el gauge
    /// `llm_gateway_discovery_blacklist` deja de contar un modelo en cuanto
    /// expira su cooldown, no solo cuando se marca.
    fn purge_and_report(&self, entries: &mut HashMap<String, Instant>) -> usize {
        purge_expired(entries);
        let len = entries.len();
        self.metrics.set_blacklist_size(len);
        len
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

/// `blend` de un modelo del catálogo por identificador (`0` si no está).
///
/// La clasificación de pago devuelve identificadores; el precio observado se
/// recupera del catálogo cacheado en el momento de la elección.
fn blend_of(catalog: &[ModelEntry], id: &str) -> f64 {
    catalog
        .iter()
        .find(|model| model.id == id)
        .and_then(|model| model.pricing.as_ref())
        .map_or(0.0, blend_price)
}

/// Mejora relativa de `cheaper` frente al precio observado `current`.
///
/// Es la magnitud que se compara con `min_price_delta`: `0.20` exige que el
/// candidato sea al menos un 20 % más barato. Sin precio de referencia positivo
/// (un modelo sin precios no es elegible, pero la observación puede ser antigua),
/// cualquier candidato más barato cuenta como mejora total.
fn relative_price_improvement(current: f64, cheaper: f64) -> f64 {
    if current > 0.0 {
        (current - cheaper) / current
    } else if cheaper < current {
        1.0
    } else {
        0.0
    }
}

/// Tier de un backend dinámico.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// Tier free (`openrouter_free`): sujeto al gate global de cuota diaria.
    Free,
    /// Tier de pago (`openrouter_paid`): lista curada, orden por precio y
    /// selección pegajosa con histéresis.
    Paid,
}

/// Backend dinámico registrado, con su estado de rotación.
struct DynamicBackend {
    profile: Profile,
    base_url: String,
    min_context: Option<u32>,
    cooldown: Duration,
    overrides: Option<ClassifyOverrides>,
    /// Tier dinámico del backend.
    tier: Tier,
    /// `true` si respeta el gate global de cuota diaria.
    respect_daily_quota: bool,
    /// Lista curada de patrones glob (solo backends de pago).
    allow: Vec<String>,
    /// Cadencia mínima de reselección del modelo de pago.
    reselect_interval: Duration,
    /// Mejora relativa mínima de precio exigida para cambiar de modelo de pago.
    min_price_delta: f64,
    rotation: AtomicUsize,
}

impl DynamicBackend {
    /// Contexto de clasificación derivado del estado del backend.
    fn classify_context(&self) -> ClassifyContext<'_> {
        ClassifyContext::new(
            self.profile,
            self.min_context,
            self.overrides.as_ref(),
            &self.base_url,
        )
    }
}

/// Objetivo del sondeo de cuota: una `base_url` dinámica y su API key.
struct QuotaTarget {
    base_url: String,
    api_key_env: Option<String>,
    interval: Duration,
}

/// Selección pegajosa de un backend de pago.
///
/// Guarda el modelo elegido, el `blend` con el que se eligió y cuándo se eligió;
/// el estado no persiste entre reinicios (arranca vacío).
#[derive(Debug, Clone)]
struct PaidSelection {
    /// Identificador del modelo elegido.
    model: String,
    /// `blend` observado en el momento de la elección.
    blend: f64,
    /// Instante de la elección.
    chosen_at: Instant,
}

/// Registro de cachés de catálogo, una por `base_url` dinámica.
///
/// Agrupa el descubrimiento de todos los backends `openrouter_free` de la
/// configuración en un único `/models` por `base_url` (los perfiles comparten
/// catálogo) y mantiene la blacklist global, el estado de rotación y el gate
/// global de cuota diaria free.
pub struct Registry {
    client: reqwest::Client,
    catalogs: HashMap<String, Arc<CatalogCache>>,
    dynamic: Vec<DynamicBackend>,
    /// Posición en `dynamic` del backend dinámico `(route, backend)`.
    index: HashMap<(usize, usize), usize>,
    blacklist: Arc<Blacklist>,
    metrics: Arc<Metrics>,
    /// Deadline del gate global de cuota free agotada; `None` si está cerrado.
    free_quota_exhausted_until: RwLock<Option<Instant>>,
    /// `true` si algún backend dinámico respeta la cuota diaria.
    respect_daily_quota: bool,
    /// Objetivos del sondeo proactivo, uno por `base_url` que respeta la cuota.
    quota_targets: Vec<QuotaTarget>,
    /// Selección pegajosa por `(route, backend)` de los backends de pago.
    paid_selections: RwLock<HashMap<(usize, usize), PaidSelection>>,
    /// Patrones de `allow` ya avisados como huérfanos, por `(base_url, patrón)`.
    ///
    /// Evita emitir el mismo aviso en cada expansión (hot path): un patrón solo
    /// se avisa una vez hasta el siguiente reinicio.
    warned_allow_patterns: Mutex<HashSet<(String, String)>>,
}

impl Registry {
    /// Construye el registro a partir de la configuración y el cliente HTTP.
    pub fn new(config: &GatewayConfig, client: reqwest::Client, metrics: Arc<Metrics>) -> Self {
        let mut catalogs: HashMap<String, Arc<CatalogCache>> = HashMap::new();
        let mut dynamic = Vec::new();
        let mut index = HashMap::new();
        let mut quota_targets: Vec<QuotaTarget> = Vec::new();
        let mut respect_daily_quota = false;

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
                let respect = backend.is_free_tier()
                    && discovery.is_none_or(|discovery| discovery.respect_daily_quota);
                if respect {
                    respect_daily_quota = true;
                    let interval = discovery.map_or(
                        Duration::from_millis(crate::config::DEFAULT_QUOTA_POLL_INTERVAL_MS),
                        |discovery| discovery.quota_poll_interval(),
                    );
                    match quota_targets
                        .iter_mut()
                        .find(|target| target.base_url == backend.base_url)
                    {
                        Some(existing) if existing.interval != interval => {
                            // El sondeo es único por `base_url`: si dos backends
                            // declaran intervalos distintos se toma el menor (el
                            // más conservador) y se avisa. No es un error.
                            existing.interval = existing.interval.min(interval);
                            tracing::warn!(
                                base_url = %backend.base_url,
                                "quota_poll_interval_ms incoherente por base_url; se toma el menor"
                            );
                        }
                        Some(_) => {}
                        None => quota_targets.push(QuotaTarget {
                            base_url: backend.base_url.clone(),
                            api_key_env: backend.api_key_env.clone(),
                            interval,
                        }),
                    }
                }
                index.insert((route_idx, backend_idx), dynamic.len());
                dynamic.push(DynamicBackend {
                    profile: backend.profile(),
                    base_url: backend.base_url.clone(),
                    min_context: discovery.and_then(|discovery| discovery.min_context),
                    cooldown: discovery
                        .map(|discovery| discovery.blacklist_cooldown())
                        .unwrap_or(Duration::from_millis(
                            crate::config::DEFAULT_BLACKLIST_COOLDOWN_MS,
                        )),
                    overrides: discovery.and_then(|discovery| discovery.overrides.clone()),
                    tier: if backend.is_free_tier() {
                        Tier::Free
                    } else {
                        Tier::Paid
                    },
                    respect_daily_quota: respect,
                    allow: discovery
                        .map(|discovery| discovery.allow.clone())
                        .unwrap_or_default(),
                    reselect_interval: discovery.map_or(
                        Duration::from_millis(crate::config::DEFAULT_RESELECT_INTERVAL_MS),
                        |discovery| discovery.reselect_interval(),
                    ),
                    min_price_delta: discovery
                        .map_or(crate::config::DEFAULT_MIN_PRICE_DELTA, |discovery| {
                            discovery.min_price_delta
                        }),
                    rotation: AtomicUsize::new(0),
                });
            }
        }

        Self {
            client,
            catalogs,
            dynamic,
            index,
            blacklist: Arc::new(Blacklist::new(Arc::clone(&metrics))),
            metrics,
            free_quota_exhausted_until: RwLock::new(None),
            respect_daily_quota,
            quota_targets,
            paid_selections: RwLock::new(HashMap::new()),
            warned_allow_patterns: Mutex::new(HashSet::new()),
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

    /// `true` si algún backend dinámico respeta la cuota diaria free.
    pub fn respect_daily_quota(&self) -> bool {
        self.respect_daily_quota
    }

    /// Intervalo del objetivo de sondeo de `base_url` (solo tests).
    #[cfg(test)]
    fn quota_interval(&self, base_url: &str) -> Option<Duration> {
        self.quota_targets
            .iter()
            .find(|target| target.base_url == base_url)
            .map(|target| target.interval)
    }

    /// `true` si el backend free `(route, backend)` se saltará por el gate abierto.
    ///
    /// Exige que el backend respete la cuota, que exista catálogo para su
    /// `base_url` (si no, no habría aportado candidatos de todos modos) y que el
    /// gate esté abierto. Lo usa `expand_backends` para contar **un** salto por
    /// petición y ruta, no uno por backend free.
    pub fn free_quota_skip(&self, route: usize, backend: usize) -> bool {
        let Some(entry) = self.dynamic(route, backend) else {
            return false;
        };
        entry.tier == Tier::Free
            && entry.respect_daily_quota
            && self.catalogs.contains_key(&entry.base_url)
            && self.quota_gate_open()
    }

    /// Abre el gate global de cuota free hasta `deadline`.
    ///
    /// No hace nada si ningún backend respeta la cuota o si `deadline` ya venció.
    /// El deadline se acota a [`MAX_QUOTA_GATE`].
    ///
    /// Política deliberada de «conservar el deadline más lejano»: si el gate ya
    /// está abierto, un `429` con un deadline anterior **no** lo acorta. Un mismo
    /// agotamiento puede llegar por varias rutas y `base_url` con resets
    /// ligeramente distintos; acortar por el primero en llegar cerraría el gate
    /// antes de que la cuota se recupere de verdad, así que se conserva el
    /// vencimiento más lejano. El sondeo es quien puede cerrarlo antes (ver
    /// [`Registry::close_quota_gate`]).
    pub fn open_quota_gate(&self, deadline: Instant) {
        if !self.respect_daily_quota {
            return;
        }
        let now = Instant::now();
        if deadline <= now {
            return;
        }
        let deadline = deadline.min(now + MAX_QUOTA_GATE);
        let mut guard = self
            .free_quota_exhausted_until
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none_or(|existing| existing < deadline) {
            *guard = Some(deadline);
        }
    }

    /// `true` si el gate de cuota free está abierto.
    ///
    /// El camino rápido (gate cerrado o deadline vigente) solo toma el lock de
    /// **lectura**; únicamente cuando el deadline ha vencido se promueve a
    /// escritura para purgarlo (doble comprobación).
    pub fn quota_gate_open(&self) -> bool {
        let now = Instant::now();
        {
            let guard = self
                .free_quota_exhausted_until
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match *guard {
                Some(deadline) if deadline > now => return true,
                None => return false,
                Some(_) => {}
            }
        }

        let mut guard = self
            .free_quota_exhausted_until
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *guard {
            Some(deadline) if deadline > Instant::now() => true,
            Some(_) => {
                *guard = None;
                false
            }
            None => false,
        }
    }

    /// Cierra el gate de cuota free si estaba abierto.
    ///
    /// Lo usa el sondeo: si vuelve a observar cuota disponible (`remaining > 0`)
    /// —por ejemplo justo tras el reset a las 00:00 UTC— el gate debe cerrarse de
    /// inmediato en lugar de bloquear el tier free hasta el deadline.
    pub fn close_quota_gate(&self) {
        let mut guard = self
            .free_quota_exhausted_until
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = None;
    }

    /// Backend dinámico registrado en `(route, backend)`, si lo hay.
    fn dynamic(&self, route: usize, backend: usize) -> Option<&DynamicBackend> {
        self.index
            .get(&(route, backend))
            .and_then(|position| self.dynamic.get(*position))
    }

    /// Cooldown configurado para el backend dinámico `(route, backend)`.
    pub fn cooldown(&self, route: usize, backend: usize) -> Option<Duration> {
        self.dynamic(route, backend).map(|entry| entry.cooldown)
    }

    /// Expande el backend dinámico `(route, backend)` a los modelos concretos
    /// elegibles, en orden de preferencia.
    ///
    /// En un backend `openrouter_free` son los modelos free clasificados para su
    /// perfil, excluyendo los que están en blacklist y aplicando rotación
    /// round-robin entre peticiones sucesivas. En un backend `openrouter_paid`
    /// son los modelos de la lista curada ordenados por precio, con selección
    /// pegajosa y sin rotación.
    ///
    /// Devuelve una lista vacía si el backend no es dinámico, no hay catálogo o
    /// el perfil no aporta candidatos.
    pub async fn expand(&self, route: usize, backend: usize) -> Vec<String> {
        let Some(entry) = self.dynamic(route, backend) else {
            return Vec::new();
        };
        if entry.tier == Tier::Free && entry.respect_daily_quota && self.quota_gate_open() {
            // El conteo del salto lo hace `expand_backends` una vez por petición y
            // ruta; aquí `expand` se ejecuta una vez por backend.
            return Vec::new();
        }
        let Some(cache) = self.catalogs.get(&entry.base_url) else {
            return Vec::new();
        };
        let Some(catalog) = cache.ensure().await else {
            return Vec::new();
        };

        match entry.tier {
            Tier::Free => self.expand_free(entry, &catalog),
            Tier::Paid => self.expand_paid(route, backend, entry, &catalog),
        }
    }

    /// Expansión del tier free: clasificación por perfil más rotación round-robin.
    fn expand_free(&self, entry: &DynamicBackend, catalog: &[ModelEntry]) -> Vec<String> {
        let eligible = eligible_models(catalog);
        let ordered = classify(&eligible, entry.classify_context());
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

    /// Expansión del tier de pago: lista curada, orden por precio y selección
    /// pegajosa con histéresis, sin rotación.
    ///
    /// Devuelve el modelo elegido al frente y el resto en orden de precio como
    /// failover. Un modelo en blacklist o ausente del catálogo deja de ser
    /// elegible y se cambia de inmediato al más barato disponible.
    fn expand_paid(
        &self,
        route: usize,
        backend: usize,
        entry: &DynamicBackend,
        catalog: &[ModelEntry],
    ) -> Vec<String> {
        self.warn_unmatched_allow(entry, catalog);

        let context = PaidContext::new(entry.min_context, &entry.allow);
        let candidates = self
            .blacklist
            .retain_eligible(&classify_paid(catalog, context));

        let Some(cheapest) = candidates.first().cloned() else {
            // Sin candidatos elegibles (lista curada vacía, sin coincidencias o
            // todo en blacklist) no hay elección que conservar.
            self.write_selections().remove(&(route, backend));
            return Vec::new();
        };

        let mut selections = self.write_selections();
        let sticky = selections.get(&(route, backend)).cloned();
        let cheapest_blend = blend_of(catalog, &cheapest);

        let chosen = match &sticky {
            Some(selection) if selection.model != cheapest => {
                let still_eligible = candidates.iter().any(|id| id == &selection.model);
                let due = selection.chosen_at.elapsed() >= entry.reselect_interval;
                let enough = relative_price_improvement(selection.blend, cheapest_blend)
                    >= entry.min_price_delta;
                if still_eligible && !(due && enough) {
                    selection.model.clone()
                } else {
                    cheapest.clone()
                }
            }
            _ => cheapest.clone(),
        };

        // El instante de elección solo se reinicia cuando cambia el modelo: el
        // refresco de precios no debe reiniciar la histéresis.
        let chosen_at = match &sticky {
            Some(selection) if selection.model == chosen => selection.chosen_at,
            _ => Instant::now(),
        };
        let observed = blend_of(catalog, &chosen);
        selections.insert(
            (route, backend),
            PaidSelection {
                model: chosen.clone(),
                blend: observed,
                chosen_at,
            },
        );
        drop(selections);

        // El elegido al frente; el resto, en orden de precio, es el failover.
        let mut ordered: Vec<String> = candidates.into_iter().filter(|id| id != &chosen).collect();
        ordered.insert(0, chosen);
        ordered
    }

    /// Avisa de los patrones de `allow` sin ninguna coincidencia en el catálogo.
    ///
    /// Emite **un único** `tracing::warn!` por `(base_url, patrón)`: el par queda
    /// registrado en `warned_allow_patterns` y no se vuelve a avisar en las
    /// expansiones siguientes. Así la lista curada a medio migrar no inunda el log
    /// en el hot path ni es un error de arranque.
    fn warn_unmatched_allow(&self, entry: &DynamicBackend, catalog: &[ModelEntry]) {
        if entry.allow.is_empty() {
            return;
        }
        let mut warned = self
            .warned_allow_patterns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for pattern in &entry.allow {
            let matched = catalog
                .iter()
                .any(|model| matches_allow(&model.id, std::slice::from_ref(pattern)));
            if !matched && warned.insert((entry.base_url.clone(), pattern.clone())) {
                tracing::warn!(
                    pattern = %pattern,
                    base_url = %entry.base_url,
                    "discovery.allow: patrón sin coincidencias en el catálogo"
                );
            }
        }
    }

    /// Toma el lock de escritura de las selecciones de pago, ignorando un posible
    /// envenenamiento.
    fn write_selections(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, HashMap<(usize, usize), PaidSelection>> {
        self.paid_selections
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Identificadores de modelos dinámicos elegibles (para `/v1/models`),
    /// deduplicados, sin rotación ni filtrado por blacklist.
    ///
    /// Solo se anuncian los modelos del tier free: los ids de la lista curada de
    /// pago no son enrutables como `model` de una petición.
    ///
    /// La deduplicación conserva el orden de primera aparición entre backends y
    /// entre llamadas con el mismo catálogo.
    pub async fn eligible_model_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for entry in &self.dynamic {
            if entry.tier != Tier::Free {
                continue;
            }
            let Some(cache) = self.catalogs.get(&entry.base_url) else {
                continue;
            };
            let Some(catalog) = cache.ensure().await else {
                continue;
            };
            let eligible = eligible_models(&catalog);
            for id in classify(&eligible, entry.classify_context()) {
                if !seen.contains(&id) {
                    seen.insert(id.clone());
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
                    // Pasa por el mismo guardián que el refresco on-demand para
                    // no solapar dos fetches de `/models`.
                    cache.refresh_guarded().await;
                }
            }));
        }
        handles
    }

    /// Lanza una tarea de sondeo de cuota free por `base_url` que respeta la
    /// cuota, siguiendo el patrón de [`Registry::spawn_refresh`]: primer tick
    /// inmediato y después cada `quota_poll_interval_ms`, con
    /// [`MissedTickBehavior::Delay`].
    ///
    /// No lanza ninguna tarea si el gate está deshabilitado o no hay objetivos.
    /// Un fallo del sondeo nunca altera el gate (stale-while-error).
    pub fn spawn_quota_poll(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        for target in &self.quota_targets {
            if target.interval.is_zero() {
                continue;
            }
            let this = Arc::clone(self);
            let target = QuotaTarget {
                base_url: target.base_url.clone(),
                api_key_env: target.api_key_env.clone(),
                interval: target.interval,
            };
            handles.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(target.interval);
                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    this.poll_quota_once(&target).await;
                }
            }));
        }
        handles
    }

    /// Ejecuta un ciclo de sondeo de cuota para `target`.
    ///
    /// Registra el gauge con el último `remaining` observado y abre el gate si
    /// llega a `0` (deadline: siguiente `00:00 UTC`). Un fallo se registra pero
    /// conserva el estado del gate.
    async fn poll_quota_once(&self, target: &QuotaTarget) {
        let Some(api_key) = resolve_poll_key(target.api_key_env.as_deref()) else {
            return;
        };
        match fetch_free_quota(
            &self.client,
            &target.base_url,
            api_key.as_deref(),
            QUOTA_TIMEOUT,
        )
        .await
        {
            Ok(Some(remaining)) => {
                self.metrics
                    .record_free_quota_remaining(&target.base_url, remaining);
                if remaining == 0 {
                    if let Some(deadline) = quota_deadline(None) {
                        self.open_quota_gate(deadline);
                        tracing::warn!(
                            base_url = %target.base_url,
                            "cuota diaria free agotada (sondeo); gate abierto"
                        );
                    }
                } else if self.quota_gate_open() {
                    // La cuota volvió (p. ej. justo tras el reset a las 00:00 UTC):
                    // se cierra el gate de inmediato en vez de esperar al deadline.
                    self.close_quota_gate();
                    tracing::info!(
                        base_url = %target.base_url,
                        remaining,
                        "cuota diaria free recuperada (sondeo); gate cerrado"
                    );
                }
            }
            Ok(None) => {
                tracing::debug!(
                    base_url = %target.base_url,
                    "GET /key sin free_model_daily_requests; se conserva el estado"
                );
            }
            Err(err) => {
                // Stale-while-error: el fallo no altera el gate.
                tracing::warn!(
                    base_url = %target.base_url,
                    error = %err,
                    "sondeo de cuota free fallido; se conserva el estado"
                );
            }
        }
    }
}

/// Resuelve la API key del sondeo de cuota a partir de su variable de entorno.
///
/// - `None` → `Some(None)`: endpoint sin autenticación.
/// - `Some(var)` definida y no vacía → `Some(Some(key))`.
/// - `Some(var)` ausente o vacía → `None`: se omite el sondeo (el backend
///   tampoco podría usarla).
fn resolve_poll_key(api_key_env: Option<&str>) -> Option<Option<String>> {
    match api_key_env {
        None => Some(None),
        Some(var) => match std::env::var(var) {
            Ok(value) if !value.is_empty() => Some(Some(value)),
            _ => {
                tracing::debug!(env_var = %var, "api_key_env sin resolver; se omite el sondeo");
                None
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry;

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

    /// Valor de la serie `name` (sin etiquetas) en el render de `metrics`.
    fn gauge_value(metrics: &Metrics, name: &str) -> String {
        metrics
            .render()
            .lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.rsplit(' ').next())
            .unwrap_or_default()
            .to_owned()
    }

    /// Toma el lock compartido del gauge global de blacklist (sin etiquetas).
    fn blacklist_gauge_guard() -> crate::metrics::BlacklistGaugeGuard {
        crate::metrics::BlacklistGaugeGuard::lock()
    }

    /// M3: el gauge de blacklist vuelve a 0 al expirar el cooldown.
    #[test]
    fn blacklist_gauge_returns_to_zero_after_cooldown() {
        let _guard = blacklist_gauge_guard();
        let metrics = metrics();
        let blacklist = Blacklist::new(Arc::clone(&metrics));

        blacklist.mark("m", Duration::from_millis(50));
        assert_eq!(
            gauge_value(&metrics, "llm_gateway_discovery_blacklist"),
            "1",
            "tras marcar debe reflejarse en el gauge"
        );

        std::thread::sleep(Duration::from_millis(90));

        // Cualquier acceso purga y debe reflejar la expiración en el gauge.
        assert!(!blacklist.is_blocked("m"));
        assert_eq!(
            gauge_value(&metrics, "llm_gateway_discovery_blacklist"),
            "0",
            "el gauge debe volver a 0 tras expirar el cooldown"
        );
    }

    #[test]
    fn blacklist_marks_and_expires() {
        let _guard = blacklist_gauge_guard();
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
        let _guard = blacklist_gauge_guard();
        let blacklist = Blacklist::new(metrics());
        blacklist.mark("b", Duration::from_secs(60));
        let models = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(blacklist.retain_eligible(&models), vec!["a", "c"]);
    }

    #[test]
    fn blacklist_is_shared_through_arc() {
        let _guard = blacklist_gauge_guard();
        let blacklist = Arc::new(Blacklist::new(metrics()));
        let other = Arc::clone(&blacklist);
        blacklist.mark("m", Duration::from_secs(60));
        assert!(other.is_blocked("m"), "la blacklist es global/compartida");
    }

    /// G6: el gauge solo se reescribe cuando cambia el tamaño de la blacklist.
    #[test]
    fn blacklist_size_updates_only_on_change() {
        let _guard = blacklist_gauge_guard();
        let metrics = metrics();
        let blacklist = Blacklist::new(Arc::clone(&metrics));

        assert_eq!(blacklist.len(), 0);
        assert_eq!(
            metrics.blacklist_size_updates(),
            0,
            "una blacklist vacía no reescribe el 0 ya publicado"
        );

        blacklist.mark("m", Duration::from_millis(500));
        assert_eq!(
            metrics.blacklist_size_updates(),
            1,
            "marcar un modelo escribe el nuevo tamaño"
        );

        let _ = blacklist.len();
        let _ = blacklist.is_blocked("m");
        let _ = blacklist.retain_eligible(&["m".to_owned()]);
        assert_eq!(
            metrics.blacklist_size_updates(),
            1,
            "accesos sin cambio de tamaño no reescriben"
        );

        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(blacklist.len(), 0);
        assert_eq!(
            metrics.blacklist_size_updates(),
            2,
            "la expiración vuelve a escribir el tamaño"
        );
    }

    /// G6: marca, expiración y nueva marca actualizan el gauge correctamente.
    #[test]
    fn blacklist_gauge_tracks_mark_expire_and_remark() {
        let _guard = blacklist_gauge_guard();
        let metrics = metrics();
        let blacklist = Blacklist::new(Arc::clone(&metrics));

        blacklist.mark("a", Duration::from_millis(100));
        assert_eq!(
            gauge_value(&metrics, "llm_gateway_discovery_blacklist"),
            "1"
        );

        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(blacklist.len(), 0);
        assert_eq!(
            gauge_value(&metrics, "llm_gateway_discovery_blacklist"),
            "0",
            "la expiración del cooldown refleja el tamaño real"
        );

        blacklist.mark("b", Duration::from_secs(60));
        assert_eq!(
            gauge_value(&metrics, "llm_gateway_discovery_blacklist"),
            "1",
            "una nueva marca tras la expiración se publica"
        );
    }

    /// Caché con `base_url` inválido y una instantánea ya vencida (TTL 0), para
    /// ejercitar la rama de refresco en segundo plano sin tocar la red.
    fn expired_cache() -> Arc<CatalogCache> {
        let cache = Arc::new(CatalogCache::new(
            reqwest::Client::new(),
            "http://127.0.0.1:1/v1".to_owned(),
            Duration::ZERO,
            metrics(),
        ));
        *cache
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Snapshot {
            models: Arc::new(Vec::new()),
            fetched_at: Instant::now(),
        });
        cache
    }

    /// A3: con el lock de refresco ocupado, `ensure` no crea una tarea nueva.
    #[tokio::test]
    async fn ensure_does_not_spawn_refresh_while_lock_is_held() {
        let cache = expired_cache();
        let _held = cache.refresh_lock.lock().await;

        let before = cache.spawn_count();
        let _ = cache.ensure().await;

        assert_eq!(
            cache.spawn_count(),
            before,
            "con el lock ocupado no debe lanzarse una tarea nueva"
        );
    }

    /// A3: sin refresco en vuelo, `ensure` sí lanza un refresco en segundo plano.
    #[tokio::test]
    async fn ensure_spawns_refresh_when_idle() {
        let cache = expired_cache();

        let before = cache.spawn_count();
        let _ = cache.ensure().await;

        assert_eq!(
            cache.spawn_count(),
            before + 1,
            "sin refresco en vuelo debe lanzarse uno"
        );
    }

    /// N3: `eligible_model_ids` deduplica modelos solapados entre perfiles y
    /// mantiene el orden de primera aparición entre llamadas.
    #[tokio::test]
    async fn eligible_model_ids_dedups_and_is_stable() {
        let yaml = r#"
route:
  - name: eligible-test
    backend:
      - kind: openrouter_free
        base_url: "http://a.test/v1"
        profile: code
      - kind: openrouter_free
        base_url: "http://b.test/v1"
        profile: writer
"#;
        let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");
        let registry = Registry::new(&config, reqwest::Client::new(), metrics());
        let free = |id: &str| entry(id, &["tools", "tool_choice"], json!("0"), json!("0"));
        registry
            .catalog("http://a.test/v1")
            .expect("caché del primer backend")
            .seed(vec![free("acme/coder-x:free"), free("shared/model:free")]);
        registry
            .catalog("http://b.test/v1")
            .expect("caché del segundo backend")
            .seed(vec![free("acme/coder-y:free"), free("shared/model:free")]);

        let first = registry.eligible_model_ids().await;
        let second = registry.eligible_model_ids().await;

        assert_eq!(
            first,
            vec![
                "acme/coder-x:free",
                "shared/model:free",
                "acme/coder-y:free"
            ],
            "orden de primera aparición: primero el perfil code y luego lo nuevo del writer"
        );
        assert_eq!(first, second, "el orden debe ser estable entre llamadas");
        assert_eq!(
            first
                .iter()
                .filter(|id| id.as_str() == "shared/model:free")
                .count(),
            1,
            "un modelo solapado solo aparece una vez"
        );
    }

    /// N3: el índice `(route, backend)` resuelve cada backend dinámico en su
    /// ruta sin confundir posiciones homónimas de rutas distintas.
    #[test]
    fn registry_index_resolves_dynamic_backends_per_route() {
        let yaml = r#"
route:
  - name: r0
    backend:
      - base_url: "http://static-0.test/v1"
        model: "s0"
      - kind: openrouter_free
        base_url: "http://a.test/v1"
        discovery:
          blacklist_cooldown_ms: 7000
  - name: r1
    backend:
      - kind: openrouter_free
        base_url: "http://b.test/v1"
        discovery:
          blacklist_cooldown_ms: 9000
      - base_url: "http://static-1.test/v1"
        model: "s1"
"#;
        let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");
        let registry = Registry::new(&config, reqwest::Client::new(), metrics());

        assert_eq!(
            registry.cooldown(0, 1),
            Some(Duration::from_millis(7000)),
            "ruta 0: el dinámico ocupa el slot 1"
        );
        assert_eq!(
            registry.cooldown(1, 0),
            Some(Duration::from_millis(9000)),
            "ruta 1: el dinámico ocupa el slot 0"
        );
        assert_ne!(
            registry.cooldown(0, 1),
            registry.cooldown(1, 0),
            "cada entrada devuelve su propio cooldown"
        );
        assert_eq!(
            registry.cooldown(0, 0),
            None,
            "un slot estático no está registrado"
        );
        assert_eq!(
            registry.cooldown(1, 1),
            None,
            "un slot estático no está registrado"
        );
    }

    /// Milisegundos epoch UTC dentro de `delta` a partir de ahora.
    fn epoch_ms_in(delta: Duration) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("reloj posterior a 1970");
        u64::try_from((now + delta).as_millis()).expect("epoch en ms cabe en u64")
    }

    /// Config mínima con un backend dinámico sobre `base_url`, para el gate.
    fn quota_config(base_url: &str) -> GatewayConfig {
        let yaml = format!(
            r#"
route:
  - name: quota-unit
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base_url}"
        api_key_env: "OPENROUTER_API_KEY"
"#
        );
        serde_saphyr::from_str(&yaml).expect("config de test válida")
    }

    #[test]
    fn key_url_joins_slashes() {
        assert_eq!(
            key_url("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1/key"
        );
        assert_eq!(
            key_url("https://openrouter.ai/api/v1/"),
            "https://openrouter.ai/api/v1/key"
        );
    }

    #[test]
    fn free_quota_remaining_parses_nested_payload() {
        let nested = json!({
            "data": {"free_model_daily_requests": {"used": 1003, "limit": 1000, "remaining": 0}}
        });
        assert_eq!(free_quota_remaining(&nested), Some(0));

        let top = json!({"free_model_daily_requests": {"remaining": "250"}});
        assert_eq!(free_quota_remaining(&top), Some(250));

        let missing = json!({"data": {"label": "sin cuota"}});
        assert_eq!(free_quota_remaining(&missing), None);
    }

    #[test]
    fn quota_deadline_falls_back_to_next_utc_midnight() {
        let deadline = quota_deadline(None).expect("sin cabecera hay deadline");
        let delta = deadline.saturating_duration_since(Instant::now()).as_secs();
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("reloj posterior a 1970")
            .as_secs();
        let expected = 86_400 - (now_unix % 86_400);
        assert!(
            delta.abs_diff(expected) <= 1,
            "el deadline debe ser el siguiente 00:00 UTC: delta={delta}s expected={expected}s"
        );
    }

    #[test]
    fn quota_deadline_uses_reset_epoch_ms() {
        let deadline = quota_deadline(Some(epoch_ms_in(Duration::from_secs(6 * 3600))))
            .expect("un reset futuro abre el gate");
        let delta = deadline.saturating_duration_since(Instant::now());
        assert!(
            delta > Duration::from_secs(6 * 3600 - 5) && delta < Duration::from_secs(6 * 3600 + 5),
            "el deadline debe seguir el reset, got {delta:?}"
        );
    }

    #[test]
    fn quota_deadline_caps_at_24h() {
        let deadline = quota_deadline(Some(epoch_ms_in(Duration::from_secs(10 * 24 * 3600))))
            .expect("un reset futuro abre el gate");
        let delta = deadline.saturating_duration_since(Instant::now());
        assert!(
            delta <= MAX_QUOTA_GATE && delta > MAX_QUOTA_GATE - Duration::from_secs(5),
            "el deadline debe acotarse a 24 h, got {delta:?}"
        );
    }

    #[test]
    fn quota_deadline_past_reset_is_none() {
        let past = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("reloj posterior a 1970")
            .as_millis()
            .saturating_sub(60_000);
        let past = u64::try_from(past).expect("epoch en ms cabe en u64");
        assert_eq!(
            quota_deadline(Some(past)),
            None,
            "un reset en el pasado no abre el gate"
        );
    }

    #[test]
    fn quota_gate_opens_and_expires() {
        let _guard = blacklist_gauge_guard();
        let registry = Registry::new(
            &quota_config("http://quota-gate.test/v1"),
            reqwest::Client::new(),
            metrics(),
        );
        assert!(!registry.quota_gate_open(), "el gate arranca cerrado");

        registry.open_quota_gate(Instant::now() + Duration::from_millis(50));
        assert!(registry.quota_gate_open(), "el gate debe abrirse");

        std::thread::sleep(Duration::from_millis(90));
        assert!(!registry.quota_gate_open(), "el gate debe expirar");
    }

    #[test]
    fn quota_gate_stays_closed_when_respect_is_false() {
        let _guard = blacklist_gauge_guard();
        let base = "http://quota-disabled.test/v1";
        let yaml = format!(
            r#"
route:
  - name: quota-disabled-unit
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          respect_daily_quota: false
"#
        );
        let config: GatewayConfig = serde_saphyr::from_str(&yaml).expect("config de test válida");
        let registry = Registry::new(&config, reqwest::Client::new(), metrics());
        assert!(!registry.respect_daily_quota());

        registry.open_quota_gate(Instant::now() + Duration::from_secs(60));
        assert!(!registry.quota_gate_open(), "el gate no debe abrirse");
    }

    #[tokio::test]
    async fn expand_returns_zero_candidates_with_gate_open() {
        let _guard = blacklist_gauge_guard();
        let base = "http://quota-expand.test/v1";
        let registry = Registry::new(&quota_config(base), reqwest::Client::new(), metrics());
        registry
            .catalog(base)
            .expect("caché registrada")
            .seed(vec![entry(
                "dyn/aa:free",
                &["tools", "tool_choice"],
                json!("0"),
                json!("0"),
            )]);

        assert_eq!(
            registry.expand(0, 0).await.len(),
            1,
            "sin gate debe haber candidato"
        );

        registry.open_quota_gate(Instant::now() + Duration::from_millis(50));
        assert!(
            registry.expand(0, 0).await.is_empty(),
            "con el gate abierto no debe haber candidatos free"
        );

        std::thread::sleep(Duration::from_millis(90));
        assert_eq!(
            registry.expand(0, 0).await.len(),
            1,
            "al expirar el gate el free vuelve a proponerse"
        );
    }

    /// `base_url` del backend de pago de los tests.
    const PAID_BASE: &str = "http://paid.test/v1";

    /// Config mínima con un backend de pago sobre `base_url`.
    fn paid_config(base_url: &str, discovery: &str) -> GatewayConfig {
        let yaml = format!(
            r#"
route:
  - name: paid-unit
    backend:
      - kind: openrouter_paid
        profile: code
        base_url: "{base_url}"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
{discovery}
"#
        );
        serde_saphyr::from_str(&yaml).expect("config de test válida")
    }

    /// Modelo de pago con precios, contexto y tool-calling explícitos.
    fn paid_entry(id: &str, prompt: &str, completion: &str, context_length: u32) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            name: None,
            description: None,
            context_length: Some(context_length),
            pricing: Some(Pricing {
                prompt: Some(json!(prompt)),
                completion: Some(json!(completion)),
            }),
            supported_parameters: Some(vec!["tools".to_owned(), "tool_choice".to_owned()]),
        }
    }

    /// Bloque `discovery` (indentado) con la lista curada de los tests.
    fn paid_discovery(extra: &str) -> String {
        format!("          allow: [\"acme/flash-*\"]\n{extra}")
    }

    /// Registry con un backend de pago sobre `base_url` y su catálogo precargado.
    fn paid_registry(base_url: &str, discovery: &str, models: Vec<ModelEntry>) -> Registry {
        let registry = Registry::new(
            &paid_config(base_url, discovery),
            reqwest::Client::new(),
            metrics(),
        );
        registry
            .catalog(base_url)
            .expect("caché del backend de pago")
            .seed(models);
        registry
    }

    /// Modelo de pago barato (`blend` 0.04).
    fn paid_cheap() -> ModelEntry {
        paid_entry("acme/flash-a", "0.04", "0.04", 8_000)
    }

    /// Modelo de pago caro (`blend` 0.1).
    fn paid_expensive() -> ModelEntry {
        paid_entry("acme/flash-b", "0.1", "0.1", 128_000)
    }

    /// Scenario 5.4: sin rotación, dos expansiones devuelven el mismo orden.
    #[tokio::test]
    async fn paid_expand_returns_the_same_order_on_consecutive_expansions() {
        let _guard = blacklist_gauge_guard();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery(""),
            vec![paid_expensive(), paid_cheap()],
        );

        let first = registry.expand(0, 0).await;
        let second = registry.expand(0, 0).await;

        assert_eq!(
            first,
            vec!["acme/flash-a".to_owned(), "acme/flash-b".to_owned()],
            "orden por precio ascendente"
        );
        assert_eq!(
            first, second,
            "sin round-robin el primer candidato no cambia entre expansiones"
        );
    }

    /// Scenario: Sin coincidencias con la lista curada el backend aporta cero
    /// candidatos.
    #[tokio::test]
    async fn paid_expand_with_unmatched_pattern_returns_no_candidates() {
        let _guard = blacklist_gauge_guard();
        let registry = paid_registry(
            PAID_BASE,
            "          allow: [\"lab-inexistente/modelo-x\"]\n",
            vec![paid_cheap()],
        );

        assert!(
            registry.expand(0, 0).await.is_empty(),
            "un patrón huérfano no aporta candidatos"
        );
    }

    /// Scenario 5.7: sin `allow` el backend de pago aporta cero candidatos.
    #[tokio::test]
    async fn paid_expand_without_allow_returns_no_candidates() {
        let _guard = blacklist_gauge_guard();
        let registry = paid_registry(
            PAID_BASE,
            "          min_context: 1000\n",
            vec![paid_cheap()],
        );

        assert!(
            registry.expand(0, 0).await.is_empty(),
            "sin lista curada no hay candidatos de pago"
        );
    }

    /// Scenario: Tras reinicio elige el más barato (sin estado previo).
    #[tokio::test]
    async fn paid_selection_first_expansion_picks_cheapest() {
        let _guard = blacklist_gauge_guard();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery(""),
            vec![paid_expensive(), paid_cheap()],
        );

        let order = registry.expand(0, 0).await;

        assert_eq!(
            order[0], "acme/flash-a",
            "la primera expansión elige el modelo más barato de la lista curada"
        );
    }

    /// Scenario: Mantiene el modelo dentro de `reselect_interval_ms` aunque el
    /// catálogo refrescado traiga uno más barato.
    #[tokio::test]
    async fn paid_selection_keeps_sticky_model_within_interval() {
        let _guard = blacklist_gauge_guard();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery("          reselect_interval_ms: 600000\n"),
            vec![paid_cheap(), paid_expensive()],
        );
        assert_eq!(registry.expand(0, 0).await[0], "acme/flash-a");

        // Refresco del catálogo: `B` pasa a ser un 30 % más barato que `A`.
        registry
            .catalog(PAID_BASE)
            .expect("caché del backend de pago")
            .seed(vec![
                paid_cheap(),
                paid_entry("acme/flash-b", "0.028", "0.028", 128_000),
            ]);

        let order = registry.expand(0, 0).await;

        assert_eq!(
            order[0], "acme/flash-a",
            "dentro del intervalo el modelo pegajoso se mantiene"
        );
        assert_eq!(
            order[1], "acme/flash-b",
            "el más barato queda como failover en orden de precio"
        );
    }

    /// Scenario: cambia tras `reselect_interval_ms` si la mejora es suficiente.
    #[tokio::test]
    async fn paid_selection_switches_after_interval_with_enough_improvement() {
        let _guard = blacklist_gauge_guard();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery("          reselect_interval_ms: 50\n"),
            vec![paid_cheap(), paid_expensive()],
        );
        assert_eq!(registry.expand(0, 0).await[0], "acme/flash-a");

        registry
            .catalog(PAID_BASE)
            .expect("caché del backend de pago")
            .seed(vec![
                paid_cheap(),
                paid_entry("acme/flash-b", "0.028", "0.028", 128_000),
            ]);
        tokio::time::sleep(Duration::from_millis(90)).await;

        let order = registry.expand(0, 0).await;

        assert_eq!(
            order[0], "acme/flash-b",
            "pasado el intervalo, un 30 % más barato releva al pegajoso"
        );
        assert_eq!(order[1], "acme/flash-a");
    }

    /// Scenario: no cambia si la mejora es marginal (5 %).
    #[tokio::test]
    async fn paid_selection_keeps_sticky_on_marginal_improvement() {
        let _guard = blacklist_gauge_guard();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery("          reselect_interval_ms: 50\n"),
            vec![paid_cheap(), paid_expensive()],
        );
        assert_eq!(registry.expand(0, 0).await[0], "acme/flash-a");

        registry
            .catalog(PAID_BASE)
            .expect("caché del backend de pago")
            .seed(vec![
                paid_cheap(),
                paid_entry("acme/flash-b", "0.038", "0.038", 128_000),
            ]);
        tokio::time::sleep(Duration::from_millis(90)).await;

        let order = registry.expand(0, 0).await;

        assert_eq!(
            order[0], "acme/flash-a",
            "una mejora por debajo de min_price_delta no justifica cambiar"
        );
    }

    /// Scenario: cambio inmediato si el pegajoso cae en blacklist.
    #[tokio::test]
    async fn paid_selection_switches_immediately_when_sticky_is_blacklisted() {
        let _guard = blacklist_gauge_guard();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery("          reselect_interval_ms: 600000\n"),
            vec![paid_cheap(), paid_expensive()],
        );
        assert_eq!(registry.expand(0, 0).await[0], "acme/flash-a");

        registry
            .blacklist()
            .mark("acme/flash-a", Duration::from_secs(60));

        let order = registry.expand(0, 0).await;

        assert_eq!(
            order,
            vec!["acme/flash-b".to_owned()],
            "el pegajoso blacklisteado deja de ser elegible"
        );
    }

    /// Scenario: cambio inmediato si el pegajoso desaparece del catálogo.
    #[tokio::test]
    async fn paid_selection_switches_immediately_when_sticky_disappears() {
        let _guard = blacklist_gauge_guard();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery("          reselect_interval_ms: 600000\n"),
            vec![paid_cheap(), paid_expensive()],
        );
        assert_eq!(registry.expand(0, 0).await[0], "acme/flash-a");

        registry
            .catalog(PAID_BASE)
            .expect("caché del backend de pago")
            .seed(vec![paid_expensive()]);

        let order = registry.expand(0, 0).await;

        assert_eq!(
            order,
            vec!["acme/flash-b".to_owned()],
            "si el pegajoso sale del catálogo se pasa de inmediato al más barato"
        );
    }

    /// Capa mínima de `tracing` que acumula los eventos `WARN` como texto.
    #[derive(Clone)]
    struct WarnLayer {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl<S: Subscriber> Layer<S> for WarnLayer {
        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            if event.metadata().level() != &Level::WARN {
                return;
            }
            let mut visitor = WarnVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .expect("lock captured warnings")
                .push(visitor.fields.join(" "));
        }
    }

    /// Visitante que serializa los campos del evento como `campo=valor`.
    #[derive(Default)]
    struct WarnVisitor {
        fields: Vec<String>,
    }

    impl Visit for WarnVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields.push(format!("{}={value:?}", field.name()));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }

    /// Captura los avisos `WARN` solo para el hilo actual.
    fn capture_warnings() -> (Arc<Mutex<Vec<String>>>, tracing::subscriber::DefaultGuard) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = registry().with(WarnLayer {
            events: Arc::clone(&events),
        });
        (events, tracing::subscriber::set_default(subscriber))
    }

    /// Scenario 2: el aviso de un patrón huérfano se emite una sola vez por
    /// `(base_url, patrón)`, no en cada expansión.
    #[tokio::test]
    async fn paid_expand_warns_once_per_unmatched_pattern() {
        let _guard = blacklist_gauge_guard();
        let (events, _capture) = capture_warnings();
        let registry = paid_registry(
            PAID_BASE,
            "          allow: [\"lab-inexistente/modelo-x\", \"acme/flash-*\"]\n",
            vec![paid_cheap()],
        );

        assert_eq!(
            registry.expand(0, 0).await,
            vec!["acme/flash-a".to_owned()],
            "el patrón con coincidencias aporta su candidato"
        );
        let _ = registry.expand(0, 0).await;

        let warnings = events.lock().expect("lock captured warnings").clone();
        let unmatched: Vec<&String> = warnings
            .iter()
            .filter(|warning| warning.contains("lab-inexistente/modelo-x"))
            .collect();
        assert_eq!(
            unmatched.len(),
            1,
            "un único aviso por patrón huérfano: {warnings:?}"
        );
        assert!(
            unmatched[0].contains(PAID_BASE),
            "el aviso debe incluir la base_url: {}",
            unmatched[0]
        );
    }

    /// Con todas las coincidencias no se emite ningún aviso.
    #[tokio::test]
    async fn paid_expand_does_not_warn_when_all_patterns_match() {
        let _guard = blacklist_gauge_guard();
        let (events, _capture) = capture_warnings();
        let registry = paid_registry(PAID_BASE, &paid_discovery(""), vec![paid_cheap()]);

        let order = registry.expand(0, 0).await;
        let _ = registry.expand(0, 0).await;

        assert_eq!(order, vec!["acme/flash-a".to_owned()]);
        assert!(
            events.lock().expect("lock captured warnings").is_empty(),
            "sin patrones huérfanos no hay avisos"
        );
    }

    /// Scenario 7: dos backends free con la misma `base_url` y distinto
    /// `quota_poll_interval_ms` comparten un objetivo con el menor intervalo.
    #[test]
    fn quota_targets_dedupe_by_base_url_taking_min_interval() {
        let _guard = blacklist_gauge_guard();
        let base = "http://quota-dedupe.test/v1";
        let yaml = format!(
            r#"
route:
  - name: quota-dedupe-a
    backend:
      - kind: openrouter_free
        profile: code
        base_url: "{base}"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          quota_poll_interval_ms: 5000
  - name: quota-dedupe-b
    backend:
      - kind: openrouter_free
        profile: writer
        base_url: "{base}"
        api_key_env: "OPENROUTER_API_KEY"
        discovery:
          quota_poll_interval_ms: 1000
"#
        );
        let config: GatewayConfig = serde_saphyr::from_str(&yaml).expect("config de test válida");
        let registry = Registry::new(&config, reqwest::Client::new(), metrics());

        assert_eq!(
            registry.quota_interval(base),
            Some(Duration::from_millis(1000)),
            "la deduplicación por base_url debe conservar el menor intervalo"
        );
    }

    // ------------------------------------------------------------------
    // Grupo 2: evento de catálogo (`gateway_catalog`)
    // ------------------------------------------------------------------

    /// Asegura que dos precios coinciden con tolerancia de punto flotante.
    fn assert_price_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "se esperaba {expected}, obtuvo {actual}"
        );
    }

    /// Modelo de pago con precios por token en cadena (formato OpenRouter).
    fn priced(id: &str, pin: &str, pout: &str) -> ModelEntry {
        paid_entry(id, pin, pout, 8_000)
    }

    /// Modelo con precios crudos como `Value` (número o cadena, válido o no).
    fn raw_priced(id: &str, prompt: Value, completion: Value) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            name: None,
            description: None,
            context_length: Some(8_000),
            pricing: Some(Pricing {
                prompt: Some(prompt),
                completion: Some(completion),
            }),
            supported_parameters: Some(vec!["tools".to_owned(), "tool_choice".to_owned()]),
        }
    }

    /// Modelo presente en el catálogo sin campo `pricing`.
    fn unpriced_entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            name: None,
            description: None,
            context_length: Some(8_000),
            pricing: None,
            supported_parameters: Some(vec!["tools".to_owned(), "tool_choice".to_owned()]),
        }
    }

    /// Transición `new`: la primera observación da de alta el modelo con sus
    /// precios normalizados a USD/Mtok y sin precios previos.
    #[test]
    fn catalog_tracker_detects_new_model() {
        let tracker = PriceTracker::new();

        let events = tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);

        assert_eq!(
            events.len(),
            1,
            "la primera observación da de alta: {events:?}"
        );
        let event = &events[0];
        assert_eq!(event.change_kind, CatalogChangeKind::New);
        assert_eq!(event.model_id, "acme/flash-a");
        assert_eq!(event.org, "acme");
        assert_eq!(event.context_length, Some(8_000));
        assert_price_close(event.pin.expect("pin nuevo"), 1.5);
        assert_price_close(event.pout.expect("pout nuevo"), 2.0);
        assert!(
            event.prev_pin.is_none(),
            "un modelo nuevo no tiene precio previo"
        );
        assert!(
            event.prev_pout.is_none(),
            "un modelo nuevo no tiene precio previo"
        );
    }

    /// Transición `price_up`: sube el precio de salida y el evento conserva el
    /// precio previo.
    #[test]
    fn catalog_tracker_detects_price_up() {
        let tracker = PriceTracker::new();
        tracker.observe(&[priced("acme/flash-a", "0.0000002", "0.0000002")]);

        let events = tracker.observe(&[priced("acme/flash-a", "0.0000002", "0.00000025")]);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].change_kind, CatalogChangeKind::PriceUp);
        assert_price_close(events[0].pout.expect("pout nuevo"), 0.25);
        assert_price_close(events[0].prev_pout.expect("pout previo"), 0.2);
    }

    /// Transición `price_down`: el escenario del spec, `pout` pasa de 0.1992 a
    /// 0.1800 USD/Mtok.
    #[test]
    fn catalog_tracker_detects_price_down() {
        let tracker = PriceTracker::new();
        tracker.observe(&[priced("acme/flash-a", "0.0000002", "0.0000001992")]);

        let events = tracker.observe(&[priced("acme/flash-a", "0.0000002", "0.00000018")]);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].change_kind, CatalogChangeKind::PriceDown);
        assert_price_close(events[0].pout.expect("pout nuevo"), 0.1800);
        assert_price_close(events[0].prev_pout.expect("pout previo"), 0.1992);
    }

    /// Transición `removed`: el modelo desaparece del catálogo y el evento
    /// conserva el último precio conocido; los precios nuevos quedan nulos.
    #[test]
    fn catalog_tracker_detects_removed_model() {
        let tracker = PriceTracker::new();
        tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);

        let events = tracker.observe(&[]);

        assert_eq!(events.len(), 1, "la baja del modelo emite: {events:?}");
        assert_eq!(events[0].change_kind, CatalogChangeKind::Removed);
        assert_eq!(events[0].model_id, "acme/flash-a");
        assert_eq!(
            events[0].pin, None,
            "un modelo removed no tiene precio nuevo"
        );
        assert_eq!(events[0].pout, None);
        assert_price_close(events[0].prev_pin.expect("pin previo"), 1.5);
        assert_price_close(events[0].prev_pout.expect("pout previo"), 2.0);
        assert_eq!(
            events[0].context_length,
            Some(8_000),
            "el contexto del último snapshot se conserva"
        );
    }

    /// Dedup (D2): un precio idéntico entre snapshots consecutivos no vuelve a
    /// emitir.
    #[test]
    fn catalog_tracker_does_not_emit_when_price_is_unchanged() {
        let tracker = PriceTracker::new();
        tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);

        let events = tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);

        assert!(
            events.is_empty(),
            "un precio sin cambios no emite: {events:?}"
        );
    }

    /// Un cambio de precio emite exactamente un evento, y solo para el modelo
    /// que cambió.
    #[test]
    fn catalog_tracker_emits_only_for_models_that_changed() {
        let tracker = PriceTracker::new();
        tracker.observe(&[
            priced("acme/flash-a", "0.0000015", "0.000002"),
            priced("acme/flash-b", "0.000001", "0.000001"),
        ]);

        let events = tracker.observe(&[
            priced("acme/flash-a", "0.0000015", "0.000002"),
            priced("acme/flash-b", "0.000001", "0.000002"),
        ]);

        assert_eq!(
            events.len(),
            1,
            "solo el modelo que cambió emite: {events:?}"
        );
        assert_eq!(events[0].model_id, "acme/flash-b");
        assert_eq!(events[0].change_kind, CatalogChangeKind::PriceUp);
    }

    /// Contrato 2.3: `blend = 0.75 · pin + 0.25 · pout` sobre los precios del
    /// evento, en USD por millón de tokens.
    #[test]
    fn catalog_event_blend_is_three_quarters_prompt_and_quarter_completion() {
        let tracker = PriceTracker::new();

        let events = tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);

        let event = &events[0];
        assert_price_close(event.pin.expect("pin"), 1.5);
        assert_price_close(event.pout.expect("pout"), 2.0);
        assert_price_close(event.blend.expect("blend"), 0.75 * 1.5 + 0.25 * 2.0);
    }

    /// El evento serializa los campos del contrato (`event_type`, `ts`,
    /// `change_kind` en minúscula…) y conserva el precio previo.
    #[test]
    fn catalog_event_serializes_contract_fields() {
        let tracker = PriceTracker::new();
        tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);

        let events = tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.0000025")]);
        let json = serde_json::to_value(&events[0]).expect("evento serializable");

        assert_eq!(json["event_type"], "catalog");
        assert_eq!(json["model_id"], "acme/flash-a");
        assert_eq!(json["org"], "acme");
        assert_eq!(json["change_kind"], "price_up");
        assert!(json["ts"].is_u64(), "ts en ms epoch");
        assert!(json["pin"].is_number());
        assert!(json["pout"].is_number());
        assert!(json["blend"].is_number());
        assert!(json["context_length"].is_number());
        assert!(json["prev_pin"].is_number());
        assert!(json["prev_pout"].is_number());
    }

    /// Un modelo `new` omite los precios previos del JSON («cuando aplique»).
    #[test]
    fn catalog_event_new_omits_prev_prices() {
        let tracker = PriceTracker::new();

        let events = tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);
        let json = serde_json::to_value(&events[0]).expect("evento serializable");

        assert!(
            json.get("prev_pin").is_none(),
            "sin precio previo no hay prev_pin"
        );
        assert!(json.get("prev_pout").is_none());
    }

    /// Un modelo `removed` serializa `pin`/`pout`/`blend` nulos y conserva los
    /// últimos precios conocidos.
    #[test]
    fn catalog_event_removed_serializes_null_prices() {
        let tracker = PriceTracker::new();
        tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);

        let events = tracker.observe(&[]);
        let json = serde_json::to_value(&events[0]).expect("evento serializable");

        assert!(json["pin"].is_null());
        assert!(json["pout"].is_null());
        assert!(json["blend"].is_null());
        assert!(json["prev_pin"].is_number());
        assert!(json["prev_pout"].is_number());
    }

    /// Un modelo presente cuyo precio deja de ser parseable (`pricing`
    /// ausente, campo no numérico o cadena no numérica) NO emite `removed`:
    /// «desconocido» no es «gratis» (D2). El último snapshot conocido se
    /// retiene hasta que vuelva un precio legible.
    #[test]
    fn catalog_tracker_retains_snapshot_when_price_becomes_unparseable() {
        let tracker = PriceTracker::new();
        tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);

        let events = tracker.observe(&[unpriced_entry("acme/flash-a")]);

        assert!(
            events.is_empty(),
            "un precio ilegible no es una baja ni un alta: {events:?}"
        );

        // El snapshot se retiene: al volver el precio idéntico no reemite.
        let events = tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);
        assert!(
            events.is_empty(),
            "el precio reaparecido idéntico no reemite: {events:?}"
        );
    }

    /// Al reaparecer un precio parseable tras uno ilegible se emite el cambio
    /// real (`price_up`/`price_down`) con los precios previos retenidos, no un
    /// `new` sin historial.
    #[test]
    fn catalog_tracker_emits_price_change_when_unparseable_price_reappears() {
        let tracker = PriceTracker::new();
        tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);
        tracker.observe(&[unpriced_entry("acme/flash-a")]);

        let events = tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.0000025")]);

        assert_eq!(
            events.len(),
            1,
            "reaparece con cambio de precio: {events:?}"
        );
        assert_eq!(
            events[0].change_kind,
            CatalogChangeKind::PriceUp,
            "no es un alta: el modelo seguía en el snapshot: {events:?}"
        );
        assert_price_close(events[0].pout.expect("pout nuevo"), 2.5);
        assert_price_close(events[0].prev_pout.expect("pout previo"), 2.0);
        assert_price_close(events[0].prev_pin.expect("pin previo"), 1.5);
    }

    /// Un precio como cadena no numérica se trata igual que uno ausente: sin
    /// emisión y con el snapshot retenido.
    #[test]
    fn catalog_tracker_treats_non_numeric_string_price_as_unparseable() {
        let tracker = PriceTracker::new();
        tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);

        let events = tracker.observe(&[raw_priced(
            "acme/flash-a",
            json!("0.0000015"),
            json!("gratis"),
        )]);

        assert!(
            events.is_empty(),
            "una cadena no numérica no emite ni borra: {events:?}"
        );
    }

    /// `NaN` e `inf` no son precios válidos: se tratan como no parseables para
    /// no propagar un `blend` no finito que rompería la deduplicación (un
    /// `blend` `NaN` no empata consigo mismo y emitiría en cada refresco).
    #[test]
    fn catalog_tracker_treats_nan_and_inf_price_as_unparseable() {
        let tracker = PriceTracker::new();

        let events =
            tracker.observe(&[raw_priced("acme/flash-a", json!("NaN"), json!("0.000002"))]);
        assert!(events.is_empty(), "NaN no es un precio: {events:?}");

        let events =
            tracker.observe(&[raw_priced("acme/flash-a", json!("inf"), json!("0.000002"))]);
        assert!(events.is_empty(), "inf no es un precio: {events:?}");

        // «Desconocido» no es «gratis»: sin precio finito conocido, la primera
        // observación válida da de alta el modelo como `new`.
        let events = tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);
        assert_eq!(
            events.len(),
            1,
            "el primer precio finito da de alta: {events:?}"
        );
        assert_eq!(events[0].change_kind, CatalogChangeKind::New);
    }

    /// Desempate en `change_kind_of`: con `blend` idéntico (sube una pata y
    /// baja la otra en la misma proporción) decide el lado del `pin`, en ambos
    /// sentidos.
    #[test]
    fn change_kind_of_tie_breaks_equal_blend_by_pin() {
        // 0.75·1.0 + 0.25·4.0 = 1.75 = 0.75·2.0 + 0.25·1.0 (blend empata).
        let prev = TrackedPrice {
            pin: 1.0,
            pout: 4.0,
            context_length: None,
        };
        assert_eq!(
            change_kind_of(&prev, 2.0, 1.0),
            CatalogChangeKind::PriceUp,
            "empate de blend con pin al alza: price_up"
        );

        // Simetría: 0.75·2.0 + 0.25·1.0 = 1.75 = 0.75·1.0 + 0.25·4.0.
        let prev = TrackedPrice {
            pin: 2.0,
            pout: 1.0,
            context_length: None,
        };
        assert_eq!(
            change_kind_of(&prev, 1.0, 4.0),
            CatalogChangeKind::PriceDown,
            "empate de blend con pin a la baja: price_down"
        );
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
            let mut visitor = WarnVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .expect("lock captured ledger logs")
                .push(visitor.fields.join(" "));
        }
    }

    /// Captura los logs `INFO` solo para el hilo actual.
    fn capture_ledger_logs() -> (Arc<Mutex<Vec<String>>>, tracing::subscriber::DefaultGuard) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = registry().with(LedgerLayer {
            events: Arc::clone(&events),
        });
        (events, tracing::subscriber::set_default(subscriber))
    }

    /// El emisor registra el evento como log estructurado con `event_type`
    /// `catalog` y los campos del contrato.
    #[test]
    fn catalog_events_are_logged_with_event_type_catalog() {
        let (captured, _guard) = capture_ledger_logs();
        let tracker = PriceTracker::new();

        let events = tracker.observe(&[priced("acme/flash-a", "0.0000015", "0.000002")]);
        emit_catalog_events(&events);

        let logs = captured.lock().expect("lock captured ledger logs").clone();
        let catalog = logs
            .iter()
            .find(|log| log.contains("event_type=catalog"))
            .expect("el evento de catálogo se emite como log estructurado");
        assert!(catalog.contains("model_id=acme/flash-a"), "{catalog}");
        assert!(catalog.contains("org=acme"), "{catalog}");
        assert!(catalog.contains("change_kind=new"), "{catalog}");
        assert!(catalog.contains("pin=1.5"), "{catalog}");
    }

    // --- Evento de selección (`gateway_selection`) ---------------------------

    /// Transición `initial`: sin selección previa, el primer elegido emite
    /// `initial`.
    #[test]
    fn selection_reason_is_initial_without_prev() {
        let catalog = vec![paid_cheap(), paid_expensive()];
        let candidates = vec!["acme/flash-a".to_owned(), "acme/flash-b".to_owned()];

        let reason = selection_reason(None, "acme/flash-a", &catalog, &candidates);

        assert_eq!(
            reason,
            Some(SelectionReason::Initial),
            "sin selección previa la razón es initial: {reason:?}"
        );
    }

    /// Transición `cheaper`: el pegajoso sigue siendo elegible pero, pasado el
    /// intervalo, el más barato mejora lo suficiente.
    #[test]
    fn selection_reason_is_cheaper_when_prev_still_eligible() {
        let catalog = vec![paid_cheap(), paid_expensive()];
        let candidates = vec!["acme/flash-b".to_owned(), "acme/flash-a".to_owned()];

        let reason = selection_reason(Some("acme/flash-a"), "acme/flash-b", &catalog, &candidates);

        assert_eq!(
            reason,
            Some(SelectionReason::Cheaper),
            "el pegajoso elegible relevado por precio es cheaper: {reason:?}"
        );
    }

    /// Transición `failover`: el pegajoso sigue en el catálogo pero ya no es
    /// elegible (cayó en blacklist).
    #[test]
    fn selection_reason_is_failover_when_prev_in_catalog_but_not_eligible() {
        let catalog = vec![paid_cheap(), paid_expensive()];
        let candidates = vec!["acme/flash-b".to_owned()];

        let reason = selection_reason(Some("acme/flash-a"), "acme/flash-b", &catalog, &candidates);

        assert_eq!(
            reason,
            Some(SelectionReason::Failover),
            "el pegajoso blacklisteado es failover: {reason:?}"
        );
    }

    /// Transición `removed`: el pegajoso desapareció del catálogo.
    #[test]
    fn selection_reason_is_removed_when_prev_absent_from_catalog() {
        let catalog = vec![paid_expensive()];
        let candidates = vec!["acme/flash-b".to_owned()];

        let reason = selection_reason(Some("acme/flash-a"), "acme/flash-b", &catalog, &candidates);

        assert_eq!(
            reason,
            Some(SelectionReason::Removed),
            "el pegajoso ausente del catálogo es removed: {reason:?}"
        );
    }

    /// Un mismo modelo elegido en refrescos consecutivos no emite evento (D2).
    #[test]
    fn selection_reason_is_none_when_model_unchanged() {
        let catalog = vec![paid_cheap(), paid_expensive()];
        let candidates = vec!["acme/flash-a".to_owned(), "acme/flash-b".to_owned()];

        let reason = selection_reason(Some("acme/flash-a"), "acme/flash-a", &catalog, &candidates);

        assert!(reason.is_none(), "sin cambio no hay razón: {reason:?}");
    }

    /// El evento incluye los campos del contrato y los precios del modelo
    /// recién elegido, normalizados a USD/Mtok.
    #[test]
    fn selection_event_builds_contract_fields() {
        let catalog = vec![paid_cheap(), paid_expensive()];
        let candidates = vec!["acme/flash-b".to_owned(), "acme/flash-a".to_owned()];

        let event = selection_event(
            "code",
            "http://paid.test/v1",
            Some("acme/flash-a"),
            "acme/flash-b",
            &catalog,
            &candidates,
            1_000,
        )
        .expect("un cambio de selección produce evento");

        assert_eq!(event.event_type, "selection");
        assert_eq!(event.ts, 1_000);
        assert_eq!(event.route, "code");
        assert_eq!(event.backend, "http://paid.test/v1");
        assert_eq!(event.model_id, "acme/flash-b");
        assert_eq!(event.reason, SelectionReason::Cheaper);
        assert_eq!(
            event.prev_model_id.as_deref(),
            Some("acme/flash-a"),
            "el modelo relevado queda como prev_model_id"
        );
        // 0.1 USD por token (`flash-b`) → 100_000 USD/Mtok.
        assert_price_close(event.pin.expect("pin del elegido"), 100_000.0);
        assert_price_close(event.pout.expect("pout del elegido"), 100_000.0);
    }

    /// El evento serializa los campos del contrato (`event_type`, `reason` en
    /// minúscula, `prev_model_id` cuando existe).
    #[test]
    fn selection_event_serializes_contract_fields() {
        let catalog = vec![paid_cheap(), paid_expensive()];
        let candidates = vec!["acme/flash-b".to_owned(), "acme/flash-a".to_owned()];

        let event = selection_event(
            "code",
            "http://paid.test/v1",
            Some("acme/flash-a"),
            "acme/flash-b",
            &catalog,
            &candidates,
            1_000,
        )
        .expect("un cambio de selección produce evento");
        let json = serde_json::to_value(&event).expect("evento serializable");

        assert_eq!(json["event_type"], "selection");
        assert_eq!(json["ts"], 1_000);
        assert_eq!(json["route"], "code");
        assert_eq!(json["backend"], "http://paid.test/v1");
        assert_eq!(json["model_id"], "acme/flash-b");
        assert_eq!(json["reason"], "cheaper");
        assert!(json["pin"].is_number());
        assert!(json["pout"].is_number());
        assert_eq!(json["prev_model_id"], "acme/flash-a");
    }

    /// Una elección `initial` omite `prev_model_id` del JSON («cuando exista»).
    #[test]
    fn selection_event_initial_omits_prev_model_id() {
        let catalog = vec![paid_cheap(), paid_expensive()];
        let candidates = vec!["acme/flash-a".to_owned(), "acme/flash-b".to_owned()];

        let event = selection_event(
            "code",
            "http://paid.test/v1",
            None,
            "acme/flash-a",
            &catalog,
            &candidates,
            1_000,
        )
        .expect("una elección inicial produce evento");
        let json = serde_json::to_value(&event).expect("evento serializable");

        assert_eq!(json["reason"], "initial");
        assert!(
            json.get("prev_model_id").is_none(),
            "sin selección previa no hay prev_model_id: {json}"
        );
    }

    /// Scenario «Elección inicial»: la primera expansión emite el log
    /// `selection` con `reason: initial`, la ruta y el backend, y sin
    /// `prev_model_id`.
    #[tokio::test]
    async fn selection_event_logged_on_first_expansion() {
        let _guard = blacklist_gauge_guard();
        let (captured, _logs) = capture_ledger_logs();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery(""),
            vec![paid_expensive(), paid_cheap()],
        );

        registry.expand(0, 0).await;

        let logs = captured.lock().expect("lock captured ledger logs").clone();
        let selection = logs
            .iter()
            .find(|log| log.contains("event_type=selection"))
            .expect("el evento de selección se emite como log estructurado");
        assert!(selection.contains("reason=initial"), "{selection}");
        assert!(selection.contains("route=paid-unit"), "{selection}");
        assert!(
            selection.contains("backend=http://paid.test/v1"),
            "{selection}"
        );
        assert!(selection.contains("model_id=acme/flash-a"), "{selection}");
        assert!(
            !selection.contains("prev_model_id=acme"),
            "sin selección previa no hay prev_model_id: {selection}"
        );
    }

    /// Scenario «Cambio por modelo más barato»: tras `reselect_interval_ms`, un
    /// candidato claramente más barato emite `reason: cheaper` con
    /// `prev_model_id`.
    #[tokio::test]
    async fn selection_event_reason_cheaper_is_logged() {
        let _guard = blacklist_gauge_guard();
        let (captured, _logs) = capture_ledger_logs();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery("          reselect_interval_ms: 50\n"),
            vec![paid_cheap(), paid_expensive()],
        );
        assert_eq!(registry.expand(0, 0).await[0], "acme/flash-a");

        // Refresco del catálogo: `B` pasa a ser un 30 % más barato que `A`.
        registry
            .catalog(PAID_BASE)
            .expect("caché del backend de pago")
            .seed(vec![
                paid_cheap(),
                paid_entry("acme/flash-b", "0.028", "0.028", 128_000),
            ]);
        tokio::time::sleep(Duration::from_millis(90)).await;

        registry.expand(0, 0).await;

        let logs = captured.lock().expect("lock captured ledger logs").clone();
        let selection = logs
            .iter()
            .rev()
            .find(|log| log.contains("event_type=selection"))
            .expect("el cambio de selección se emite");
        assert!(selection.contains("reason=cheaper"), "{selection}");
        assert!(selection.contains("model_id=acme/flash-b"), "{selection}");
        assert!(
            selection.contains("prev_model_id=acme/flash-a"),
            "{selection}"
        );
    }

    /// Scenario «Cambio por failover»: el pegajoso cae en blacklist y se emite
    /// `reason: failover` con `prev_model_id`.
    #[tokio::test]
    async fn selection_event_reason_failover_is_logged() {
        let _guard = blacklist_gauge_guard();
        let (captured, _logs) = capture_ledger_logs();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery("          reselect_interval_ms: 600000\n"),
            vec![paid_cheap(), paid_expensive()],
        );
        assert_eq!(registry.expand(0, 0).await[0], "acme/flash-a");

        registry
            .blacklist()
            .mark("acme/flash-a", Duration::from_secs(60));

        registry.expand(0, 0).await;

        let logs = captured.lock().expect("lock captured ledger logs").clone();
        let selection = logs
            .iter()
            .rev()
            .find(|log| log.contains("event_type=selection"))
            .expect("el cambio de selección por blacklist se emite");
        assert!(selection.contains("reason=failover"), "{selection}");
        assert!(selection.contains("model_id=acme/flash-b"), "{selection}");
        assert!(
            selection.contains("prev_model_id=acme/flash-a"),
            "{selection}"
        );
    }

    /// El pegajoso que desaparece del catálogo emite `reason: removed` con
    /// `prev_model_id`.
    #[tokio::test]
    async fn selection_event_reason_removed_is_logged() {
        let _guard = blacklist_gauge_guard();
        let (captured, _logs) = capture_ledger_logs();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery("          reselect_interval_ms: 600000\n"),
            vec![paid_cheap(), paid_expensive()],
        );
        assert_eq!(registry.expand(0, 0).await[0], "acme/flash-a");

        registry
            .catalog(PAID_BASE)
            .expect("caché del backend de pago")
            .seed(vec![paid_expensive()]);

        registry.expand(0, 0).await;

        let logs = captured.lock().expect("lock captured ledger logs").clone();
        let selection = logs
            .iter()
            .rev()
            .find(|log| log.contains("event_type=selection"))
            .expect("el cambio de selección por baja del catálogo se emite");
        assert!(selection.contains("reason=removed"), "{selection}");
        assert!(selection.contains("model_id=acme/flash-b"), "{selection}");
        assert!(
            selection.contains("prev_model_id=acme/flash-a"),
            "{selection}"
        );
    }

    /// El modelo elegido que no cambia entre refrescos no emite evento (D2):
    /// solo cuenta la elección inicial.
    #[tokio::test]
    async fn selection_event_not_logged_when_model_unchanged() {
        let _guard = blacklist_gauge_guard();
        let (captured, _logs) = capture_ledger_logs();
        let registry = paid_registry(
            PAID_BASE,
            &paid_discovery(""),
            vec![paid_expensive(), paid_cheap()],
        );
        assert_eq!(registry.expand(0, 0).await[0], "acme/flash-a");

        // Refresco con el mismo catálogo: el elegido no cambia.
        registry
            .catalog(PAID_BASE)
            .expect("caché del backend de pago")
            .seed(vec![paid_cheap(), paid_expensive()]);
        assert_eq!(registry.expand(0, 0).await[0], "acme/flash-a");

        let logs = captured.lock().expect("lock captured ledger logs").clone();
        assert_eq!(
            logs.iter()
                .filter(|log| log.contains("event_type=selection"))
                .count(),
            1,
            "un modelo elegido sin cambios no emite: {logs:?}"
        );
    }
}
