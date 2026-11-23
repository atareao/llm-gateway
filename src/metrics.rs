//! Métricas del gateway sobre instrumentos OpenTelemetry.
//!
//! El conjunto de series es finito y se conoce al arrancar (rutas y backends de
//! `gateway.yaml`), así que en [`Metrics::new`] se **pre-registran** los
//! conjuntos de atributos de cada serie: la ruta caliente solo recorre índices
//! y llama a `add`/`record`, sin `HashMap` ni `Mutex`.
//!
//! Los instrumentos viven en el `MeterProvider` global instalado por
//! [`crate::telemetry`], que lleva dos readers: el `PrometheusExporter`
//! (siempre; sirve `GET /metrics`) y, si `signals.metrics`, un `PeriodicReader`
//! que los exporta por OTLP. Una única fuente de verdad para scrape y export.
//!
//! Los nombres de serie `llm_gateway_*` son contrato observable: los counters
//! se nombran **sin** el sufijo `_total` porque el `PrometheusExporter` lo
//! añade al renderizar.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram};
use prometheus::Encoder as _;

use crate::config::{BackendKind, GatewayConfig, Profile};
use crate::discovery::SelectionReason;
use crate::proxy::backend_id;
use crate::telemetry;

/// Límites superiores de los buckets del histograma, en segundos.
const BUCKETS: [f64; 11] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

/// Nombre del `Meter` (scope de instrumentación).
const SCOPE_NAME: &str = "llm-gateway";

const OUTCOMES: [Outcome; 2] = [Outcome::Success, Outcome::AllFailed];
const GLOBAL_OUTCOMES: [GlobalOutcome; 2] =
    [GlobalOutcome::InvalidRequest, GlobalOutcome::ModelNotFound];
const ATTEMPT_RESULTS: [AttemptResult; 7] = [
    AttemptResult::Success,
    AttemptResult::RetryableStatus,
    AttemptResult::TransportError,
    AttemptResult::FirstByteTimeout,
    AttemptResult::MissingApiKey,
    AttemptResult::BodyError,
    AttemptResult::NoToolCall,
];

/// Resultado final de una petición a `/v1/chat/completions`.
#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    Success,
    AllFailed,
}

impl Outcome {
    /// Valor de la etiqueta `outcome`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::AllFailed => "all_failed",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Success => 0,
            Self::AllFailed => 1,
        }
    }
}

/// Error detectado antes de resolver la ruta.
#[derive(Clone, Copy, Debug)]
pub enum GlobalOutcome {
    InvalidRequest,
    ModelNotFound,
}

impl GlobalOutcome {
    /// Valor de la etiqueta `kind`.
    pub fn label(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::ModelNotFound => "model_not_found",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::InvalidRequest => 0,
            Self::ModelNotFound => 1,
        }
    }
}

/// Resultado de un intento contra un backend.
#[derive(Clone, Copy, Debug)]
pub enum AttemptResult {
    Success,
    RetryableStatus,
    TransportError,
    FirstByteTimeout,
    MissingApiKey,
    BodyError,
    /// Respuesta `2xx` sin `tool_calls` cuando la petición los exigía.
    NoToolCall,
}

impl AttemptResult {
    /// Valor de la etiqueta `result`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::RetryableStatus => "retryable_status",
            Self::TransportError => "transport_error",
            Self::FirstByteTimeout => "first_byte_timeout",
            Self::MissingApiKey => "missing_api_key",
            Self::BodyError => "body_error",
            Self::NoToolCall => "no_tool_call",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Success => 0,
            Self::RetryableStatus => 1,
            Self::TransportError => 2,
            Self::FirstByteTimeout => 3,
            Self::MissingApiKey => 4,
            Self::BodyError => 5,
            Self::NoToolCall => 6,
        }
    }
}

/// Atributos pre-registrados de un backend.
struct BackendMetrics {
    /// `[result]` → atributos `route`, `backend`, `result`.
    attempts: [Vec<KeyValue>; 7],
    /// Atributos `route`, `backend`.
    duration: Vec<KeyValue>,
    /// El backend declara un bloque `warmup`.
    warmup_enabled: bool,
    /// `[success, failure]` → atributos `route`, `backend`, `result`.
    warmups: [Vec<KeyValue>; 2],
}

/// Atributos pre-registrados de una ruta.
struct RouteMetrics {
    /// `[streaming][outcome]` → atributos `route`, `streaming`, `outcome`.
    requests: [[Vec<KeyValue>; 2]; 2],
    /// Atributos `route`.
    failover: Vec<KeyValue>,
    /// `[streaming]` → atributos `route`, `streaming`.
    duration: [Vec<KeyValue>; 2],
    /// Atributos `route` del contador de saltos del tier free.
    free_quota_skips: Vec<KeyValue>,
    backends: Vec<BackendMetrics>,
}

/// Registro de métricas pre-registrado a partir de la configuración.
pub struct Metrics {
    requests: Counter<u64>,
    request_duration: Histogram<f64>,
    failover: Counter<u64>,
    backend_attempts: Counter<u64>,
    backend_duration: Histogram<f64>,
    errors: Counter<u64>,
    backend_warm: Gauge<u64>,
    warmup: Counter<u64>,
    routes: Vec<RouteMetrics>,
    /// `[kind]` → atributos `kind`.
    error_kinds: [Vec<KeyValue>; 2],
    /// Total de modelos del catálogo dinámico (por `base_url`).
    discovery_models: Gauge<u64>,
    /// Modelos free elegibles del catálogo (por `base_url`).
    discovery_free: Gauge<u64>,
    /// Modelos elegibles por perfil (por `base_url` y `profile`).
    discovery_profile: Gauge<u64>,
    /// Resultado de los refrescos del catálogo (por `base_url` y `result`).
    discovery_refresh: Counter<u64>,
    /// Tamaño actual de la blacklist global.
    discovery_blacklist: Gauge<u64>,
    /// Cuota diaria free restante observada (por `base_url`).
    ///
    /// **No** se pre-registra: solo aparece cuando el sondeo ha observado un
    /// valor, de modo que sin observación no se emite ninguna serie.
    free_quota_remaining: Gauge<u64>,
    /// Peticiones que saltaron el tier free por el gate de cuota abierto (por
    /// `route`).
    free_quota_skips: Counter<u64>,
    /// Tokens procesados por petición atendida (por `route`, `model` y
    /// `type=prompt|completion`).
    ///
    /// **No** se pre-registra: el conjunto de modelos no se conoce al arrancar
    /// (los dinámicos rotan), y una serie solo aparece con `usage` conocido.
    tokens: Counter<u64>,
    /// Coste acumulado estimado de las peticiones atendidas, en USD (por
    /// `route` y `model`), con los precios del momento de la petición.
    ///
    /// **No** se pre-registra: igual que [`Metrics::tokens`], solo hay serie
    /// cuando el coste es calculable.
    cost: Counter<f64>,
    /// Cambios del modelo elegido de una ruta, por motivo
    /// (`initial`/`cheaper`/`failover`/`removed`).
    ///
    /// **No** se pre-registra: como el resto del ledger, la serie solo aparece
    /// con el primer cambio de selección observado.
    model_selection: Counter<u64>,
    /// Precio del modelo seleccionado de una ruta, en USD/Mtok (por
    /// `side=prompt|completion`).
    ///
    /// **No** se pre-registra: sin precio conocido no hay serie (un `0` sería
    /// un precio falso).
    selected_price: Gauge<f64>,
    /// Precio `blend` (USD/Mtok) de cada modelo de la lista curada.
    ///
    /// **No** se pre-registra: la lista curada y sus precios solo se conocen
    /// con el primer snapshot del catálogo.
    allowlist_price: Gauge<f64>,
    /// Último tamaño de blacklist publicado en el gauge. Arranca en 0, que es
    /// lo que deja el pre-registro de [`Metrics::new`]; así una blacklist vacía
    /// no reescribe el gauge en cada acceso.
    ///
    /// La caché es **por instancia de `Metrics`** y asume una única instancia
    /// por proceso: la serie OTel del gauge es global, de modo que dos
    /// instancias compartirían la serie pero no este atómico.
    blacklist_size: AtomicUsize,
    /// Actualizaciones del tamaño de blacklist que reescribieron el gauge.
    ///
    /// No cuenta el `record(0)` del pre-registro de [`Metrics::new`], solo las
    /// escrituras que cambian el tamaño publicado. Solo para tests.
    #[cfg(test)]
    blacklist_size_updates: AtomicUsize,
}

/// Atributos `route`, `streaming`, `outcome`.
fn request_attrs(route: &str, streaming: bool, outcome: Outcome) -> Vec<KeyValue> {
    vec![
        KeyValue::new("route", route.to_owned()),
        KeyValue::new("streaming", streaming.to_string()),
        KeyValue::new("outcome", outcome.label()),
    ]
}

/// Atributos `route`, `streaming`.
fn duration_attrs(route: &str, streaming: bool) -> Vec<KeyValue> {
    vec![
        KeyValue::new("route", route.to_owned()),
        KeyValue::new("streaming", streaming.to_string()),
    ]
}

/// Atributos `base_url` de las métricas de descubrimiento.
fn discovery_attrs(base_url: &str) -> Vec<KeyValue> {
    vec![KeyValue::new("base_url", base_url.to_owned())]
}

/// Atributos `base_url`, `profile` de las métricas de descubrimiento.
fn discovery_profile_attrs(base_url: &str, profile: Profile) -> Vec<KeyValue> {
    vec![
        KeyValue::new("base_url", base_url.to_owned()),
        KeyValue::new("profile", profile.label()),
    ]
}

/// Atributos `base_url`, `result` de los refrescos de descubrimiento.
fn discovery_refresh_attrs(base_url: &str, ok: bool) -> Vec<KeyValue> {
    vec![
        KeyValue::new("base_url", base_url.to_owned()),
        KeyValue::new("result", if ok { "success" } else { "failure" }),
    ]
}

/// Lado del precio en los gauges del ledger: entrada (`prompt`) o salida
/// (`completion`), la misma pareja de valores del `type` de los tokens.
#[derive(Clone, Copy)]
enum LedgerSide {
    Prompt,
    Completion,
}

impl LedgerSide {
    /// Valor de la etiqueta `side` (y del `type` de los tokens).
    fn label(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Completion => "completion",
        }
    }
}

/// Alias legible para el `type` del contador de tokens: mismo vocabulario
/// (`prompt`/`completion`) que el lado de los precios.
type LedgerTokenType = LedgerSide;

/// Atributos `route`, `model`, `type` del contador de tokens.
fn ledger_tokens_attrs(route: &str, model: &str, kind: LedgerTokenType) -> Vec<KeyValue> {
    vec![
        KeyValue::new("route", route.to_owned()),
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("type", kind.label()),
    ]
}

/// Atributos `route`, `model` del contador de coste.
fn ledger_pair_attrs(route: &str, model: &str) -> Vec<KeyValue> {
    vec![
        KeyValue::new("route", route.to_owned()),
        KeyValue::new("model", model.to_owned()),
    ]
}

/// Atributos `route`, `model`, `side` del gauge de precio del seleccionado.
fn ledger_price_attrs(route: &str, model: &str, side: LedgerSide) -> Vec<KeyValue> {
    vec![
        KeyValue::new("route", route.to_owned()),
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("side", side.label()),
    ]
}

/// `base_url`s únicas de los backends dinámicos declarados en la config.
fn dynamic_base_urls(config: &GatewayConfig) -> Vec<String> {
    let mut bases: Vec<String> = config
        .route
        .iter()
        .flat_map(|route| route.backend.iter())
        .filter(|backend| backend.kind == BackendKind::OpenrouterFree)
        .map(|backend| backend.base_url.clone())
        .collect();
    bases.sort();
    bases.dedup();
    bases
}

impl Metrics {
    /// Pre-registra una serie por ruta, por backend y por resultado posible.
    ///
    /// Se registra un `0` en cada serie pre-registrada para que `/metrics`
    /// emita el conjunto completo de series desde el primer scrape, igual que
    /// hacía el registro de atómicos.
    pub fn new(config: &GatewayConfig) -> Self {
        // Sin `telemetry::init` (tests, usos embebidos) garantiza al menos el
        // reader Prometheus para que `/metrics` funcione.
        telemetry::ensure_meter_provider();
        let meter = opentelemetry::global::meter(SCOPE_NAME);

        let requests = meter
            .u64_counter("llm_gateway_requests")
            .with_description("Peticiones a /v1/chat/completions por ruta y resultado.")
            .build();
        let request_duration = meter
            .f64_histogram("llm_gateway_request_duration_seconds")
            .with_description(
                "Latencia total de las peticiones (en streaming, tiempo hasta la primera cabecera).",
            )
            .with_boundaries(BUCKETS.to_vec())
            .build();
        let failover = meter
            .u64_counter("llm_gateway_failover")
            .with_description("Peticiones que necesitaron más de un intento.")
            .build();
        let backend_attempts = meter
            .u64_counter("llm_gateway_backend_attempts")
            .with_description("Intentos contra cada backend por resultado.")
            .build();
        let backend_duration = meter
            .f64_histogram("llm_gateway_backend_duration_seconds")
            .with_description(
                "Latencia de cada intento contra un backend (en streaming, tiempo hasta la primera cabecera).",
            )
            .with_boundaries(BUCKETS.to_vec())
            .build();
        let errors = meter
            .u64_counter("llm_gateway_errors")
            .with_description("Peticiones rechazadas antes de resolver la ruta.")
            .build();
        let backend_warm = meter
            .u64_gauge("llm_gateway_backend_warm")
            .with_description("Si el último warmup del backend respondió 2xx (1) o no (0).")
            .build();
        let warmup = meter
            .u64_counter("llm_gateway_warmup")
            .with_description("Warmups ejecutados por backend y resultado.")
            .build();
        let discovery_models = meter
            .u64_gauge("llm_gateway_discovery_models")
            .with_description("Número total de modelos del catálogo dinámico.")
            .build();
        let discovery_free = meter
            .u64_gauge("llm_gateway_discovery_free_models")
            .with_description("Número de modelos free elegibles del catálogo.")
            .build();
        let discovery_profile = meter
            .u64_gauge("llm_gateway_discovery_profile_models")
            .with_description("Número de modelos elegibles por perfil.")
            .build();
        let discovery_refresh = meter
            .u64_counter("llm_gateway_discovery_refresh")
            .with_description("Refrescos del catálogo dinámico por resultado.")
            .build();
        let discovery_blacklist = meter
            .u64_gauge("llm_gateway_discovery_blacklist")
            .with_description("Número de modelos en blacklist dinámica.")
            .build();
        let free_quota_remaining = meter
            .u64_gauge("llm_gateway_free_quota_remaining")
            .with_description("Cuota diaria free restante observada por base_url.")
            .build();
        let free_quota_skips = meter
            .u64_counter("llm_gateway_free_quota_skips")
            .with_description("Peticiones que saltaron el tier free por el gate de cuota abierto.")
            .build();
        let tokens = meter
            .u64_counter("llm_gateway_tokens")
            .with_description("Tokens procesados por ruta, modelo y tipo (prompt/completion).")
            .build();
        let cost = meter
            .f64_counter("llm_gateway_cost_usd")
            .with_description(
                "Coste acumulado estimado en USD por ruta y modelo (precios del momento).",
            )
            .build();
        let model_selection = meter
            .u64_counter("llm_gateway_model_selection")
            .with_description(
                "Cambios del modelo elegido por ruta y motivo (initial/cheaper/failover/removed).",
            )
            .build();
        let selected_price = meter
            .f64_gauge("llm_gateway_selected_model_price_usd")
            .with_description(
                "Precio del modelo seleccionado por ruta en USD/Mtok (side=prompt entrada, side=completion salida).",
            )
            .build();
        let allowlist_price = meter
            .f64_gauge("llm_gateway_allowlist_price_usd")
            .with_description(
                "Precio blend (0.75·pin + 0.25·pout, USD/Mtok) de cada modelo de la lista curada.",
            )
            .build();

        let error_kinds = std::array::from_fn(|index| {
            vec![KeyValue::new("kind", GLOBAL_OUTCOMES[index].label())]
        });
        for attrs in &error_kinds {
            errors.add(0, attrs);
        }

        let routes = config
            .route
            .iter()
            .map(|route| {
                let requests = std::array::from_fn(|streaming| {
                    std::array::from_fn(|outcome| {
                        request_attrs(&route.name, streaming == 1, OUTCOMES[outcome])
                    })
                });
                let failover = vec![KeyValue::new("route", route.name.clone())];
                let duration =
                    std::array::from_fn(|streaming| duration_attrs(&route.name, streaming == 1));
                let free_quota_skips = vec![KeyValue::new("route", route.name.clone())];
                let backends = route
                    .backend
                    .iter()
                    .map(|backend| {
                        let id = backend_id(backend);
                        let attempts = std::array::from_fn(|result| {
                            vec![
                                KeyValue::new("route", route.name.clone()),
                                KeyValue::new("backend", id.clone()),
                                KeyValue::new("result", ATTEMPT_RESULTS[result].label()),
                            ]
                        });
                        let duration = vec![
                            KeyValue::new("route", route.name.clone()),
                            KeyValue::new("backend", id.clone()),
                        ];
                        let warmups = std::array::from_fn(|result| {
                            vec![
                                KeyValue::new("route", route.name.clone()),
                                KeyValue::new("backend", id.clone()),
                                KeyValue::new(
                                    "result",
                                    if result == 0 { "success" } else { "failure" },
                                ),
                            ]
                        });
                        BackendMetrics {
                            attempts,
                            duration,
                            warmup_enabled: backend.warmup.is_some(),
                            warmups,
                        }
                    })
                    .collect();
                RouteMetrics {
                    requests,
                    failover,
                    duration,
                    free_quota_skips,
                    backends,
                }
            })
            .collect::<Vec<_>>();

        // Pre-registro: emite las series en cero desde el primer scrape.
        for route in &routes {
            for attrs in route.requests.iter().flatten() {
                requests.add(0, attrs);
            }
            failover.add(0, &route.failover);
            free_quota_skips.add(0, &route.free_quota_skips);
            for backend in &route.backends {
                for attrs in &backend.attempts {
                    backend_attempts.add(0, attrs);
                }
                if backend.warmup_enabled {
                    backend_warm.record(0, &backend.duration);
                    for attrs in &backend.warmups {
                        warmup.add(0, attrs);
                    }
                }
            }
        }

        // Pre-registro de las series de descubrimiento por cada `base_url`
        // dinámica declarada, para que aparezcan en `/metrics` desde el arranque.
        let bases = dynamic_base_urls(config);
        for base in &bases {
            discovery_models.record(0, &discovery_attrs(base));
            discovery_free.record(0, &discovery_attrs(base));
            for profile in [Profile::Architect, Profile::Code, Profile::Writer] {
                discovery_profile.record(0, &discovery_profile_attrs(base, profile));
            }
            for ok in [true, false] {
                discovery_refresh.add(0, &discovery_refresh_attrs(base, ok));
            }
        }
        // La blacklist es global (sin `base_url`), así que su serie se
        // pre-registra aunque la config no declare backends dinámicos.
        //
        // En tests conviven varias instancias de `Metrics` en el mismo proceso
        // (la producción asume una sola). Si el test ya mantiene el lock del
        // gauge, se pre-registra directamente; si no, se toma solo cuando está
        // libre para no pisar una lectura concurrente. Si `try_lock` falla,
        // otro test está leyendo o escribiendo el gauge y este pre-registro se
        // omite a propósito: bloquear aquí podría interbloquear el test que
        // tiene el lock si a su vez construye un `Metrics`.
        #[cfg(test)]
        {
            let already_locked = HOLDS_BLACKLIST_GAUGE_LOCK.with(std::cell::Cell::get);
            if already_locked {
                discovery_blacklist.record(0, &[]);
            } else if let Ok(_guard) = BLACKLIST_GAUGE_LOCK.try_lock() {
                discovery_blacklist.record(0, &[]);
            }
        }
        #[cfg(not(test))]
        discovery_blacklist.record(0, &[]);

        Self {
            requests,
            request_duration,
            failover,
            backend_attempts,
            backend_duration,
            errors,
            backend_warm,
            warmup,
            routes,
            error_kinds,
            discovery_models,
            discovery_free,
            discovery_profile,
            discovery_refresh,
            discovery_blacklist,
            free_quota_remaining,
            free_quota_skips,
            tokens,
            cost,
            model_selection,
            selected_price,
            allowlist_price,
            blacklist_size: AtomicUsize::new(0),
            #[cfg(test)]
            blacklist_size_updates: AtomicUsize::new(0),
        }
    }

    /// Registra el resultado final de una petición y su latencia total.
    pub fn record_request(
        &self,
        route: usize,
        streaming: bool,
        outcome: Outcome,
        elapsed: Duration,
    ) {
        if let Some(route) = self.routes.get(route) {
            let streaming = usize::from(streaming);
            self.requests
                .add(1, &route.requests[streaming][outcome.index()]);
            self.request_duration
                .record(elapsed.as_secs_f64(), &route.duration[streaming]);
        }
    }

    /// Registra el resultado de un intento contra un backend y su latencia.
    pub fn record_attempt(
        &self,
        route: usize,
        backend: usize,
        result: AttemptResult,
        elapsed: Duration,
    ) {
        if let Some(route) = self.routes.get(route)
            && let Some(backend) = route.backends.get(backend)
        {
            self.backend_attempts
                .add(1, &backend.attempts[result.index()]);
            self.backend_duration
                .record(elapsed.as_secs_f64(), &backend.duration);
        }
    }

    /// Registra el resultado de un warmup: fija el gauge `warm` a `1` (éxito)
    /// o `0` (fallo) e incrementa el counter por resultado.
    pub fn record_warmup(&self, route: usize, backend: usize, ok: bool) {
        if let Some(route) = self.routes.get(route)
            && let Some(backend) = route.backends.get(backend)
        {
            self.backend_warm.record(u64::from(ok), &backend.duration);
            self.warmup.add(1, &backend.warmups[usize::from(!ok)]);
        }
    }

    /// Registra que una petición necesitó más de un intento.
    pub fn record_failover(&self, route: usize) {
        if let Some(route) = self.routes.get(route) {
            self.failover.add(1, &route.failover);
        }
    }

    /// Registra un rechazo anterior a la resolución de ruta.
    pub fn record_global(&self, outcome: GlobalOutcome) {
        self.errors.add(1, &self.error_kinds[outcome.index()]);
    }

    /// Registra el tamaño total y elegible del catálogo dinámico de `base_url`.
    pub fn record_discovery_catalog(&self, base_url: &str, total: usize, eligible: usize) {
        let attrs = discovery_attrs(base_url);
        self.discovery_models.record(total as u64, &attrs);
        self.discovery_free.record(eligible as u64, &attrs);
    }

    /// Registra el número de modelos elegibles de un perfil.
    pub fn record_discovery_profile(&self, base_url: &str, profile: Profile, count: usize) {
        self.discovery_profile
            .record(count as u64, &discovery_profile_attrs(base_url, profile));
    }

    /// Registra el resultado de un refresco del catálogo.
    pub fn record_discovery_refresh(&self, base_url: &str, ok: bool) {
        self.discovery_refresh
            .add(1, &discovery_refresh_attrs(base_url, ok));
    }

    /// Fija la cuota diaria free restante observada para `base_url`.
    ///
    /// Es el único punto que emite la serie `llm_gateway_free_quota_remaining`:
    /// sin observación (sondeo deshabilitado o sin `free_model_daily_requests`)
    /// no se publica ninguna serie.
    pub fn record_free_quota_remaining(&self, base_url: &str, remaining: u64) {
        self.free_quota_remaining
            .record(remaining, &discovery_attrs(base_url));
    }

    /// Registra que la ruta `route` saltó el tier free por el gate abierto.
    pub fn record_free_quota_skip(&self, route: usize) {
        if let Some(route) = self.routes.get(route) {
            self.free_quota_skips.add(1, &route.free_quota_skips);
        }
    }

    // --- Ledger de observabilidad (#39, Grupo 5): métricas -------------------

    /// Registra los tokens y el coste de una petición atendida por `model`.
    ///
    /// Solo incrementa las series con dato conocido: sin `usage` o sin precio
    /// no se añade nada (nunca se inventa un `0`; un coste `0.0` con precios
    /// conocidos es un dato legítimo). Los atributos se construyen por llamada
    /// porque el conjunto de modelos no es finito ni conocido al arrancar: es
    /// el mismo compromiso de cardinalidad acotada del ledger (`route`, `model`
    /// y `type`/`side`/`reason`; nunca `request_id`).
    pub fn record_ledger_usage(
        &self,
        route: &str,
        model: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        cost_usd: Option<f64>,
    ) {
        if let Some(tokens_in) = tokens_in {
            self.tokens.add(
                tokens_in,
                &ledger_tokens_attrs(route, model, LedgerTokenType::Prompt),
            );
        }
        if let Some(tokens_out) = tokens_out {
            self.tokens.add(
                tokens_out,
                &ledger_tokens_attrs(route, model, LedgerTokenType::Completion),
            );
        }
        if let Some(cost) = cost_usd {
            self.cost.add(cost, &ledger_pair_attrs(route, model));
        }
    }

    /// Registra un cambio del modelo elegido de una ruta, por motivo.
    ///
    /// `reason` es el enum [`SelectionReason`]: el vocabulario de 4 valores
    /// del contrato `gateway_selection` (`initial`, `cheaper`, `failover`,
    /// `removed`) es imposible de violar desde el sistema de tipos.
    pub fn record_model_selection(&self, route: &str, reason: SelectionReason) {
        self.model_selection.add(
            1,
            &[
                KeyValue::new("route", route.to_owned()),
                KeyValue::new("reason", reason.to_string()),
            ],
        );
    }

    /// Fija el precio del modelo seleccionado de una ruta, por lado.
    ///
    /// Es un gauge de estado: solo publica los lados con precio conocido y
    /// sobrescribe el último valor (no acumula). Un lado sin precio conservará
    /// el último valor publicado hasta que vuelva a conocerse: eliminar la
    /// serie no es posible desde el SDK y un `0` sería un precio falso.
    ///
    /// Limitaciones documentadas en `docs/observability-ledger.md` (notas de
    /// la auditoría del Grupo 5): series huérfanas cuando el modelo cambia o
    /// sale de catálogo —el SDK OTel no borra la serie anterior, filtra por
    /// `model` o cruza con `llm_gateway_model_selection_total`— y desfase del
    /// gauge hasta el siguiente refresco cuando este es on-demand.
    pub fn set_selected_model_price(
        &self,
        route: &str,
        model: &str,
        pin: Option<f64>,
        pout: Option<f64>,
    ) {
        if let Some(pin) = pin {
            self.selected_price
                .record(pin, &ledger_price_attrs(route, model, LedgerSide::Prompt));
        }
        if let Some(pout) = pout {
            self.selected_price.record(
                pout,
                &ledger_price_attrs(route, model, LedgerSide::Completion),
            );
        }
    }

    /// Publica el precio de un modelo de la lista curada.
    ///
    /// Gauge de estado con etiqueta única `model`; `blend` es la ponderación
    /// de selección `0.75·pin + 0.25·pout`, en USD/Mtok. Republicar el mismo
    /// modelo sobrescribe la serie (no la duplica).
    ///
    /// Cuando un modelo sale de la lista curada, su última serie queda huérfana
    /// hasta reiniciar (el SDK OTel no borra series): ver las notas de la
    /// auditoría del Grupo 5 en `docs/observability-ledger.md`.
    pub fn set_allowlist_price(&self, model: &str, blend: f64) {
        self.allowlist_price
            .record(blend, &[KeyValue::new("model", model.to_owned())]);
    }

    /// Fija el tamaño actual de la blacklist dinámica.
    ///
    /// Solo escribe el gauge si el tamaño difiere del último publicado; los
    /// accesos sin cambio de tamaño (la mayoría de purgas) no lo reescriben.
    ///
    /// La secuencia `swap` + `record` **requiere serialización externa**: dos
    /// llamadas concurrentes podrían publicar un valor distinto del último
    /// `swap` o dejar la caché desfasada respecto al gauge. Las llamadas desde
    /// [`crate::discovery::Blacklist`] quedan serializadas por su lock de
    /// escritura, donde no se produce un valor incorrecto; sin esa
    /// serialización, esta función no es segura por sí sola.
    pub(crate) fn set_blacklist_size(&self, size: usize) {
        let previous = self.blacklist_size.swap(size, Ordering::Relaxed);
        if previous == size {
            return;
        }
        self.discovery_blacklist.record(size as u64, &[]);
        #[cfg(test)]
        self.blacklist_size_updates.fetch_add(1, Ordering::Relaxed);
    }

    /// Actualizaciones de tamaño que reescribieron el gauge (solo para tests).
    ///
    /// Excluye el `record(0)` del pre-registro de [`Metrics::new`].
    #[cfg(test)]
    pub(crate) fn blacklist_size_updates(&self) -> usize {
        self.blacklist_size_updates.load(Ordering::Relaxed)
    }

    /// Renderiza todas las series en formato de exposición de Prometheus.
    ///
    /// Lee del mismo registro OTel que alimenta la exportación OTLP, así que
    /// scrape y export comparten una única fuente de verdad.
    pub fn render(&self) -> String {
        let families = telemetry::prometheus_registry().gather();
        let mut buffer = Vec::new();
        if let Err(err) = prometheus::TextEncoder::new().encode(&families, &mut buffer) {
            tracing::error!(error = %err, "no se pudieron codificar las métricas");
            return String::new();
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }
}

/// Serializa, solo en tests, los que tocan el gauge global de blacklist: al no
/// tener etiquetas es compartido por todo el proceso, así que cualquier test que
/// lo lea o escriba debe tomar este lock para no interferir con otro.
///
/// No se usa directamente: los tests toman [`BlacklistGaugeGuard`].
#[cfg(test)]
static BLACKLIST_GAUGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
thread_local! {
    /// `true` mientras el hilo actual mantiene [`BLACKLIST_GAUGE_LOCK`].
    static HOLDS_BLACKLIST_GAUGE_LOCK: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Guardián de test del gauge global de blacklist.
///
/// Equivale a [`BLACKLIST_GAUGE_LOCK`], pero marca el lock como tomado por el
/// hilo actual para que un [`Metrics::new`] dentro de la sección crítica pueda
/// pre-registrar el gauge sin bloquearse consigo mismo.
#[cfg(test)]
pub(crate) struct BlacklistGaugeGuard {
    /// `Some` mientras el guardián vive; se toma explícitamente al soltarlo.
    guard: Option<std::sync::MutexGuard<'static, ()>>,
}

#[cfg(test)]
impl BlacklistGaugeGuard {
    /// Toma el lock tolerando un posible envenenamiento.
    ///
    /// No es reentrante: si el hilo ya lo mantiene, el `debug_assert` de abajo
    /// lo detecta antes de bloquearse para siempre.
    pub(crate) fn lock() -> Self {
        debug_assert!(
            !HOLDS_BLACKLIST_GAUGE_LOCK.with(std::cell::Cell::get),
            "BlacklistGaugeGuard no es reentrante"
        );
        let guard = BLACKLIST_GAUGE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        HOLDS_BLACKLIST_GAUGE_LOCK.with(|held| held.set(true));
        Self { guard: Some(guard) }
    }
}

#[cfg(test)]
impl Drop for BlacklistGaugeGuard {
    fn drop(&mut self) {
        // Suelta el lock antes de limpiar el indicador del hilo: así el hilo
        // nunca observa «lock libre» mientras `HOLDS_BLACKLIST_GAUGE_LOCK`
        // sigue en `true`. El orden inverso también sería correcto, pero este
        // es el que refleja el estado real en cada instante.
        drop(self.guard.take());
        HOLDS_BLACKLIST_GAUGE_LOCK.with(|held| held.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, GatewayConfig, Route, Warmup};

    /// Construye una config con una ruta y un backend; si `warmup`, lo declara.
    ///
    /// El nombre de ruta es único por test: los instrumentos viven en el
    /// `MeterProvider` global compartido por todo el proceso, así que la
    /// aislamiento entre tests se consigue con conjuntos de etiquetas únicos.
    fn config(route_name: &str, warmup: bool) -> GatewayConfig {
        GatewayConfig {
            route: vec![Route {
                name: route_name.to_owned(),
                backend: vec![Backend {
                    kind: crate::config::BackendKind::Static,
                    base_url: "http://127.0.0.1:1/v1".to_owned(),
                    model: Some("m".to_owned()),
                    api_key_env: None,
                    timeout_ms: None,
                    profile: None,
                    discovery: None,
                    warmup: warmup.then_some(Warmup {
                        interval_ms: 1_000,
                        timeout_ms: None,
                    }),
                }],
            }],
            observability: None,
        }
    }

    /// Línea de `text` para la serie `name` con `route="<route>"`, si existe.
    fn line_for<'a>(text: &'a str, name: &str, route: &str) -> Option<&'a str> {
        text.lines()
            .find(|line| line.starts_with(name) && line.contains(&format!(r#"route="{route}""#)))
    }

    /// Valor (último campo) de una línea de serie.
    fn value(line: &str) -> &str {
        line.rsplit(' ').next().unwrap_or_default()
    }

    /// Valor de una serie o `None` si la serie no está presente.
    fn series_value<'a>(text: &'a str, name: &str, route: &str) -> Option<&'a str> {
        line_for(text, name, route).map(value)
    }

    #[test]
    fn render_emits_zero_series_from_the_first_scrape() {
        let metrics = Metrics::new(&config("scrape-vacio", false));
        let text = metrics.render();

        assert!(
            text.contains("# TYPE llm_gateway_requests_total counter"),
            "falta el TYPE del counter:\n{text}"
        );
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| {
                line.starts_with("llm_gateway_requests_total")
                    && line.contains(r#"route="scrape-vacio""#)
            })
            .collect();
        assert_eq!(
            lines.len(),
            4,
            "deben emitirse las 4 combinaciones:\n{text}"
        );
        assert!(
            lines.iter().all(|line| value(line) == "0"),
            "todas las series deben estar en cero:\n{text}"
        );
    }

    #[test]
    fn render_keeps_the_llm_gateway_series_names() {
        let metrics = Metrics::new(&config("nombres-de-serie", false));
        let text = metrics.render();

        for name in [
            "llm_gateway_requests_total",
            "llm_gateway_failover_total",
            "llm_gateway_backend_attempts_total",
            "llm_gateway_errors_total",
        ] {
            assert!(text.contains(name), "falta la serie {name}:\n{text}");
        }
    }

    #[test]
    fn render_escapes_label_values() {
        let metrics = Metrics::new(&config("a\"b\\c", false));
        let text = metrics.render();
        assert!(
            text.contains(r#"route="a\"b\\c""#),
            "etiqueta sin escapar en:\n{text}"
        );
    }

    #[test]
    fn render_escapes_newline() {
        let metrics = Metrics::new(&config("a\nb", false));
        let text = metrics.render();
        assert!(
            text.contains(r#"route="a\nb""#),
            "salto de línea sin escapar:\n{text}"
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let route = "buckets-acumulativos";
        let metrics = Metrics::new(&config(route, false));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(30));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(300));

        let text = metrics.render();
        let bucket = |bound: &str| {
            text.lines()
                .find(|line| {
                    line.starts_with("llm_gateway_request_duration_seconds_bucket")
                        && line.contains(&format!(r#"route="{route}""#))
                        && line.contains(&format!(r#"le="{bound}""#))
                })
                .map(value)
        };

        assert_eq!(
            bucket("0.05"),
            Some("1"),
            "0.05 s solo cuenta la primera:\n{text}"
        );
        assert_eq!(bucket("0.5"), Some("2"), "0.5 s cuenta ambas:\n{text}");
    }

    #[test]
    fn render_inf_matches_count() {
        let route = "inf-y-count";
        let metrics = Metrics::new(&config(route, false));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(10));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(20));

        let text = metrics.render();
        let inf = text
            .lines()
            .find(|line| {
                line.starts_with("llm_gateway_request_duration_seconds_bucket")
                    && line.contains(&format!(r#"route="{route}""#))
                    && line.contains(r#"le="+Inf""#)
            })
            .map(value);
        assert_eq!(inf, Some("2"), "bucket +Inf:\n{text}");
        assert_eq!(
            series_value(&text, "llm_gateway_request_duration_seconds_count", route),
            Some("2"),
            "count:\n{text}"
        );
    }

    #[test]
    fn requests_counter_increments_by_route_streaming_and_outcome() {
        let route = "counter-por-combinacion";
        let metrics = Metrics::new(&config(route, false));
        metrics.record_request(0, false, Outcome::Success, Duration::from_millis(5));
        metrics.record_request(0, true, Outcome::AllFailed, Duration::from_millis(5));

        let text = metrics.render();
        let series = |streaming: &str, outcome: &str| {
            text.lines()
                .find(|line| {
                    line.starts_with("llm_gateway_requests_total")
                        && line.contains(&format!(r#"route="{route}""#))
                        && line.contains(&format!(r#"streaming="{streaming}""#))
                        && line.contains(&format!(r#"outcome="{outcome}""#))
                })
                .map(value)
        };

        assert_eq!(
            series("false", "success"),
            Some("1"),
            "success no-stream:\n{text}"
        );
        assert_eq!(
            series("true", "all_failed"),
            Some("1"),
            "all_failed streaming:\n{text}"
        );
        assert_eq!(
            series("true", "success"),
            Some("0"),
            "resto en cero:\n{text}"
        );
        assert_eq!(
            series("false", "all_failed"),
            Some("0"),
            "resto en cero:\n{text}"
        );
    }

    #[test]
    fn global_outcomes_are_recorded_by_kind() {
        let metrics = Metrics::new(&config("outcome-global", false));
        metrics.record_global(GlobalOutcome::ModelNotFound);

        let text = metrics.render();
        let line = text
            .lines()
            .find(|line| {
                line.starts_with("llm_gateway_errors_total")
                    && line.contains(r#"kind="model_not_found""#)
            })
            .expect("errors_total con kind=model_not_found");
        assert_eq!(value(line), "1", "errors_total:\n{text}");
    }

    #[test]
    fn warmup_families_only_exist_for_backends_that_declare_warmup() {
        let route = "sin-warmup";
        let metrics = Metrics::new(&config(route, false));
        let text = metrics.render();

        for name in ["llm_gateway_backend_warm", "llm_gateway_warmup_total"] {
            assert!(
                line_for(&text, name, route).is_none(),
                "{name} no debe tener series para una ruta sin warmup:\n{text}"
            );
        }
    }

    #[test]
    fn warmup_gauge_and_counter_are_recorded() {
        let route = "con-warmup";
        let metrics = Metrics::new(&config(route, true));
        metrics.record_warmup(0, 0, true);
        metrics.record_warmup(0, 0, false);

        let text = metrics.render();
        assert_eq!(
            series_value(&text, "llm_gateway_backend_warm", route),
            Some("0"),
            "el gauge warm debe quedar a 0 tras el fallo:\n{text}"
        );

        let warmup = |result: &str| {
            text.lines()
                .find(|line| {
                    line.starts_with("llm_gateway_warmup_total")
                        && line.contains(&format!(r#"route="{route}""#))
                        && line.contains(&format!(r#"result="{result}""#))
                })
                .map(value)
        };
        assert_eq!(warmup("success"), Some("1"), "warmup success:\n{text}");
        assert_eq!(warmup("failure"), Some("1"), "warmup failure:\n{text}");
    }

    #[test]
    fn attempt_results_are_recorded_per_backend() {
        let route = "attempts-por-backend";
        let metrics = Metrics::new(&config(route, false));
        metrics.record_attempt(
            0,
            0,
            AttemptResult::TransportError,
            Duration::from_millis(2),
        );
        metrics.record_attempt(0, 0, AttemptResult::Success, Duration::from_millis(3));

        let text = metrics.render();
        let attempt = |result: &str| {
            text.lines()
                .find(|line| {
                    line.starts_with("llm_gateway_backend_attempts_total")
                        && line.contains(&format!(r#"route="{route}""#))
                        && line.contains(&format!(r#"result="{result}""#))
                })
                .map(value)
        };
        assert_eq!(
            attempt("transport_error"),
            Some("1"),
            "intento fallido:\n{text}"
        );
        assert_eq!(attempt("success"), Some("1"), "intento con éxito:\n{text}");
    }

    #[test]
    fn failover_counter_is_recorded_per_route() {
        let route = "failover-por-ruta";
        let metrics = Metrics::new(&config(route, false));
        metrics.record_failover(0);

        let text = metrics.render();
        assert_eq!(
            series_value(&text, "llm_gateway_failover_total", route),
            Some("1"),
            "failover_total:\n{text}"
        );
    }

    /// Config con un único backend dinámico (`openrouter_free`) sobre `base_url`.
    fn dynamic_config(base_url: &str) -> GatewayConfig {
        GatewayConfig {
            route: vec![Route {
                name: "discovery-metrics".to_owned(),
                backend: vec![Backend {
                    kind: crate::config::BackendKind::OpenrouterFree,
                    base_url: base_url.to_owned(),
                    model: None,
                    api_key_env: Some("OPENROUTER_API_KEY".to_owned()),
                    timeout_ms: None,
                    profile: Some(Profile::Code),
                    discovery: Some(crate::config::Discovery {
                        refresh_interval_ms: 300_000,
                        blacklist_cooldown_ms: 60_000,
                        min_context: None,
                        overrides: None,
                        respect_daily_quota: true,
                        quota_poll_interval_ms: 60_000,
                        allow: Vec::new(),
                        reselect_interval_ms: 14_400_000,
                        min_price_delta: 0.20,
                    }),
                    warmup: None,
                }],
            }],
            observability: None,
        }
    }

    /// Config con un backend dinámico y una ruta propia, para aislar las series
    /// de cuota del resto de tests (el registro Prometheus es global).
    fn quota_metrics_config(base_url: &str) -> GatewayConfig {
        let mut config = dynamic_config(base_url);
        config.route[0].name = "free-quota-metrics".to_owned();
        config
    }

    /// Línea de la serie `name` cuyo `base_url` es `base_url` (y opcionalmente
    /// `profile`/`result`).
    fn discovery_line<'a>(
        text: &'a str,
        name: &str,
        base_url: &str,
        extra: Option<(&str, &str)>,
    ) -> Option<&'a str> {
        text.lines().find(|line| {
            line.starts_with(name)
                && line.contains(&format!(r#"base_url="{base_url}""#))
                && extra.is_none_or(|(key, value)| line.contains(&format!(r#"{key}="{value}""#)))
        })
    }

    #[test]
    fn discovery_metrics_are_preregistered_at_zero() {
        let _guard = BlacklistGaugeGuard::lock();
        let base = "http://discovery-preregistro.invalid/v1";
        let metrics = Metrics::new(&dynamic_config(base));
        let text = metrics.render();

        for (name, extra) in [
            ("llm_gateway_discovery_models", None),
            ("llm_gateway_discovery_free_models", None),
            (
                "llm_gateway_discovery_profile_models",
                Some(("profile", "code")),
            ),
            (
                "llm_gateway_discovery_refresh_total",
                Some(("result", "success")),
            ),
            (
                "llm_gateway_discovery_refresh_total",
                Some(("result", "failure")),
            ),
            ("llm_gateway_discovery_blacklist", None),
        ] {
            // La blacklist es global y no lleva `base_url` en sus atributos.
            let line = if name == "llm_gateway_discovery_blacklist" {
                text.lines().find(|line| line.starts_with(name))
            } else {
                discovery_line(&text, name, base, extra)
            };
            let line = line.unwrap_or_else(|| panic!("falta la serie {name} para {base}:\n{text}"));
            assert_eq!(
                value(line),
                "0",
                "{name} debe pre-registrarse en cero:\n{text}"
            );
        }
    }

    #[test]
    fn discovery_metrics_are_recorded_and_increment() {
        let _guard = BlacklistGaugeGuard::lock();
        let base = "http://discovery-registro.invalid/v1";
        let metrics = Metrics::new(&dynamic_config(base));

        metrics.record_discovery_catalog(base, 10, 3);
        metrics.record_discovery_profile(base, Profile::Code, 2);
        metrics.record_discovery_refresh(base, true);
        metrics.record_discovery_refresh(base, false);
        metrics.set_blacklist_size(4);

        let text = metrics.render();

        let catalog = discovery_line(&text, "llm_gateway_discovery_models", base, None)
            .expect("serie de modelos");
        assert_eq!(value(catalog), "10", "modelos totales:\n{text}");

        let free = discovery_line(&text, "llm_gateway_discovery_free_models", base, None)
            .expect("serie de modelos free");
        assert_eq!(value(free), "3", "modelos free elegibles:\n{text}");

        let profile = discovery_line(
            &text,
            "llm_gateway_discovery_profile_models",
            base,
            Some(("profile", "code")),
        )
        .expect("serie por perfil");
        assert_eq!(value(profile), "2", "elegibles por perfil:\n{text}");

        let success = discovery_line(
            &text,
            "llm_gateway_discovery_refresh_total",
            base,
            Some(("result", "success")),
        )
        .expect("serie de refrescos OK");
        assert_eq!(value(success), "1", "refrescos con éxito:\n{text}");

        let failure = discovery_line(
            &text,
            "llm_gateway_discovery_refresh_total",
            base,
            Some(("result", "failure")),
        )
        .expect("serie de refrescos fallidos");
        assert_eq!(value(failure), "1", "refrescos fallidos:\n{text}");

        let blacklist = text
            .lines()
            .find(|line| line.starts_with("llm_gateway_discovery_blacklist"))
            .expect("serie de blacklist");
        assert_eq!(value(blacklist), "4", "tamaño de blacklist:\n{text}");
    }

    /// Scenario: el gauge de cuota y el contador de saltos se emiten.
    #[test]
    fn free_quota_gauge_and_skips_counter_are_recorded() {
        let _guard = BlacklistGaugeGuard::lock();
        let base = "http://free-quota-metrics.invalid/v1";
        let metrics = Metrics::new(&quota_metrics_config(base));

        metrics.record_free_quota_remaining(base, 250);
        metrics.record_free_quota_skip(0);

        let text = metrics.render();

        let gauge = discovery_line(&text, "llm_gateway_free_quota_remaining", base, None)
            .expect("serie del gauge de cuota");
        assert_eq!(value(gauge), "250", "gauge de cuota:\n{text}");

        let skips = line_for(
            &text,
            "llm_gateway_free_quota_skips_total",
            "free-quota-metrics",
        )
        .expect("serie del contador de saltos");
        assert_eq!(value(skips), "1", "contador de saltos:\n{text}");
    }

    // --- Grupo 5 (#39): métricas del ledger ----------------------------------

    /// Línea de la serie `name` que contiene todos los pares `key="value"`.
    fn ledger_line<'a>(text: &'a str, name: &str, pairs: &[(&str, &str)]) -> Option<&'a str> {
        text.lines().find(|line| {
            line.starts_with(name)
                && pairs
                    .iter()
                    .all(|(key, expected)| line.contains(&format!(r#"{key}="{expected}""#)))
        })
    }

    /// Valor de una línea de serie como `f64` (precios y coste son flotantes).
    fn fnum(line: &str) -> f64 {
        line.rsplit(' ')
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(f64::NAN)
    }

    /// Scenario «Tokens y coste incrementan»: 1000 in / 200 out incrementan
    /// `tokens_total` por `type` (`prompt`/`completion`) y el coste calculado
    /// incrementa `cost_usd_total` por ruta y modelo.
    #[test]
    fn ledger_tokens_and_cost_counters_increment_per_type() {
        let metrics = Metrics::new(&config("ledger-tokens", false));
        metrics.record_ledger_usage(
            "ledger-tokens",
            "acme/flash-a",
            Some(1000),
            Some(200),
            Some(0.0036),
        );

        let text = metrics.render();
        let prompt = ledger_line(
            &text,
            "llm_gateway_tokens_total",
            &[
                ("route", "ledger-tokens"),
                ("model", "acme/flash-a"),
                ("type", "prompt"),
            ],
        )
        .unwrap_or_else(|| panic!("falta tokens_total type=prompt:\n{text}"));
        assert_eq!(fnum(prompt), 1000.0, "tokens de entrada:\n{prompt}");

        let completion = ledger_line(
            &text,
            "llm_gateway_tokens_total",
            &[
                ("route", "ledger-tokens"),
                ("model", "acme/flash-a"),
                ("type", "completion"),
            ],
        )
        .unwrap_or_else(|| panic!("falta tokens_total type=completion:\n{text}"));
        assert_eq!(fnum(completion), 200.0, "tokens de salida:\n{completion}");

        let cost = ledger_line(
            &text,
            "llm_gateway_cost_usd_total",
            &[("route", "ledger-tokens"), ("model", "acme/flash-a")],
        )
        .unwrap_or_else(|| panic!("falta cost_usd_total:\n{text}"));
        assert_eq!(fnum(cost), 0.0036, "coste en USD:\n{cost}");
    }

    /// Los contadores del ledger acumulan entre peticiones (no reemplazan).
    #[test]
    fn ledger_counters_accumulate_across_requests() {
        let metrics = Metrics::new(&config("ledger-acumulo", false));
        metrics.record_ledger_usage(
            "ledger-acumulo",
            "acme/flash-a",
            Some(1000),
            Some(200),
            Some(0.0036),
        );
        metrics.record_ledger_usage(
            "ledger-acumulo",
            "acme/flash-a",
            Some(500),
            Some(20),
            Some(0.0026),
        );

        let text = metrics.render();
        let prompt = ledger_line(
            &text,
            "llm_gateway_tokens_total",
            &[("route", "ledger-acumulo"), ("type", "prompt")],
        )
        .unwrap_or_else(|| panic!("falta tokens_total type=prompt:\n{text}"));
        assert_eq!(
            fnum(prompt),
            1500.0,
            "tokens de entrada acumulados:\n{prompt}"
        );

        let completion = ledger_line(
            &text,
            "llm_gateway_tokens_total",
            &[("route", "ledger-acumulo"), ("type", "completion")],
        )
        .unwrap_or_else(|| panic!("falta tokens_total type=completion:\n{text}"));
        assert_eq!(
            fnum(completion),
            220.0,
            "tokens de salida acumulados:\n{completion}"
        );

        let cost = ledger_line(
            &text,
            "llm_gateway_cost_usd_total",
            &[("route", "ledger-acumulo")],
        )
        .unwrap_or_else(|| panic!("falta cost_usd_total:\n{text}"));
        assert!(
            (fnum(cost) - (0.0036 + 0.0026)).abs() < f64::EPSILON,
            "coste acumulado:\n{cost}"
        );
    }

    /// Sin `usage` (o sin precio) no se registra nada: nunca se inventa un `0`.
    /// Con datos parciales solo se registra la parte conocida.
    #[test]
    fn ledger_usage_without_data_records_nothing() {
        let metrics = Metrics::new(&config("ledger-vacio", false));
        metrics.record_ledger_usage("ledger-vacio", "acme/flash-a", None, None, None);

        let text = metrics.render();
        for name in ["llm_gateway_tokens_total", "llm_gateway_cost_usd_total"] {
            assert!(
                ledger_line(&text, name, &[("route", "ledger-vacio")]).is_none(),
                "{name} no debe existir sin datos:\n{text}"
            );
        }

        // Datos parciales: tokens conocidos, coste desconocido (sin precio).
        metrics.record_ledger_usage("ledger-parcial", "acme/flash-a", Some(10), None, None);
        let text = metrics.render();
        let prompt = ledger_line(
            &text,
            "llm_gateway_tokens_total",
            &[("route", "ledger-parcial"), ("type", "prompt")],
        )
        .unwrap_or_else(|| panic!("los tokens conocidos sí se registran:\n{text}"));
        assert_eq!(fnum(prompt), 10.0, "tokens parciales:\n{prompt}");
        assert!(
            ledger_line(
                &text,
                "llm_gateway_cost_usd_total",
                &[("route", "ledger-parcial")]
            )
            .is_none(),
            "sin precio no hay serie de coste:\n{text}"
        );
    }

    /// Scenario «Contador de selecciones por motivo»: un cambio de selección
    /// incrementa `model_selection_total{route,reason}`.
    #[test]
    fn model_selection_counter_increments_by_reason() {
        let metrics = Metrics::new(&config("ledger-seleccion", false));
        metrics.record_model_selection("ledger-seleccion", SelectionReason::Failover);
        metrics.record_model_selection("ledger-seleccion", SelectionReason::Failover);
        metrics.record_model_selection("ledger-seleccion", SelectionReason::Initial);

        let text = metrics.render();
        let failover = ledger_line(
            &text,
            "llm_gateway_model_selection_total",
            &[("route", "ledger-seleccion"), ("reason", "failover")],
        )
        .unwrap_or_else(|| panic!("falta model_selection_total reason=failover:\n{text}"));
        assert_eq!(fnum(failover), 2.0, "dos failovers:\n{failover}");

        let initial = ledger_line(
            &text,
            "llm_gateway_model_selection_total",
            &[("route", "ledger-seleccion"), ("reason", "initial")],
        )
        .unwrap_or_else(|| panic!("falta model_selection_total reason=initial:\n{text}"));
        assert_eq!(fnum(initial), 1.0, "un initial:\n{initial}");
    }

    /// Scenario «Gauge del modelo seleccionado»: `side=prompt` refleja el
    /// precio de entrada y `side=completion` el de salida; republicar
    /// sobrescribe el valor (gauge de estado, no acumula) y sin precio no hay
    /// serie nueva.
    #[test]
    fn selected_price_gauge_reflects_current_price() {
        let metrics = Metrics::new(&config("ledger-precio-sel", false));
        metrics.set_selected_model_price("ledger-precio-sel", "acme/flash-a", Some(2.0), Some(8.0));

        let text = metrics.render();
        let pin = ledger_line(
            &text,
            "llm_gateway_selected_model_price_usd",
            &[
                ("route", "ledger-precio-sel"),
                ("model", "acme/flash-a"),
                ("side", "prompt"),
            ],
        )
        .unwrap_or_else(|| panic!("falta side=prompt:\n{text}"));
        assert_eq!(fnum(pin), 2.0, "precio de entrada en USD/Mtok:\n{pin}");

        let pout = ledger_line(
            &text,
            "llm_gateway_selected_model_price_usd",
            &[
                ("route", "ledger-precio-sel"),
                ("model", "acme/flash-a"),
                ("side", "completion"),
            ],
        )
        .unwrap_or_else(|| panic!("falta side=completion:\n{text}"));
        assert_eq!(fnum(pout), 8.0, "precio de salida en USD/Mtok:\n{pout}");

        // Republicar: el lado conocido se sobrescribe; el desconocido conserva
        // el último valor publicado (gauge de estado).
        metrics.set_selected_model_price("ledger-precio-sel", "acme/flash-a", Some(2.5), None);
        let text = metrics.render();
        let pin = ledger_line(
            &text,
            "llm_gateway_selected_model_price_usd",
            &[
                ("route", "ledger-precio-sel"),
                ("model", "acme/flash-a"),
                ("side", "prompt"),
            ],
        )
        .unwrap_or_else(|| panic!("falta side=prompt tras republicar:\n{text}"));
        assert_eq!(fnum(pin), 2.5, "precio actualizado:\n{pin}");
        let pout = ledger_line(
            &text,
            "llm_gateway_selected_model_price_usd",
            &[
                ("route", "ledger-precio-sel"),
                ("model", "acme/flash-a"),
                ("side", "completion"),
            ],
        )
        .expect("el último precio de salida conocido se conserva");
        assert_eq!(
            fnum(pout),
            8.0,
            "sin pout nuevo se conserva el último:\n{pout}"
        );

        // Un modelo sin precio conocido no publica ninguna serie. La etiqueta
        // del modelo es única de este test: otros tests de selección publican
        // los suyos en el registro global compartido.
        metrics.set_selected_model_price("ledger-precio-sel", "sel/sin-precio", None, None);
        let text = metrics.render();
        assert!(
            ledger_line(
                &text,
                "llm_gateway_selected_model_price_usd",
                &[("model", "sel/sin-precio")]
            )
            .is_none(),
            "sin precio no hay serie:\n{text}"
        );
    }

    /// Scenario «Gauge de la lista curada»: una serie por modelo con precio
    /// conocido; republicar un modelo actualiza su valor sin duplicar series.
    ///
    /// Los modelos son únicos de este test: la etiqueta del gauge es solo
    /// `model` y otros tests (p. ej. los de selección) publican los suyos en
    /// el registro global compartido.
    #[test]
    fn allowlist_price_gauge_publishes_one_series_per_model() {
        let metrics = Metrics::new(&config("ledger-precio-allow", false));
        metrics.set_allowlist_price("ledger/curado-a", 0.04);
        metrics.set_allowlist_price("ledger/curado-b", 0.1);

        let text = metrics.render();
        let series = text
            .lines()
            .filter(|line| {
                line.starts_with("llm_gateway_allowlist_price_usd")
                    && line.contains(r#"model="ledger/curado-"#)
            })
            .count();
        assert_eq!(series, 2, "una serie por modelo:\n{text}");

        let curado_a = ledger_line(
            &text,
            "llm_gateway_allowlist_price_usd",
            &[("model", "ledger/curado-a")],
        )
        .unwrap_or_else(|| panic!("falta la serie de ledger/curado-a:\n{text}"));
        assert_eq!(
            fnum(curado_a),
            0.04,
            "blend de ledger/curado-a:\n{curado_a}"
        );

        // Republicar actualiza el valor sin duplicar la serie.
        metrics.set_allowlist_price("ledger/curado-a", 0.05);
        let text = metrics.render();
        let series = text
            .lines()
            .filter(|line| {
                line.starts_with("llm_gateway_allowlist_price_usd")
                    && line.contains(r#"model="ledger/curado-a""#)
            })
            .count();
        assert_eq!(series, 1, "republicar no duplica la serie:\n{text}");
        let curado_a = ledger_line(
            &text,
            "llm_gateway_allowlist_price_usd",
            &[("model", "ledger/curado-a")],
        )
        .expect("serie de ledger/curado-a tras republicar");
        assert_eq!(fnum(curado_a), 0.05, "precio actual:\n{curado_a}");
    }

    /// Scenario «request_id no es etiqueta de métrica»: ninguna serie incluye
    /// `request_id`, ni siquiera tras registrar usage, coste, selección y
    /// precios.
    #[test]
    fn metrics_never_carry_request_id_label() {
        let metrics = Metrics::new(&config("ledger-sin-request-id", false));
        metrics.record_ledger_usage(
            "ledger-sin-request-id",
            "acme/flash-a",
            Some(1000),
            Some(200),
            Some(0.0036),
        );
        metrics.record_model_selection("ledger-sin-request-id", SelectionReason::Initial);
        metrics.set_selected_model_price(
            "ledger-sin-request-id",
            "acme/flash-a",
            Some(2.0),
            Some(8.0),
        );
        metrics.set_allowlist_price("curado/sin-request-id", 0.04);

        let text = metrics.render();
        assert!(
            !text.contains("request_id="),
            "ninguna serie debe etiquetar request_id:\n{text}"
        );
    }
}
