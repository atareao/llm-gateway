//! Router y handlers del gateway compatible con OpenAI.
//!
//! Esta capa no interpreta el cuerpo: lo reenvía tal cual **salvo `model`**,
//! que se sustituye por el del backend al que se enruta la petición.
//!
//! El enrutado recorre la cadena ordenada de backends de la ruta y hace
//! **failover antes del primer byte**: si un intento falla por conexión,
//! timeout, `429` o `5xx`, se prueba el siguiente; un `4xx` distinto de `429`
//! se devuelve al cliente sin reintentar.
//!
//! Además, solo en **no-stream**, si la petición incluye `tools` como lista no
//! vacía y `tool_choice` exige una llamada a herramienta (`required`, `any` u
//! objeto con función), una respuesta de éxito (`2xx`) cuyo cuerpo no contenga
//! `tool_calls` cuenta como intento fallido y se continúa con el siguiente
//! backend. La inspección es conservadora (si el cuerpo no parsea o la forma es
//! inesperada, se devuelve como éxito) y **no** altera los bytes enviados al
//! cliente.
//!
//! ### Limitación de streaming
//! En modo `stream: true`, una vez empezada la emisión **no hay failover**: si
//! el upstream se corta a mitad, el stream se cierra con error (no se
//! reintenta, porque ya se habrían enviado bytes al cliente). La condición del
//! `tool_choice` tampoco se aplica en streaming: los `tool_calls` llegarían como
//! deltas SSE y detectarlos obligaría a bufferizar, rompiendo el passthrough.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt as _;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::config::{Backend, BackendKind, GatewayConfig, Route};
use crate::discovery::Registry;
use crate::error::AppError;
use crate::metrics::{AttemptResult, GlobalOutcome, Metrics, Outcome};

/// Timeout de establecimiento de conexión hacia un backend, para que un
/// backend caído no deje la petición colgada antes de decidir el failover.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Nombre de la cabecera que identifica el backend que atendió la petición.
pub const BACKEND_HEADER: &str = "x-gateway-backend";

/// Cabecera de correlación de la petición.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// `content-type` por defecto para respuestas no-stream.
const DEFAULT_CONTENT_TYPE: &str = "application/json";

/// `content-type` por defecto para respuestas en streaming.
const DEFAULT_STREAM_CONTENT_TYPE: &str = "text/event-stream";

/// Presupuesto por defecto de **timeout total** para peticiones no-stream
/// cuando el backend no define `timeout_ms`.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Presupuesto por defecto de **time-to-first-byte** en streaming cuando el
/// backend no define `timeout_ms`. Solo limita el arranque: una vez llega la
/// primera cabecera, el stream fluye sin deadline.
const DEFAULT_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(5);

/// Cota superior del cooldown derivado de `Retry-After`.
///
/// Un `Retry-After` en formato fecha puede apuntar a años vista; sin cota, un
/// único `429`/`5xx` con una fecha lejana blacklistearía el modelo durante años.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// Horizonte de reset que convierte un `429` en agotamiento **diario**.
///
/// La heurística de cabeceras (`X-RateLimit-Remaining: 0`) solo abre el gate si
/// el reset está a más de cinco minutos vista: un `429` de minuto trae un reset
/// cercano y no debe bloquear el free todo el día.
const QUOTA_RESET_HORIZON: Duration = Duration::from_secs(5 * 60);

/// Nombre de `error.metadata.limit_source` que identifica la cuota diaria free.
const DAILY_LIMIT_SOURCE: &str = "openrouter_free_tier_daily";

/// Tope del cuerpo de un `429` inspeccionado buscando `limit_source`.
const QUOTA_BODY_LIMIT: usize = 64 * 1024;

/// Señal de cuota deducida del cuerpo de una respuesta `429`.
enum QuotaSignal {
    /// Cuota diaria free agotada (`limit_source` = diaria).
    Daily,
    /// Otro límite (`per_minute`, proveedor…) que no abre el gate.
    Other,
    /// Cuerpo ausente o sin metadatos: se decide por las cabeceras.
    Unknown,
}

/// Fallo al contactar con un backend, antes de tener su respuesta.
enum SendError {
    /// Error de transporte de `reqwest` (conexión, TLS, lectura…).
    Transport(reqwest::Error),
    /// Venció el guard de time-to-first-byte.
    FirstByteTimeout(Duration),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Transport(err) => write!(f, "{err}"),
            SendError::FirstByteTimeout(budget) => {
                write!(f, "timeout de primer byte tras {budget:?}")
            }
        }
    }
}

/// Estado compartido por los handlers: configuración de rutas y cliente HTTP
/// reutilizable (con pool de conexiones) hacia los backends.
#[derive(Clone)]
pub struct AppState {
    /// Configuración de rutas cargada al arrancar.
    pub config: Arc<GatewayConfig>,
    /// Cliente HTTP compartido hacia los backends.
    pub client: reqwest::Client,
    /// Registro de métricas Prometheus.
    pub metrics: Arc<Metrics>,
    /// Registro de descubrimiento del catálogo dinámico.
    pub discovery: Arc<Registry>,
}

impl AppState {
    /// Construye el estado a partir de una configuración ya cargada.
    ///
    /// El cliente HTTP aplica un [`CONNECT_TIMEOUT`] para que un backend caído
    /// falle rápido y el failover no se demore.
    ///
    /// # Errors
    /// Devuelve el error si el cliente HTTP no se puede construir (p. ej. si
    /// falla la inicialización del backend TLS).
    pub fn new(config: GatewayConfig) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;

        let metrics = Arc::new(Metrics::new(&config));
        let discovery = Arc::new(Registry::new(&config, client.clone(), Arc::clone(&metrics)));

        Ok(Self {
            config: Arc::new(config),
            client,
            metrics,
            discovery,
        })
    }
}

/// Construye el router de la aplicación.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics_handler))
        .with_state(state)
}

/// `GET /metrics`: exposición en formato Prometheus.
async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
}

/// Identificador determinista del backend usado, en el formato
/// `<base_url>#<model>` (p. ej. `http://localhost:11434/v1#qwen2.5:7b-instruct`).
///
/// Se expone para que clientes y tests puedan predecirlo; se emite en la
/// cabecera `x-gateway-backend` en las respuestas con éxito.
pub fn backend_id(backend: &Backend) -> String {
    let model = backend.model.as_deref().unwrap_or("dynamic");
    crate::config::format_id(&backend.base_url, model)
}

/// `POST /v1/chat/completions` (no-stream o streaming SSE).
///
/// Resuelve la ruta lógica a partir del campo `model` y delega el enrutado (con
/// failover) en [`route_with_failover`].
async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let request_id = resolve_request_id(&headers);

    // Span de servidor para la petición completa, incluidos los rechazos
    // previos a resolver la ruta. Si la petición entrante trae un `traceparent`
    // válido se enlaza como hijo (traza distribuida); `route`/`streaming` se
    // rellenan más abajo con `Span::record`.
    let span = tracing::info_span!(
        "request",
        request_id = %request_id,
        route = tracing::field::Empty,
        streaming = tracing::field::Empty,
        "otel.kind" = "server",
    );
    link_remote_parent(&span, &headers);

    let handler_span = span.clone();
    handle_chat_completions(state, body, request_id, &handler_span)
        .instrument(span)
        .await
}

/// Cuerpo del handler, cubierto por el span de servidor que crea
/// [`chat_completions`].
async fn handle_chat_completions(
    state: AppState,
    body: Result<Json<Value>, JsonRejection>,
    request_id: String,
    span: &tracing::Span,
) -> Response {
    let started = Instant::now();

    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => {
            state.metrics.record_global(GlobalOutcome::InvalidRequest);
            tracing::warn!(
                request_id = %request_id,
                error = %rejection,
                "cuerpo de petición inválido"
            );
            return with_request_id(
                AppError::InvalidRequest(rejection.body_text()).into_response(),
                &request_id,
            );
        }
    };

    let Some(model) = body.get("model").and_then(Value::as_str) else {
        state.metrics.record_global(GlobalOutcome::InvalidRequest);
        tracing::warn!(request_id = %request_id, "petición sin 'model' válido");
        return with_request_id(
            AppError::InvalidRequest(
                "el campo 'model' es obligatorio y debe ser una cadena".to_owned(),
            )
            .into_response(),
            &request_id,
        );
    };

    let Some(route_idx) = state
        .config
        .route
        .iter()
        .position(|route| route.name == model)
    else {
        state.metrics.record_global(GlobalOutcome::ModelNotFound);
        tracing::warn!(request_id = %request_id, model = %model, "modelo desconocido");
        return with_request_id(
            AppError::ModelNotFound(model.to_owned()).into_response(),
            &request_id,
        );
    };

    let route = &state.config.route[route_idx];
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    span.record("route", tracing::field::display(&route.name));
    span.record("streaming", streaming);
    let result = route_with_failover(&state, route_idx, route, &body, streaming, &request_id).await;

    let elapsed = started.elapsed();
    let outcome = if result.is_ok() {
        Outcome::Success
    } else {
        Outcome::AllFailed
    };
    state
        .metrics
        .record_request(route_idx, streaming, outcome, elapsed);

    match result {
        Ok(response) => with_request_id(response, &request_id),
        Err(err) => with_request_id(err.into_response(), &request_id),
    }
}

/// Candidato concreto tras expandir un backend de la ruta.
///
/// Contiene solo los campos que el enrutado necesita, para no clonar el
/// [`Backend`] completo (con su `discovery`, `warmup`, etc.) en el hot path.
struct Candidate {
    /// Índice del backend en la configuración (slot para métricas).
    slot: usize,
    /// Modelo real a enviar; en dinámicos, el modelo expandido del catálogo.
    model: String,
    /// URL base del backend al que se envía la petición.
    base_url: String,
    /// Timeout en milisegundos, si el backend lo define.
    timeout_ms: Option<u64>,
    /// Variable de entorno de la API key, si el backend la declara.
    api_key_env: Option<String>,
    /// Cooldown de blacklist si el candidato procede de un backend dinámico.
    dynamic_cooldown: Option<Duration>,
    /// `true` si el candidato procede de un backend del tier free
    /// (`openrouter_free`); solo esos pueden abrir el gate de cuota diaria.
    free_tier: bool,
    /// `true` si el candidato respeta el gate global de cuota diaria
    /// (`openrouter_free` con `discovery.respect_daily_quota`, por defecto `true`).
    /// Un `429` diario solo abre el gate si el candidato lo respeta.
    respect_daily_quota: bool,
}

impl Candidate {
    /// Construye un candidato a partir de un backend de configuración, copiando
    /// solo los campos necesarios. Un backend sin `model` usa `"dynamic"`, igual
    /// que [`backend_id`]; solo los backends dinámicos pueden carecer de `model`
    /// (los estáticos lo exigen en `validate`).
    fn from_backend(slot: usize, backend: &Backend) -> Self {
        debug_assert!(
            backend.model.is_some() || backend.is_dynamic(),
            "backend estático sin model: {backend:?}"
        );
        Self {
            slot,
            model: backend
                .model
                .clone()
                .unwrap_or_else(|| "dynamic".to_owned()),
            base_url: backend.base_url.clone(),
            timeout_ms: backend.timeout_ms,
            api_key_env: backend.api_key_env.clone(),
            dynamic_cooldown: None,
            free_tier: backend.kind == BackendKind::OpenrouterFree,
            respect_daily_quota: backend.is_free_tier()
                && backend
                    .discovery
                    .as_ref()
                    .is_none_or(|discovery| discovery.respect_daily_quota),
        }
    }

    /// Sustituye el modelo del candidato (expansión de un backend dinámico).
    fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }

    /// Marca el candidato como procedente de un backend dinámico, de modo que
    /// sus fallos puedan blacklistear el modelo.
    fn with_dynamic_cooldown(mut self, cooldown: Option<Duration>) -> Self {
        self.dynamic_cooldown = cooldown;
        self
    }

    /// Identificador determinista del candidato, en el formato
    /// `<base_url>#<model>`, idéntico al de [`backend_id`].
    fn id(&self) -> String {
        crate::config::format_id(&self.base_url, &self.model)
    }
}

/// Expande cada backend de la ruta en su posición: los estáticos aportan un
/// único candidato y los dinámicos (`openrouter_free`) se expanden a sus modelos
/// elegibles, en orden y con rotación round-robin.
///
/// No clona el [`Backend`] completo: los candidatos se construyen campo a campo
/// con [`Candidate::from_backend`].
async fn expand_backends(state: &AppState, route_idx: usize, route: &Route) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    // El salto del tier free por el gate se cuenta una sola vez por petición y
    // ruta, aunque haya varios backends free.
    let mut free_skipped = false;
    for (slot, backend) in route.backend.iter().enumerate() {
        if backend.is_dynamic() {
            if state.discovery.free_quota_skip(route_idx, slot) {
                free_skipped = true;
            }
            let cooldown = state.discovery.cooldown(route_idx, slot);
            // Un candidato ligero por modelo elegible.
            for model in state.discovery.expand(route_idx, slot).await {
                candidates.push(
                    Candidate::from_backend(slot, backend)
                        .with_model(model)
                        .with_dynamic_cooldown(cooldown),
                );
            }
        } else {
            candidates.push(Candidate::from_backend(slot, backend));
        }
    }
    if free_skipped {
        state.metrics.record_free_quota_skip(route_idx);
    }
    candidates
}

/// Marca `model` (de un backend dinámico) en la blacklist global si su respuesta
/// lo exige: `429`, `5xx` o presencia de un `Retry-After` **positivo**. El
/// cooldown efectivo es el mayor entre el configurado y el indicado por
/// `Retry-After`, acotado a [`MAX_RETRY_AFTER`].
///
/// Un `Retry-After` que se traduce a duración cero (p. ej. una fecha HTTP ya
/// pasada) no es informativo: no fuerza por sí solo la blacklist de un status no
/// reintentable.
fn blacklist_if_needed(
    state: &AppState,
    model: &str,
    response: &reqwest::Response,
    cooldown: Duration,
) {
    let retry_after = parse_retry_after(response.headers().get(header::RETRY_AFTER))
        .filter(|delay| !delay.is_zero());
    if !is_retryable(response.status()) && retry_after.is_none() {
        return;
    }
    let effective = effective_cooldown(retry_after, cooldown);
    state.discovery.blacklist().mark(model, effective);
    tracing::warn!(
        model = %model,
        status = %response.status(),
        cooldown_ms = effective.as_millis() as u64,
        "modelo dinámico en blacklist"
    );
}

/// Cooldown efectivo de blacklist: el mayor entre el configurado y el
/// `Retry-After`, nunca por encima de [`MAX_RETRY_AFTER`].
fn effective_cooldown(retry_after: Option<Duration>, cooldown: Duration) -> Duration {
    cooldown
        .max(retry_after.unwrap_or(Duration::ZERO))
        .min(MAX_RETRY_AFTER)
}

/// Interpreta el encabezado `Retry-After`, en segundos o como fecha HTTP.
///
/// Acepta el formato numérico (`Retry-After: 30`) y el de fecha IMF-fixdate
/// (`Retry-After: Wed, 21 Oct 2099 07:28:00 GMT`), que se traduce al delta hasta
/// esa fecha. Una fecha ya pasada se interpreta como `0` (nunca un delta
/// negativo), de modo que el cooldown efectivo cae al configurado. Un valor no
/// interpretable devuelve `None`.
fn parse_retry_after(value: Option<&HeaderValue>) -> Option<Duration> {
    let text = value?.to_str().ok()?.trim();
    if let Ok(seconds) = text.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = httpdate::parse_http_date(text).ok()?;
    Some(
        deadline
            .duration_since(std::time::SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

/// Reset del límite de la respuesta (`X-RateLimit-Reset`, epoch en ms).
fn quota_reset_epoch_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("x-ratelimit-reset")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// `true` si las cabeceras indican agotamiento **diario**: `X-RateLimit-Remaining`
/// a cero **y** un reset a más de [`QUOTA_RESET_HORIZON`] vista.
fn headers_show_daily_exhaustion(headers: &HeaderMap, reset_ms: Option<u64>) -> bool {
    let remaining_zero = headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        == Some(0.0);
    if !remaining_zero {
        return false;
    }
    reset_ms
        .and_then(crate::discovery::epoch_ms_to_instant)
        .is_some_and(|deadline| {
            deadline.saturating_duration_since(Instant::now()) > QUOTA_RESET_HORIZON
        })
}

/// Señal de cuota deducida del cuerpo de la respuesta.
///
/// `limit_source` es autoritativo: `openrouter_free_tier_daily` abre el gate y
/// cualquier otro valor (minuto, proveedor) lo deja cerrado, ignorando la
/// heurística de cabeceras. Un `provider_code` presente también se considera
/// «otro» límite. Sin metadatos, la señal es desconocida.
fn quota_signal(body: Option<&Value>) -> QuotaSignal {
    let Some(metadata) = body.and_then(|body| body.pointer("/error/metadata")) else {
        return QuotaSignal::Unknown;
    };
    if let Some(source) = metadata.get("limit_source") {
        let daily = source.as_str() == Some(DAILY_LIMIT_SOURCE);
        return if daily {
            QuotaSignal::Daily
        } else {
            QuotaSignal::Other
        };
    }
    if metadata.get("provider_code").is_some() {
        return QuotaSignal::Other;
    }
    QuotaSignal::Unknown
}

/// Lee el cuerpo de un `429` de forma acotada para buscar `limit_source`.
///
/// El cuerpo se descarta por encima de [`QUOTA_BODY_LIMIT`]. Se lee **fragmento
/// a fragmento** (`chunk`) en vez de `bytes()` para no acumular entero un cuerpo
/// sin `Content-Length` (chunked) antes de decidir: en cuanto la suma supera el
/// límite se descarta, sin esperar al resto del cuerpo.
async fn read_quota_body(mut response: reqwest::Response) -> Option<Value> {
    if response
        .content_length()
        .is_some_and(|len| len > QUOTA_BODY_LIMIT as u64)
    {
        return None;
    }
    let mut body: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > QUOTA_BODY_LIMIT {
                    return None;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    serde_json::from_slice(&body).ok()
}

/// Detecta un `429` de cuota diaria free y abre el gate global.
///
/// Solo se invoca para respuestas `429` de un backend del tier free que ya se
/// van a descartar (son reintentables), así que consumir su cuerpo es seguro.
async fn detect_daily_quota(state: &AppState, response: reqwest::Response) {
    let headers = response.headers().clone();
    let reset_ms = quota_reset_epoch_ms(&headers);
    let header_daily = headers_show_daily_exhaustion(&headers, reset_ms);
    let body = read_quota_body(response).await;

    let daily = match quota_signal(body.as_ref()) {
        QuotaSignal::Daily => true,
        QuotaSignal::Other => false,
        QuotaSignal::Unknown => header_daily,
    };
    if !daily {
        return;
    }

    let Some(deadline) = crate::discovery::quota_deadline(reset_ms) else {
        return;
    };
    state.discovery.open_quota_gate(deadline);
    tracing::warn!("429 de cuota diaria free; gate abierto hasta el reset");
}

/// Recorre la cadena de backends de `route` en orden hasta obtener una
/// respuesta válida (**failover antes del primer byte**): reintenta en error de
/// conexión, timeout, `429` o `5xx`; un `4xx` distinto de `429` se devuelve tal
/// cual. Un backend cuya `api_key_env` no resuelve se salta como si fallara.
///
/// En **no-stream**, si la petición exige un `tool_call` (ver
/// [`request_requires_tool_call`]) y una respuesta de **éxito** (`2xx`) no lo
/// trae (ver [`response_has_tool_call`]), ese intento se registra como
/// [`AttemptResult::NoToolCall`] y se prueba el siguiente backend. Un
/// `4xx`/`3xx` no se inspecciona por esta vía: se devuelve tal cual, como antes.
///
/// Al agotar la cadena, el motivo del fallo decide el error devuelto: si hubo al
/// menos un intento `NoToolCall` **tiene prioridad**, y se devuelve
/// [`AppError::NoToolCallFromAllBackends`] sobre cualquier otro motivo
/// (p. ej. [`AppError::AllBackendsFailed`]).
///
/// Los eventos resumen ("petición completada", "petición fallida") repiten
/// `request_id`, `route` y `streaming` como campos del **evento**: el bridge de
/// `opentelemetry-appender-tracing` exporta los campos del evento y **no**
/// hereda los del span que lo cubre, y la spec exige conservarlos en el log.
///
/// En modo `streaming`, al primer backend OK se devuelve su stream sin
/// consumir; a partir de ahí ya no hay failover.
async fn route_with_failover(
    state: &AppState,
    route_idx: usize,
    route: &Route,
    body: &Value,
    streaming: bool,
    request_id: &str,
) -> Result<Response, AppError> {
    let started = Instant::now();
    let mut attempts = 0usize;
    let mut considered = 0usize;
    // Último candidato probado: el evento `gateway_requests` etiqueta con el
    // modelo del último intento (el elegido si hubo éxito).
    let mut last: Option<&Candidate> = None;
    // Se decide una sola vez por petición y solo aplica en no-stream.
    let requires_tool_call = !streaming && request_requires_tool_call(body);
    let mut no_tool_call_seen = false;

    // Los backends dinámicos se expanden en su posición a modelos concretos.
    let candidates = expand_backends(state, route_idx, route).await;

    for candidate in &candidates {
        considered += 1;
        last = Some(candidate);
        let backend_idx = candidate.slot;
        // Resuelto una vez por intento: alimenta los logs de todos los caminos y
        // la cabecera `x-gateway-backend`, sin `format!` repetidos.
        let candidate_id = candidate.id();
        let attempt_started = Instant::now();

        let Some(api_key) = resolve_api_key_env(candidate.api_key_env.as_deref(), &candidate_id)
        else {
            state.metrics.record_attempt(
                route_idx,
                backend_idx,
                AttemptResult::MissingApiKey,
                attempt_started.elapsed(),
            );
            tracing::warn!(
                backend = %candidate_id,
                result = AttemptResult::MissingApiKey.label(),
                latency_ms = attempt_started.elapsed().as_millis() as u64,
                "backend sin api_key; probando el siguiente"
            );
            continue;
        };
        attempts += 1;

        let mut payload = body_for_model(body, &candidate.model);
        // En streaming se pide al upstream que incluya el `usage` en el chunk
        // final (`stream_options.include_usage: true`), base del ledger.
        if streaming {
            inject_include_usage(&mut payload);
        }
        let send_result = send(
            &state.client,
            candidate,
            &candidate_id,
            payload,
            streaming,
            api_key.as_deref(),
        )
        .await;

        // Blacklist dinámica: un `429`/`5xx`/`Retry-After` marca el modelo.
        if let (Ok(response), Some(cooldown)) = (&send_result, candidate.dynamic_cooldown) {
            blacklist_if_needed(state, &candidate.model, response, cooldown);
        }

        match send_result {
            Ok(response) if is_retryable(response.status()) => {
                let status = response.status();
                // Un `429` del tier free puede indicar agotamiento diario: se
                // inspecciona (cuerpo incluido) antes de descartar la respuesta.
                // Solo si el backend respeta el gate de cuota diaria: un `429` de
                // un backend con `respect_daily_quota: false` no debe abrirlo.
                if candidate.free_tier
                    && candidate.respect_daily_quota
                    && status == StatusCode::TOO_MANY_REQUESTS
                {
                    detect_daily_quota(state, response).await;
                }
                state.metrics.record_attempt(
                    route_idx,
                    backend_idx,
                    AttemptResult::RetryableStatus,
                    attempt_started.elapsed(),
                );
                tracing::warn!(
                    backend = %candidate_id,
                    status = %status,
                    result = AttemptResult::RetryableStatus.label(),
                    latency_ms = attempt_started.elapsed().as_millis() as u64,
                    "backend retornó un status reintentable; probando el siguiente"
                );
            }
            Ok(response) => {
                if streaming {
                    state.metrics.record_attempt(
                        route_idx,
                        backend_idx,
                        AttemptResult::Success,
                        attempt_started.elapsed(),
                    );
                    tracing::debug!(
                        backend = %candidate_id,
                        result = AttemptResult::Success.label(),
                        latency_ms = attempt_started.elapsed().as_millis() as u64,
                        "intento completado"
                    );
                    if attempts > 1 {
                        state.metrics.record_failover(route_idx);
                    }
                    tracing::info!(
                        request_id = %request_id,
                        route = %route.name,
                        streaming,
                        outcome = Outcome::Success.label(),
                        attempts,
                        latency_ms = started.elapsed().as_millis() as u64,
                        "petición completada"
                    );
                    // Ledger de observabilidad (#39): el evento `request` se
                    // emite al agotarse el stream (chunk centinela), fuera del
                    // camino de bytes. Los precios se resuelven ahora, en el
                    // momento de la petición; tokens, `finish_reason`, latencia
                    // y coste se completan al terminar el stream.
                    let price = state.discovery.model_price(&candidate.base_url, &candidate.model);
                    let event = RequestEvent {
                        event_type: REQUEST_EVENT_TYPE,
                        ts: 0,
                        request_id: request_id.to_owned(),
                        route: route.name.clone(),
                        model_id: Some(candidate.model.clone()),
                        streaming,
                        attempts,
                        failover: attempts > 1,
                        outcome: Outcome::Success.label(),
                        finish_reason: None,
                        latency_ms: 0,
                        tokens_in: None,
                        tokens_out: None,
                        pin: price.map(|(pin, _)| pin),
                        pout: price.map(|(_, pout)| pout),
                        cost_usd: None,
                    };
                    return Ok(stream_response(
                        response,
                        &candidate_id,
                        Some(RequestSink { event, started }),
                    ));
                }

                match full_response(response, &candidate_id).await {
                    Ok((response, body_bytes)) => {
                        if requires_tool_call
                            && response.status().is_success()
                            && !response_has_tool_call(&body_bytes)
                        {
                            state.metrics.record_attempt(
                                route_idx,
                                backend_idx,
                                AttemptResult::NoToolCall,
                                attempt_started.elapsed(),
                            );
                            tracing::warn!(
                                backend = %candidate_id,
                                result = AttemptResult::NoToolCall.label(),
                                latency_ms = attempt_started.elapsed().as_millis() as u64,
                                "respuesta sin tool_calls cuando la petición los exige; probando el siguiente"
                            );
                            no_tool_call_seen = true;
                            continue;
                        }
                        // Ledger de observabilidad (#39): tokens y
                        // `finish_reason` de la respuesta no-stream, con un
                        // único parseo del cuerpo; sin `usage` quedan
                        // desconocidos.
                        let (usage, finish_reason) = parse_body_ledger(&body_bytes);
                        if !usage.is_unknown() {
                            tracing::debug!(
                                backend = %candidate_id,
                                tokens_in = ?usage.prompt_tokens,
                                tokens_out = ?usage.completion_tokens,
                                "usage de la respuesta"
                            );
                        }
                        state.metrics.record_attempt(
                            route_idx,
                            backend_idx,
                            AttemptResult::Success,
                            attempt_started.elapsed(),
                        );
                        tracing::debug!(
                            backend = %candidate_id,
                            result = AttemptResult::Success.label(),
                            latency_ms = attempt_started.elapsed().as_millis() as u64,
                            "intento completado"
                        );
                        if attempts > 1 {
                            state.metrics.record_failover(route_idx);
                        }
                        tracing::info!(
                            request_id = %request_id,
                            route = %route.name,
                            streaming,
                            outcome = Outcome::Success.label(),
                            attempts,
                            latency_ms = started.elapsed().as_millis() as u64,
                            "petición completada"
                        );
                        // Ledger de observabilidad (#39): el evento `request`
                        // con los precios del modelo en el momento de la
                        // petición; un fallo del emisor no altera la respuesta
                        // (4.3).
                        let price = state.discovery.model_price(&candidate.base_url, &candidate.model);
                        let event = RequestEvent {
                            event_type: REQUEST_EVENT_TYPE,
                            ts: crate::discovery::now_ms(),
                            request_id: request_id.to_owned(),
                            route: route.name.clone(),
                            model_id: Some(candidate.model.clone()),
                            streaming,
                            attempts,
                            failover: attempts > 1,
                            outcome: Outcome::Success.label(),
                            finish_reason,
                            latency_ms: started.elapsed().as_millis() as u64,
                            tokens_in: usage.prompt_tokens,
                            tokens_out: usage.completion_tokens,
                            pin: price.map(|(pin, _)| pin),
                            pout: price.map(|(_, pout)| pout),
                            cost_usd: cost_usd(
                                usage.prompt_tokens,
                                usage.completion_tokens,
                                price.map(|(pin, _)| pin),
                                price.map(|(_, pout)| pout),
                            ),
                        };
                        emit_request_event(&event);
                        return Ok(response);
                    }
                    Err(err) => {
                        state.metrics.record_attempt(
                            route_idx,
                            backend_idx,
                            AttemptResult::BodyError,
                            attempt_started.elapsed(),
                        );
                        tracing::warn!(
                            backend = %candidate_id,
                            result = AttemptResult::BodyError.label(),
                            error = %err,
                            latency_ms = attempt_started.elapsed().as_millis() as u64,
                            "error leyendo la respuesta del backend; probando el siguiente"
                        );
                    }
                }
            }
            Err(err) => {
                let result = match err {
                    SendError::Transport(_) => AttemptResult::TransportError,
                    SendError::FirstByteTimeout(_) => AttemptResult::FirstByteTimeout,
                };
                state.metrics.record_attempt(
                    route_idx,
                    backend_idx,
                    result,
                    attempt_started.elapsed(),
                );
                tracing::warn!(
                    backend = %candidate_id,
                    result = result.label(),
                    error = %err,
                    latency_ms = attempt_started.elapsed().as_millis() as u64,
                    "backend inalcanzable; probando el siguiente"
                );
            }
        }
    }

    if attempts > 1 {
        state.metrics.record_failover(route_idx);
    }
    tracing::warn!(
        request_id = %request_id,
        route = %route.name,
        streaming,
        outcome = Outcome::AllFailed.label(),
        attempts,
        considered,
        latency_ms = started.elapsed().as_millis() as u64,
        "petición fallida"
    );

    // Ledger de observabilidad (#39): la petición agotó la cadena; el evento
    // etiqueta con el modelo del último intento (si lo hubo) y registra el
    // resto de campos con lo conocido: un fallo también es un dato del ledger.
    let (model_id, price) = last.map_or((None, None), |candidate| {
        let price = state.discovery.model_price(&candidate.base_url, &candidate.model);
        (Some(candidate.model.clone()), price)
    });
    let event = RequestEvent {
        event_type: REQUEST_EVENT_TYPE,
        ts: crate::discovery::now_ms(),
        request_id: request_id.to_owned(),
        route: route.name.clone(),
        model_id,
        streaming,
        attempts,
        failover: attempts > 1,
        outcome: Outcome::AllFailed.label(),
        finish_reason: None,
        latency_ms: started.elapsed().as_millis() as u64,
        tokens_in: None,
        tokens_out: None,
        pin: price.map(|(pin, _)| pin),
        pout: price.map(|(_, pout)| pout),
        cost_usd: None,
    };
    emit_request_event(&event);

    // Se distingue el motivo del agotamiento: si algún intento falló por no
    // traer `tool_calls`, el cliente recibe un código específico.
    if no_tool_call_seen {
        return Err(AppError::NoToolCallFromAllBackends {
            model: route.name.clone(),
            attempts: considered,
        });
    }

    Err(AppError::AllBackendsFailed {
        model: route.name.clone(),
        attempts: considered,
    })
}

/// Resuelve la API key del backend, si procede.
///
/// - `api_key_env = None` → `Some(None)` (backend local, sin cabecera).
/// - `api_key_env = Some(var)` con la variable definida y no vacía → `Some(Some(key))`.
/// - `api_key_env = Some(var)` sin definir (o vacía) → `None`: el backend se
///   considera no disponible y se salta al siguiente.
///
/// Nunca registra el valor de la clave; solo, si acaso, el nombre de la
/// variable y el identificador del backend.
pub(crate) fn resolve_api_key(backend: &Backend) -> Option<Option<String>> {
    resolve_api_key_env(backend.api_key_env.as_deref(), &backend_id(backend))
}

/// Implementación común de [`resolve_api_key`] para backends de configuración y
/// candidatos ya expandidos. `backend_label` solo se usa para los logs.
fn resolve_api_key_env(api_key_env: Option<&str>, backend_label: &str) -> Option<Option<String>> {
    match api_key_env {
        None => Some(None),
        Some(var) => match std::env::var(var) {
            Ok(value) if !value.is_empty() => Some(Some(value)),
            _ => {
                tracing::warn!(
                    backend = %backend_label,
                    env_var = %var,
                    "api_key_env sin resolver; se salta el backend"
                );
                None
            }
        },
    }
}

/// Indica si un status del upstream permite reintentar con otro backend
/// (`429` o `5xx`); un `4xx` distinto de `429` no se reintenta.
fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Indica si la petición **exige** una llamada a herramienta: requiere que
/// `tools` sea una lista no vacía y que `tool_choice` sea `required`, `any` o
/// un objeto que nombre una función.
///
/// Con `tool_choice` igual a `auto`, `none`, ausente o de forma inesperada, no
/// se aplica la condición post-respuesta (el modelo puede legítimamente no
/// llamar a ninguna herramienta).
fn request_requires_tool_call(body: &Value) -> bool {
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if !has_tools {
        return false;
    }

    match body.get("tool_choice") {
        Some(Value::String(choice)) => choice == "required" || choice == "any",
        Some(Value::Object(choice)) => choice.contains_key("function"),
        _ => false,
    }
}

/// Indica si el cuerpo de la respuesta contiene alguna llamada a herramienta
/// (`choices[*].message.tool_calls` no vacío).
///
/// La inspección es **conservadora**: si el cuerpo no parsea como JSON, no trae
/// `choices` (o está vacío) o la forma de alguna `choice` es inesperada, se
/// devuelve `true` para no penalizar una respuesta rara (se tratará como
/// éxito). Solo devuelve `false` cuando se confirma la ausencia de `tool_calls`
/// en todas las `choices`.
fn response_has_tool_call(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return true;
    };

    let Some(choices) = value
        .get("choices")
        .and_then(Value::as_array)
        .filter(|choices| !choices.is_empty())
    else {
        return true;
    };

    let mut absence_confirmed = true;
    for choice in choices {
        // Sin `message` no se puede confirmar la ausencia: no penalizamos.
        let Some(message) = choice.get("message") else {
            absence_confirmed = false;
            continue;
        };
        match message.get("tool_calls") {
            // Ausente o lista vacía: esta `choice` no trae llamada.
            None => {}
            Some(Value::Array(tool_calls)) if tool_calls.is_empty() => {}
            // Lista no vacía: hay llamada a herramienta.
            Some(Value::Array(_)) => return true,
            // Forma inesperada: inspección no concluyente, no penalizamos.
            Some(_) => absence_confirmed = false,
        }
    }

    !absence_confirmed
}

/// Envía la petición al `candidate` y devuelve la respuesta con las cabeceras
/// ya disponibles, **sin** consumir el cuerpo (permite decidir el failover antes
/// del primer byte). `backend_label` es el identificador ya resuelto del
/// candidato (para el span). Si `api_key` es `Some`, añade `Authorization: Bearer`.
///
/// **Invariante:** `backend_label == candidate.id()`; el llamante lo resuelve una
/// vez y lo comparte con los logs y la cabecera `x-gateway-backend`.
///
/// - **No-stream**: se aplica **siempre** un timeout total (`request.timeout(budget)`),
///   con `budget = timeout_ms` si el backend lo define, o [`DEFAULT_REQUEST_TIMEOUT`]
///   (120 s) en caso contrario. Así una petición no-stream colgada nunca deja la
///   conexión abierta indefinidamente.
/// - **Stream**: se aplica un guard de **time-to-first-byte** con
///   `tokio::time::timeout(timeout_ms | default, request.send())`, que solo
///   limita el arranque: en cuanto llegan las cabeceras, el cuerpo fluye sin
///   deadline. **No** se usa un timeout global, que abortaría respuestas largas.
async fn send(
    client: &reqwest::Client,
    candidate: &Candidate,
    backend_label: &str,
    payload: Value,
    streaming: bool,
    api_key: Option<&str>,
) -> Result<reqwest::Response, SendError> {
    // Span de cliente por cada POST a un backend. Se crea antes de enviar para
    // poder inyectar su contexto (`traceparent`) en las cabeceras salientes.
    let span = tracing::info_span!(
        "backend_request",
        backend = %backend_label,
        model = %candidate.model,
        streaming,
        "otel.kind" = "client",
    );

    let mut request = client.post(chat_url(&candidate.base_url)).json(&payload);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let mut trace_headers = HeaderMap::new();
    inject_traceparent(&span, &mut trace_headers);
    request = request.headers(trace_headers);

    if streaming {
        let budget = candidate
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_FIRST_BYTE_TIMEOUT);
        match tokio::time::timeout(budget, request.send())
            .instrument(span)
            .await
        {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(err)) => Err(SendError::Transport(err)),
            Err(_elapsed) => Err(SendError::FirstByteTimeout(budget)),
        }
    } else {
        let budget = candidate
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        request = request.timeout(budget);
        request
            .send()
            .instrument(span)
            .await
            .map_err(SendError::Transport)
    }
}

/// Convierte la respuesta del upstream en una respuesta no-stream (buffered).
///
/// Devuelve además los bytes del cuerpo ya buffereado para que el enrutado
/// pueda inspeccionarlos (p. ej. para comprobar la presencia de `tool_calls`)
/// sin recomputarlos; el `Body` de la respuesta se construye a partir de una
/// copia barata de esos mismos bytes, así que lo devuelto al cliente es
/// idéntico al upstream.
async fn full_response(
    response: reqwest::Response,
    backend_label: &str,
) -> Result<(Response, Bytes), AppError> {
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = response.bytes().await.map_err(AppError::Upstream)?;

    let response = assemble(
        status,
        content_type,
        Body::from(body.clone()),
        DEFAULT_CONTENT_TYPE,
        backend_label,
    );

    Ok((response, body))
}

/// Reenvía el cuerpo del upstream como stream SSE, **sin bufferizar**, usando
/// el stream de `reqwest` directamente.
///
/// A medida que el stream fluye, extrae el `usage` del chunk final ([`StreamUsage`])
/// para el ledger de observabilidad, sin alterar los bytes que recibe el cliente.
///
/// Si el upstream se corta a mitad, el error del stream propaga al cliente y
/// la conexión se cierra con error; no se reintenta (limitación documentada).
fn stream_response(
    response: reqwest::Response,
    backend_label: &str,
    sink: Option<RequestSink>,
) -> Response {
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let mut usage = StreamUsage::default();
    let mut sink = sink;
    // Centinela de fin de stream (un `Bytes` vacío tras el último chunk) para
    // procesar la línea parcial pendiente (EOF sin `\n` final). En HTTP/2 un
    // DATA(0, END_STREAM) sí llega como chunk vacío en `bytes_stream` (igual
    // que los frames vacíos intermedios); no colisiona con datos reales
    // porque un chunk vacío no aporta bytes, `finish` es idempotente (sin
    // línea pendiente no hace nada) y el `usage` solo se retiene con JSON
    // válido.
    let body = Body::from_stream(
        response
            .bytes_stream()
            .chain(futures_util::stream::once(std::future::ready(Ok(
                Bytes::new(),
            ))))
            .filter_map(move |result| {
                let keep = match &result {
                    Ok(bytes) if bytes.is_empty() => {
                        usage.finish();
                        // Ledger de observabilidad (#39): el evento `request`
                        // se emite al agotarse el stream, fuera del camino de
                        // bytes; el passthrough no se altera y un fallo del
                        // emisor queda aislado (4.3).
                        if let Some(sink) = sink.take() {
                            sink.complete(&usage);
                        }
                        None
                    }
                    Ok(bytes) => {
                        usage.feed(bytes);
                        Some(result)
                    }
                    Err(_) => Some(result),
                };
                std::future::ready(keep)
            }),
    );

    assemble(
        status,
        content_type,
        body,
        DEFAULT_STREAM_CONTENT_TYPE,
        backend_label,
    )
}

/// Compone la respuesta final preservando el `content-type` real del upstream
/// (con un fallback sensato) y, en caso de éxito, añade la cabecera
/// `x-gateway-backend` con el identificador del backend usado.
fn assemble(
    status: StatusCode,
    content_type: Option<HeaderValue>,
    body: Body,
    default_content_type: &'static str,
    backend_label: &str,
) -> Response {
    let content_type =
        content_type.unwrap_or_else(|| HeaderValue::from_static(default_content_type));

    let mut response = (status, [(header::CONTENT_TYPE, content_type)], body).into_response();

    if status.is_success()
        && let Ok(value) = HeaderValue::from_str(backend_label)
    {
        response
            .headers_mut()
            .insert(HeaderName::from_static(BACKEND_HEADER), value);
    }

    response
}

/// Tokens de una respuesta, con `None` para los valores **desconocidos**.
///
/// Un campo `None` significa que el upstream no informó ese token; nunca se
/// sustituye por `0`: un cero inventado contaminaría costes y métricas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TokenUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

impl TokenUsage {
    /// Indica si no se conoció ningún token (`usage` ausente o nulo).
    fn is_unknown(&self) -> bool {
        self.prompt_tokens.is_none() && self.completion_tokens.is_none()
    }
}

/// Extrae el `usage` (tokens de prompt y de completion) de un objeto JSON de
/// respuesta o de un evento SSE ya parseado.
///
/// Campos ausentes, `usage: null` o valores no numéricos quedan como `None`
/// (desconocidos); no se inventan ceros.
fn parse_usage(value: &Value) -> TokenUsage {
    let usage = value.get("usage");
    TokenUsage {
        prompt_tokens: usage
            .and_then(|usage| usage.get("prompt_tokens"))
            .and_then(Value::as_u64),
        completion_tokens: usage
            .and_then(|usage| usage.get("completion_tokens"))
            .and_then(Value::as_u64),
    }
}

/// Parsea el cuerpo de una respuesta no-stream y extrae su `usage`.
///
/// Un cuerpo que no sea JSON válido produce tokens desconocidos.
fn parse_body_usage(bytes: &[u8]) -> TokenUsage {
    serde_json::from_slice::<Value>(bytes)
        .map(|value| parse_usage(&value))
        .unwrap_or_default()
}

/// Inyecta `stream_options.include_usage: true` en el payload de una petición
/// en streaming, para que el upstream añada el `usage` al chunk final.
///
/// Preserva el resto de `stream_options` y fuerza `include_usage` a `true`
/// aunque el cliente no lo pida (el ledger lo necesita).
fn inject_include_usage(payload: &mut Value) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    let options = object
        .entry("stream_options".to_owned())
        .and_modify(|value| {
            // Un valor presente pero no-objeto (p. ej. `null`) se reemplaza:
            // con `entry().or_insert_with` a secas sobreviviría y la inyección
            // se perdería en silencio (`as_object_mut` daría `None`).
            if !value.is_object() {
                *value = Value::Object(Map::new());
            }
        })
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(options) = options.as_object_mut() {
        options.insert("include_usage".to_owned(), Value::Bool(true));
    }
}

/// Cap del buffer de reensamblado de líneas SSE (bytes). Una línea parcial que
/// lo supere se descarta: el `usage` del chunk final siempre llega en una
/// línea corta, y el cap solo acota la memoria ante upstreams malformados.
const MAX_SSE_LINE_BYTES: usize = 64 * 1024;

/// Acumula las líneas SSE que llegan troceadas y retiene el último `usage`
/// conocido visto (el del chunk final de un stream con `include_usage`).
///
/// El buffer es de bytes: la conversión a UTF-8 (lossy) se hace una vez por
/// línea completa, no en cada fragmento (así un codepoint multibyte partido
/// entre chunks no se corrompe con caracteres de reemplazo espurios).
#[derive(Default)]
struct StreamUsage {
    buffer: Vec<u8>,
    /// `true` mientras se descarta una línea parcial sobredimensionada.
    discarding: bool,
    usage: TokenUsage,
    /// Último `finish_reason` **no nulo** visto en los chunks (`choices[0]`).
    finish_reason: Option<String>,
}

impl StreamUsage {
    /// Procesa un fragmento de bytes del stream (límites de chunk arbitrarios;
    /// una línea `data:` puede llegar partida entre varios fragmentos).
    ///
    /// Solo se procesan las líneas completas: el resto queda en el buffer a la
    /// espera del siguiente fragmento. Una línea parcial que supere
    /// [`MAX_SSE_LINE_BYTES`] se descarta (junto con todo lo que reste de ella
    /// hasta su `\n`) para acotar la memoria del reensamblado.
    fn feed(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while !rest.is_empty() {
            if self.discarding {
                // Línea sobredimensionada en curso: se salta hasta su `\n`
                // (incluido) sin acumular nada.
                let Some(pos) = rest.iter().position(|&byte| byte == b'\n') else {
                    return;
                };
                self.discarding = false;
                rest = &rest[pos + 1..];
            } else if let Some(pos) = rest.iter().position(|&byte| byte == b'\n') {
                let (line, tail) = rest.split_at(pos);
                self.buffer.extend_from_slice(line);
                self.consume_buffered_line();
                rest = &tail[1..];
            } else {
                self.buffer.extend_from_slice(rest);
                if self.buffer.len() > MAX_SSE_LINE_BYTES {
                    self.buffer.clear();
                    self.discarding = true;
                }
                return;
            }
        }
    }

    /// Procesa la línea parcial pendiente al terminar el stream (EOF sin `\n`
    /// final), para no perder el `usage` de upstreams que no cierran la última
    /// línea. Idempotente: sin línea pendiente no hace nada.
    fn finish(&mut self) {
        if self.buffer.is_empty() {
            self.discarding = false;
            return;
        }
        self.consume_buffered_line();
    }

    /// Consume la línea completa acumulada en el buffer. Las líneas
    /// sobredimensionadas se descartan sin parsear (el `usage` del chunk final
    /// siempre llega en una línea corta).
    fn consume_buffered_line(&mut self) {
        let bytes = std::mem::take(&mut self.buffer);
        if bytes.len() > MAX_SSE_LINE_BYTES {
            return;
        }
        let line = String::from_utf8_lossy(&bytes);
        self.consume_line(line.trim_end_matches(['\r', '\n']));
    }

    /// Consume una línea SSE completa. Solo los eventos `data:` con un JSON
    /// que traiga `usage` **conocido** actualizan el valor retenido: los
    /// intermedios (`usage: null`) y `data: [DONE]` no lo tocan.
    fn consume_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim_start();
        if data == "[DONE]" {
            return;
        }
        // Pre-filtro barato: la gran mayoría de los chunks del stream no traen
        // `usage`, y así se evita parsear JSON por cada línea. Comportamiento
        // idéntico (salvo claves `\uXXXX` escapadas, irreales en la práctica).
        if !data.contains("\"usage\"") {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if data.contains("\"usage\"") {
            let usage = parse_usage(&value);
            if !usage.is_unknown() && usage != self.usage {
                tracing::debug!(
                    tokens_in = ?usage.prompt_tokens,
                    tokens_out = ?usage.completion_tokens,
                    "usage del stream (chunk final)"
                );
                self.usage = usage;
            }
        }
        // Pre-filtro barato análogo al del `usage`: solo se parsea la línea
        // cuando puede traer `finish_reason` (los chunks intermedios traen
        // `null`, que no sobrescribe el último conocido).
        if data.contains("\"finish_reason\"")
            && let Some(reason) = extract_finish_reason(&value)
        {
            self.finish_reason = Some(reason);
        }
    }
}

// --- Ledger de observabilidad: evento de petición (`gateway_requests`) ------

/// `event_type` del evento de petición del ledger (singular, como el resto de
/// eventos: `catalog`, `selection`, `request`).
const REQUEST_EVENT_TYPE: &str = "request";

/// Evento estructurado `gateway_requests`: un evento por petición atendida
/// (D5), con el resultado del enrutado, el `usage` del upstream y el coste
/// calculado con los precios **en el momento de la petición**.
///
/// Los precios (`pin`, `pout`) van en USD por millón de tokens, la misma
/// unidad de `gateway_catalog` y `gateway_selection`; `cost_usd` se calcula
/// como `tokens_in·pin + tokens_out·pout` (precios por token, es decir,
/// divididos entre un millón) y va en USD. Cuando falta el `usage` (o el
/// precio del modelo), tokens y coste quedan **desconocidos** (`null`): nunca
/// se inventa un `0`.
#[derive(Debug, Clone, Serialize)]
struct RequestEvent {
    /// Tipo de evento del ledger: siempre `request`.
    event_type: &'static str,
    /// Momento de la emisión (fin de la petición), en ms epoch.
    ts: u64,
    /// Identificador de correlación de la petición.
    request_id: String,
    /// Nombre de la ruta atendida (`route.name` de la configuración).
    route: String,
    /// Modelo del último intento; `None` si no se intentó ninguno.
    model_id: Option<String>,
    /// `true` si la petición era `stream: true`.
    streaming: bool,
    /// Intentos reales contra upstream (con la API key resuelta).
    attempts: usize,
    /// `true` si hizo falta más de un intento (failover antes del primer
    /// byte).
    failover: bool,
    /// Resultado global de la petición (`success`/`all_failed`).
    outcome: &'static str,
    /// `finish_reason` informado por el upstream, si se conoció.
    finish_reason: Option<String>,
    /// Latencia total de la petición, en ms (en streaming, hasta agotar el
    /// stream).
    latency_ms: u64,
    /// Tokens de entrada (`usage.prompt_tokens`); `None` si no se informó.
    tokens_in: Option<u64>,
    /// Tokens de salida (`usage.completion_tokens`); `None` si no se informó.
    tokens_out: Option<u64>,
    /// Precio de entrada del modelo elegido, en USD/Mtok, en el momento de la
    /// petición.
    pin: Option<f64>,
    /// Precio de salida del modelo elegido, en USD/Mtok, en el momento de la
    /// petición.
    pout: Option<f64>,
    /// Coste de la petición en USD; `None` si faltan tokens o precios.
    cost_usd: Option<f64>,
}

/// `cost_usd` de una petición: `tokens_in·pin + tokens_out·pout`, con los
/// precios en USD/Mtok y el resultado en USD.
///
/// Devuelve `None` si falta cualquier pieza (un token desconocido o un precio
/// desconocido): datos parciales no inventan un coste. Un coste `0.0` **con
/// precios conocidos** sí es legítimo (0 tokens consumidos es un dato, no una
/// ausencia).
fn cost_usd(
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    pin: Option<f64>,
    pout: Option<f64>,
) -> Option<f64> {
    let _ = (tokens_in, tokens_out, pin, pout);
    None
}

/// `finish_reason` del primer `choices[0]` de una respuesta o evento SSE de
/// chat. Un `null` (chunks intermedios del stream) o su ausencia devuelven
/// `None`: solo un valor informado cuenta.
fn extract_finish_reason(value: &Value) -> Option<String> {
    let _ = value;
    None
}

/// `usage` y `finish_reason` de una respuesta no-stream, con un **único**
/// parseo del cuerpo (hot path del ledger). Un cuerpo que no sea JSON válido
/// produce tokens desconocidos y `finish_reason` `None`.
fn parse_body_ledger(bytes: &[u8]) -> (TokenUsage, Option<String>) {
    let _ = bytes;
    (TokenUsage::default(), None)
}

/// Emite el evento `gateway_requests` como log estructurado con `event_type`
/// `request` (D5): `tracing` lo envía a OpenObserve vía el stack OTLP de
/// [`crate::telemetry`]. Es el punto de extensión para cambiar de transport
/// sin tocar la lógica de la petición.
fn emit_request_event(_event: &RequestEvent) {}

/// Receptor del evento `gateway_requests` de una petición **en streaming**.
///
/// Se emite al agotarse el stream (el centinela de [`stream_response`]), fuera
/// del camino de bytes: el passthrough no se toca y no hay locks contenciosos
/// (el `StreamUsage` queda atrapado en el closure del stream y este sink es el
/// único que lo lee, al final).
struct RequestSink {
    /// Campos ya conocidos en el momento del enrutado, con los precios del
    /// modelo resueltos entonces (`momento de la petición`).
    event: RequestEvent,
    /// Instante de inicio de la petición: la latencia del evento cubre todo el
    /// streaming, no solo hasta el primer byte.
    started: Instant,
}

impl RequestSink {
    /// Completa el evento con el `usage`/`finish_reason` retenidos por el
    /// extractor del stream y lo emite.
    fn complete(self, _extractor: &StreamUsage) {}
}

/// Clona `body` y sustituye su `model` por `model`. El resto de campos
/// (incluidos los desconocidos) se conservan intactos.
fn body_for_model(body: &Value, model: &str) -> Value {
    let mut value = body.clone();
    if let Some(object) = value.as_object_mut() {
        object.insert("model".to_owned(), Value::String(model.to_owned()));
    }
    value
}

/// Clona `body` y sustituye su `model` por el del backend. El resto de campos
/// (incluidos los desconocidos) se conservan intactos.
///
/// Un backend sin `model` (p. ej. un dinámico sin expandir) devuelve el cuerpo
/// tal cual.
pub(crate) fn body_for_backend(body: &Value, backend: &Backend) -> Value {
    match backend.model.as_deref() {
        Some(model) => body_for_model(body, model),
        None => body.clone(),
    }
}

/// Compone la URL del endpoint de chat a partir de la `base_url` del backend
/// (que ya incluye `/v1`), uniendo bien las barras.
pub(crate) fn chat_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// Devuelve el `x-request-id` del cliente si es válido, o genera uno.
///
/// Se acepta un valor ASCII visible de 1 a 128 bytes; cualquier otra cosa se
/// descarta para no propagar cabeceras malformadas ni valores arbitrariamente
/// largos a los logs.
/// Enlaza `span` con el contexto de traza de las cabeceras entrantes
/// (`traceparent`) si es válido; si no, el span queda como raíz de la traza.
///
/// `set_parent` devuelve error si no hay layer de `tracing-opentelemetry`
/// instalado (p. ej. sin `signals.traces`): es benigno y se ignora.
fn link_remote_parent(span: &tracing::Span, headers: &HeaderMap) {
    use opentelemetry::trace::TraceContextExt as _;

    let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&opentelemetry_http::HeaderExtractor(headers))
    });
    if parent.span().span_context().is_valid() {
        let _ = span.set_parent(parent);
    }
}

/// Inyecta el contexto de traza de `span` en las cabeceras salientes
/// (`traceparent`) para que el backend continúe la traza distribuida.
fn inject_traceparent(span: &tracing::Span, headers: &mut HeaderMap) {
    let context = span.context();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut opentelemetry_http::HeaderInjector(headers))
    });
}

fn resolve_request_id(headers: &HeaderMap) -> String {
    if let Some(value) = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        && !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return value.to_owned();
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "req-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Añade `x-request-id` a la respuesta.
fn with_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    response
}

/// `GET /v1/models`: lista los nombres lógicos de ruta y los modelos dinámicos
/// elegibles del catálogo `openrouter_free`.
async fn list_models(State(state): State<AppState>) -> Json<Value> {
    let mut data: Vec<Value> = state
        .config
        .route
        .iter()
        .map(|route| json!({"id": route.name, "object": "model"}))
        .collect();

    for id in state.discovery.eligible_model_ids().await {
        data.push(json!({"id": id, "object": "model"}));
    }

    Json(json!({"object": "list", "data": data}))
}

/// `GET /healthz`: sondeo de salud.
async fn healthz() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::{
        AppState, Candidate, MAX_RETRY_AFTER, MAX_SSE_LINE_BYTES, QUOTA_BODY_LIMIT, QuotaSignal,
        RequestEvent, StreamUsage, TokenUsage, backend_id, chat_url, cost_usd,
        effective_cooldown, emit_request_event, expand_backends,
        headers_show_daily_exhaustion, inject_include_usage, parse_body_ledger, parse_body_usage,
        parse_retry_after, parse_usage, quota_reset_epoch_ms, quota_signal, read_quota_body,
        request_requires_tool_call, response_has_tool_call,
    };
    use crate::config::{Backend, BackendKind, GatewayConfig};
    use crate::discovery::{ModelEntry, Pricing};
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use serde_json::json;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Modelo elegible (free + tool-calling) para precargar un catálogo.
    fn eligible_model(id: &str, context_length: u32) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            name: None,
            description: None,
            context_length: Some(context_length),
            pricing: Some(Pricing {
                prompt: Some(json!("0")),
                completion: Some(json!("0")),
            }),
            supported_parameters: Some(vec!["tools".to_owned(), "tool_choice".to_owned()]),
        }
    }

    /// Backend estático de test con `model`, `timeout_ms` y `api_key_env`.
    fn static_backend() -> Backend {
        Backend {
            kind: BackendKind::Static,
            base_url: "http://localhost:11434/v1".to_owned(),
            model: Some("qwen2.5:7b-instruct".to_owned()),
            api_key_env: Some("K".to_owned()),
            timeout_ms: Some(9000),
            profile: None,
            discovery: None,
            warmup: None,
        }
    }

    /// M5: `from_backend` copia solo los campos ligeros y conserva el id
    /// `<base_url>#<model>` que emite el gateway.
    #[test]
    fn candidate_from_backend_copies_light_fields() {
        let backend = static_backend();

        let candidate = Candidate::from_backend(2, &backend);

        assert_eq!(candidate.slot, 2);
        assert_eq!(candidate.model, "qwen2.5:7b-instruct");
        assert_eq!(candidate.base_url, "http://localhost:11434/v1");
        assert_eq!(candidate.timeout_ms, Some(9000));
        assert_eq!(candidate.api_key_env.as_deref(), Some("K"));
        assert!(
            candidate.dynamic_cooldown.is_none(),
            "un backend estático no tiene cooldown dinámico"
        );
        assert_eq!(candidate.id(), backend_id(&backend));
        assert_eq!(
            candidate.id(),
            "http://localhost:11434/v1#qwen2.5:7b-instruct"
        );
    }

    /// M5: `with_model` sustituye solo el modelo de un candidato ya construido.
    #[test]
    fn candidate_with_model_only_replaces_model() {
        let candidate =
            Candidate::from_backend(4, &static_backend()).with_model("dyn/aa:free".to_owned());

        assert_eq!(candidate.model, "dyn/aa:free");
        assert_eq!(candidate.slot, 4);
        assert_eq!(candidate.base_url, "http://localhost:11434/v1");
        assert_eq!(candidate.timeout_ms, Some(9000));
        assert_eq!(candidate.api_key_env.as_deref(), Some("K"));
        assert!(candidate.dynamic_cooldown.is_none());
        assert_eq!(candidate.id(), "http://localhost:11434/v1#dyn/aa:free");
    }

    /// M5: un dinámico sin `model` usa el mismo fallback `"dynamic"` que
    /// [`backend_id`]; los estáticos exigen `model` en `validate`.
    #[test]
    fn candidate_without_model_uses_dynamic_fallback() {
        let backend = Backend {
            kind: BackendKind::OpenrouterFree,
            model: None,
            ..static_backend()
        };

        let candidate = Candidate::from_backend(0, &backend);

        assert_eq!(candidate.model, "dynamic");
        assert_eq!(candidate.id(), backend_id(&backend));
    }

    /// El identificador `<base_url>#<model>` es idéntico en las tres rutas que
    /// lo producen: [`backend_id`], [`Backend::label`] y [`Candidate::id`].
    #[test]
    fn backend_identifiers_agree_across_formatters() {
        let backend = static_backend();
        let candidate = Candidate::from_backend(0, &backend);

        assert_eq!(backend_id(&backend), backend.label());
        assert_eq!(candidate.id(), backend.label());
        assert_eq!(candidate.id(), backend_id(&backend));
    }

    /// M5: `expand_backends` expande la ruta en su posición: el estático aporta
    /// un candidato y el dinámico (con `overrides` y `warmup`) uno por modelo
    /// elegible, arrastrando `timeout_ms` y `api_key_env` del backend.
    ///
    /// El lock del gauge se mantiene durante la expansión para que las purgas de
    /// la blacklist (que escriben el gauge global) no se crucen con otros tests;
    /// el runtime es `current_thread`, así que no bloquea otra tarea.
    #[tokio::test]
    async fn expand_backends_expands_dynamic_in_place_with_light_fields() {
        // El acceso a la blacklist toca el gauge global (sin etiquetas).
        let _guard = crate::metrics::BlacklistGaugeGuard::lock();
        let yaml = r#"
route:
  - name: expand-test
    backend:
      - base_url: "http://static.test/v1"
        model: "local/static"
        api_key_env: "STATIC_KEY"
        timeout_ms: 9000
      - kind: openrouter_free
        base_url: "http://dynamic.test/v1"
        api_key_env: "DYN_KEY"
        timeout_ms: 5000
        profile: code
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 7000
          overrides:
            include: ["beta/two:free"]
        warmup:
          interval_ms: 1000
"#;
        let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");
        let state = AppState::new(config).expect("estado de test");
        state
            .discovery
            .catalog("http://dynamic.test/v1")
            .expect("caché del backend dinámico")
            .seed(vec![
                eligible_model("alpha/one:free", 2000),
                eligible_model("beta/two:free", 4000),
            ]);

        let route = &state.config.route[0];
        let candidates = expand_backends(&state, 0, route).await;

        assert_eq!(candidates.len(), 3, "estático + dos modelos del catálogo");

        let static_candidate = &candidates[0];
        assert_eq!(static_candidate.slot, 0);
        assert_eq!(static_candidate.model, "local/static");
        assert_eq!(static_candidate.base_url, "http://static.test/v1");
        assert_eq!(static_candidate.timeout_ms, Some(9000));
        assert_eq!(static_candidate.api_key_env.as_deref(), Some("STATIC_KEY"));
        assert!(
            static_candidate.dynamic_cooldown.is_none(),
            "un estático no lleva cooldown dinámico"
        );

        // `overrides.include` fuerza `beta/two:free` al principio del perfil.
        assert_eq!(
            candidates[1..]
                .iter()
                .map(|candidate| candidate.model.as_str())
                .collect::<Vec<_>>(),
            ["beta/two:free", "alpha/one:free"],
            "los dinámicos se expanden en su posición y en orden de clasificación"
        );
        for candidate in &candidates[1..] {
            assert_eq!(candidate.slot, 1, "mismo backend dinámico");
            assert_eq!(candidate.base_url, "http://dynamic.test/v1");
            assert_eq!(candidate.timeout_ms, Some(5000), "timeout arrastrado");
            assert_eq!(
                candidate.api_key_env.as_deref(),
                Some("DYN_KEY"),
                "api_key_env arrastrado"
            );
            assert_eq!(
                candidate.dynamic_cooldown,
                Some(Duration::from_millis(7000)),
                "solo los dinámicos llevan el cooldown configurado"
            );
        }
    }

    #[test]
    fn parse_retry_after_reads_seconds() {
        assert_eq!(
            parse_retry_after(Some(&HeaderValue::from_static("30"))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after(Some(&HeaderValue::from_static(" 5 "))),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            parse_retry_after(Some(&HeaderValue::from_static("0"))),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after(None), None);
        assert_eq!(
            parse_retry_after(Some(&HeaderValue::from_static("no-es-una-fecha"))),
            None
        );
    }

    /// M4: `Retry-After` en formato fecha HTTP futura se interpreta como delta.
    #[test]
    fn parse_retry_after_reads_future_http_date() {
        let parsed = parse_retry_after(Some(&HeaderValue::from_static(
            "Wed, 21 Oct 2099 07:28:00 GMT",
        )))
        .expect("una fecha HTTP futura debe interpretarse");
        assert!(
            parsed > Duration::from_secs(3600),
            "el delta hasta 2099 es grande, got {parsed:?}"
        );
    }

    /// M4: una fecha HTTP ya pasada se interpreta como 0 (nunca negativa), para
    /// que el cooldown efectivo caiga al configurado.
    #[test]
    fn parse_retry_after_past_http_date_is_zero() {
        assert_eq!(
            parse_retry_after(Some(&HeaderValue::from_static(
                "Wed, 21 Oct 2015 07:28:00 GMT"
            ))),
            Some(Duration::ZERO)
        );
    }

    /// A2: un `Retry-After` muy lejano se acota al cap, no blacklistea años.
    #[test]
    fn effective_cooldown_caps_a_far_future_retry_after() {
        // ~10 años: mucho más que el cap, debe recortarse.
        let far = Duration::from_secs(60 * 60 * 24 * 365 * 10);
        assert_eq!(
            effective_cooldown(Some(far), Duration::from_secs(30)),
            MAX_RETRY_AFTER,
            "una fecha muy lejana no debe blacklistear el modelo durante años"
        );
    }

    /// A2: un `Retry-After` positivo por debajo del cap manda sobre el cooldown.
    #[test]
    fn effective_cooldown_uses_retry_after_when_larger() {
        assert_eq!(
            effective_cooldown(Some(Duration::from_secs(120)), Duration::from_secs(30)),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn chat_url_joins_slashes() {
        assert_eq!(
            chat_url("http://localhost:11434/v1"),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            chat_url("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            chat_url("http://localhost:11434/v1///"),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    /// `tools` como lista no vacía + `tool_choice` exigente.
    #[test]
    fn request_requires_tool_call_only_with_strict_choice() {
        let tools = json!([{"type": "function", "function": {"name": "f"}}]);

        for choice in [json!("required"), json!("any")] {
            assert!(
                request_requires_tool_call(&json!({"tools": tools, "tool_choice": choice})),
                "tool_choice {choice} debe exigir tool call"
            );
        }

        assert!(request_requires_tool_call(&json!({
            "tools": tools,
            "tool_choice": {"type": "function", "function": {"name": "f"}}
        })));

        for choice in [json!("auto"), json!("none")] {
            assert!(
                !request_requires_tool_call(&json!({"tools": tools, "tool_choice": choice})),
                "tool_choice {choice} NO debe exigir tool call"
            );
        }

        assert!(!request_requires_tool_call(&json!({"tools": tools})));
        assert!(!request_requires_tool_call(
            &json!({"tools": [], "tool_choice": "required"})
        ));
        assert!(!request_requires_tool_call(
            &json!({"tool_choice": "required"})
        ));
        assert!(!request_requires_tool_call(&json!({
            "tools": tools,
            "tool_choice": {"type": "function"}
        })));
    }

    /// Inspección conservadora: solo `false` si se confirma la ausencia.
    #[test]
    fn response_has_tool_call_is_conservative() {
        assert!(response_has_tool_call(b"no es json"));
        assert!(response_has_tool_call(br#"{"choices":[]}"#));
        assert!(response_has_tool_call(br#"{"sin":"choices"}"#));
        assert!(response_has_tool_call(br#"{"choices":"no-es-array"}"#));
        assert!(response_has_tool_call(
            br#"{"choices":[{"sin":"message"}]}"#
        ));
        assert!(response_has_tool_call(
            br#"{"choices":[{"message":{"tool_calls":"no-es-array"}}]}"#
        ));
        assert!(response_has_tool_call(
            br#"{"choices":[{"message":{"tool_calls":[{"id":"c1"}]}}]}"#
        ));
        // `message` presente sin `tool_calls` (o con lista vacía) es la
        // ausencia normal: se confirma y se permite el failover.
        assert!(!response_has_tool_call(
            br#"{"choices":[{"message":{"tool_calls":[]}}]}"#
        ));
        assert!(!response_has_tool_call(
            br#"{"choices":[{"message":{"role":"assistant"}}]}"#
        ));
        assert!(!response_has_tool_call(
            br#"{"choices":[{"message":{"content":"hola"}}]}"#
        ));
    }

    /// Milisegundos epoch UTC dentro de `delta` a partir de ahora.
    fn epoch_ms_in(delta: Duration) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("reloj posterior a 1970");
        u64::try_from((now + delta).as_millis()).expect("epoch en ms cabe en u64")
    }

    /// Scenario: `limit_source` distingue la cuota diaria del resto.
    #[test]
    fn quota_signal_reads_limit_source() {
        let daily = json!({"error": {"metadata": {"limit_source": "openrouter_free_tier_daily"}}});
        assert!(matches!(quota_signal(Some(&daily)), QuotaSignal::Daily));

        let minute =
            json!({"error": {"metadata": {"limit_source": "openrouter_free_tier_per_minute"}}});
        assert!(matches!(quota_signal(Some(&minute)), QuotaSignal::Other));

        let provider = json!({"error": {"metadata": {"provider_code": 429}}});
        assert!(matches!(quota_signal(Some(&provider)), QuotaSignal::Other));

        assert!(matches!(quota_signal(None), QuotaSignal::Unknown));
        let plain = json!({"error": {"message": "boom"}});
        assert!(matches!(quota_signal(Some(&plain)), QuotaSignal::Unknown));
    }

    /// Scenario: la heurística exige `Remaining: 0` y un reset a >5 min.
    #[test]
    fn header_heuristic_requires_zero_remaining_and_far_reset() {
        let remaining = HeaderName::from_static("x-ratelimit-remaining");
        let reset = HeaderName::from_static("x-ratelimit-reset");

        let mut far = HeaderMap::new();
        far.insert(remaining.clone(), HeaderValue::from_static("0"));
        far.insert(
            reset.clone(),
            HeaderValue::from_str(&epoch_ms_in(Duration::from_secs(6 * 3600)).to_string())
                .expect("valor de cabecera"),
        );
        assert!(
            headers_show_daily_exhaustion(&far, quota_reset_epoch_ms(&far)),
            "Remaining 0 con reset a 6 h debe abrir el gate"
        );

        let mut near = HeaderMap::new();
        near.insert(remaining.clone(), HeaderValue::from_static("0"));
        near.insert(
            reset.clone(),
            HeaderValue::from_str(&epoch_ms_in(Duration::from_secs(60)).to_string())
                .expect("valor de cabecera"),
        );
        assert!(
            !headers_show_daily_exhaustion(&near, quota_reset_epoch_ms(&near)),
            "un reset a 1 min no es agotamiento diario"
        );

        let mut with_remaining = HeaderMap::new();
        with_remaining.insert(remaining, HeaderValue::from_static("5"));
        with_remaining.insert(
            reset,
            HeaderValue::from_str(&epoch_ms_in(Duration::from_secs(6 * 3600)).to_string())
                .expect("valor de cabecera"),
        );
        assert!(
            !headers_show_daily_exhaustion(&with_remaining, quota_reset_epoch_ms(&with_remaining)),
            "con cuota restante > 0 no es agotamiento diario"
        );
    }

    #[test]
    fn quota_reset_epoch_ms_reads_header() {
        let name = HeaderName::from_static("x-ratelimit-reset");
        let mut headers = HeaderMap::new();
        assert_eq!(quota_reset_epoch_ms(&headers), None);

        headers.insert(name.clone(), HeaderValue::from_static("1700000000000"));
        assert_eq!(quota_reset_epoch_ms(&headers), Some(1_700_000_000_000));

        headers.insert(name, HeaderValue::from_static("nope"));
        assert_eq!(quota_reset_epoch_ms(&headers), None);
    }

    /// Modelo de pago elegible (precio positivo + tool-calling) de un catálogo.
    fn paid_model(id: &str, price: &str, context_length: u32) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            name: None,
            description: None,
            context_length: Some(context_length),
            pricing: Some(Pricing {
                prompt: Some(json!(price)),
                completion: Some(json!(price)),
            }),
            supported_parameters: Some(vec!["tools".to_owned(), "tool_choice".to_owned()]),
        }
    }

    /// Scenario 5.4: `expand_backends` expande un backend de pago en orden de
    /// precio, sin rotación, y conserva el `static` final como último recurso.
    #[tokio::test]
    async fn expand_backends_expands_paid_backend_without_rotation() {
        // El acceso a la blacklist toca el gauge global (sin etiquetas).
        let _guard = crate::metrics::BlacklistGaugeGuard::lock();
        let yaml = r#"
route:
  - name: paid-expand-test
    backend:
      - kind: openrouter_paid
        base_url: "http://paid.test/v1"
        api_key_env: "PAID_KEY"
        timeout_ms: 4000
        profile: code
        discovery:
          refresh_interval_ms: 300000
          blacklist_cooldown_ms: 7000
          allow: ["acme/flash-*"]
      - base_url: "http://static.test/v1"
        model: "local/final"
"#;
        let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");
        let state = AppState::new(config).expect("estado de test");
        state
            .discovery
            .catalog("http://paid.test/v1")
            .expect("caché del backend de pago")
            .seed(vec![
                paid_model("acme/flash-b", "0.1", 128_000),
                paid_model("acme/flash-a", "0.04", 8_000),
            ]);

        let route = &state.config.route[0];
        let candidates = expand_backends(&state, 0, route).await;

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.model.as_str())
                .collect::<Vec<_>>(),
            ["acme/flash-a", "acme/flash-b", "local/final"],
            "el de pago aporta sus modelos por precio y el estático cierra la ruta"
        );
        for candidate in &candidates[..2] {
            assert_eq!(candidate.slot, 0, "mismo backend de pago");
            assert_eq!(candidate.base_url, "http://paid.test/v1");
            assert_eq!(candidate.timeout_ms, Some(4000), "timeout arrastrado");
            assert_eq!(candidate.api_key_env.as_deref(), Some("PAID_KEY"));
            assert_eq!(
                candidate.dynamic_cooldown,
                Some(Duration::from_millis(7000)),
                "los fallos del modelo de pago también blacklistean"
            );
            assert!(
                !candidate.free_tier,
                "el respaldo de pago no pertenece al tier free"
            );
        }
        assert_eq!(
            candidates[2].slot, 1,
            "el estático final es el siguiente backend"
        );

        let again = expand_backends(&state, 0, route).await;
        assert_eq!(
            again[0].model, "acme/flash-a",
            "sin rotación el primer candidato es el mismo en expansiones sucesivas"
        );
    }

    /// Scenario 5.7: sin `allow` el backend de pago no aporta candidatos y la
    /// ruta continúa con el siguiente backend declarado.
    #[tokio::test]
    async fn expand_backends_paid_without_allow_ships_only_the_next_backend() {
        let _guard = crate::metrics::BlacklistGaugeGuard::lock();
        let yaml = r#"
route:
  - name: paid-no-allow-test
    backend:
      - kind: openrouter_paid
        base_url: "http://paid.test/v1"
        api_key_env: "PAID_KEY"
        profile: code
        discovery:
          refresh_interval_ms: 300000
          min_context: 1000
      - base_url: "http://static.test/v1"
        model: "local/final"
"#;
        let config: GatewayConfig = serde_saphyr::from_str(yaml).expect("config de test válida");
        let state = AppState::new(config).expect("estado de test");
        state
            .discovery
            .catalog("http://paid.test/v1")
            .expect("caché del backend de pago")
            .seed(vec![paid_model("acme/flash-a", "0.04", 8_000)]);

        let route = &state.config.route[0];
        let candidates = expand_backends(&state, 0, route).await;

        assert_eq!(
            candidates.len(),
            1,
            "sin lista curada el backend de pago aporta cero candidatos"
        );
        assert_eq!(candidates[0].model, "local/final");
        assert_eq!(
            candidates[0].slot, 1,
            "la ruta sigue con el siguiente backend"
        );
    }

    /// Scenario: el cuerpo de un `429` sin `Content-Length` (chunked) se lee de
    /// forma acotada; un primer fragmento por encima del límite se descarta sin
    /// esperar al resto del cuerpo.
    #[tokio::test]
    async fn read_quota_body_bounds_reads_of_chunked_bodies() {
        use axum::body::Body;
        use axum::routing::get;
        use futures_util::StreamExt as _;
        use futures_util::stream;

        // Primer fragmento válido como JSON pero más grande que el límite.
        let first = format!(r#"{{"error":"{}"}}"#, "x".repeat(QUOTA_BODY_LIMIT + 4096));
        let app = axum::Router::new().route(
            "/big",
            get(move || {
                let first = first.clone();
                async move {
                    let head =
                        stream::once(async move { Ok::<_, std::io::Error>(first.into_bytes()) });
                    // El resto del cuerpo nunca llega: una lectura acotada no
                    // debe esperarlo.
                    Body::from_stream(head.chain(stream::pending()))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve test server") });

        let response = reqwest::get(format!("http://{addr}/big"))
            .await
            .expect("GET /big");
        assert!(
            response.content_length().is_none(),
            "el servidor de test debe responder sin Content-Length"
        );

        let outcome = tokio::time::timeout(Duration::from_millis(500), read_quota_body(response))
            .await
            .expect("la lectura acotada no debe esperar al resto del cuerpo");
        assert!(
            outcome.is_none(),
            "un cuerpo por encima del límite debe descartarse"
        );
    }

    // ---- Grupo 1 «Parseo de usage» del ledger de observabilidad (#39) ----

    /// Scenario: Respuesta no-stream con usage — los tokens quedan extraídos
    /// del objeto `usage` del body.
    #[test]
    fn parse_usage_extracts_tokens_from_response_body() {
        let body = json!({
            "id": "gen-1",
            "choices": [{"message": {"role": "assistant", "content": "hola"}}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 34}
        });

        let usage = parse_usage(&body);

        assert_eq!(usage.prompt_tokens, Some(12));
        assert_eq!(usage.completion_tokens, Some(34));
    }

    /// Scenario: Respuesta no-stream sin usage — el parseo del body completo
    /// (JSON válido) también extrae los tokens.
    #[test]
    fn parse_body_usage_extracts_tokens_from_valid_json() {
        let body = br#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4}}"#;

        let usage = parse_body_usage(body);

        assert_eq!(
            usage,
            TokenUsage {
                prompt_tokens: Some(3),
                completion_tokens: Some(4)
            }
        );
    }

    /// Scenario: Sin usage los tokens quedan desconocidos — y **no** se
    /// registra `0` cuando falta `usage`.
    #[test]
    fn parse_usage_without_usage_reports_unknown_not_zero() {
        let body = json!({"id": "gen-1", "choices": []});

        let usage = parse_usage(&body);

        assert_eq!(usage.prompt_tokens, None);
        assert_eq!(usage.completion_tokens, None);
        assert!(
            usage
                != TokenUsage {
                    prompt_tokens: Some(0),
                    completion_tokens: Some(0)
                },
            "los tokens desconocidos no deben representarse como 0"
        );
    }

    /// Un body que no sea JSON válido produce tokens desconocidos (sin ceros
    /// inventados).
    #[test]
    fn parse_body_usage_invalid_json_is_unknown() {
        assert_eq!(parse_body_usage(b"no es json"), TokenUsage::default());
    }

    /// `usage` parcial (solo `prompt_tokens`): el campo que falta queda
    /// desconocido, sin inventar un valor.
    #[test]
    fn parse_usage_partial_fields_leave_missing_unknown() {
        let body = json!({"usage": {"prompt_tokens": 5}});

        let usage = parse_usage(&body);

        assert_eq!(
            usage,
            TokenUsage {
                prompt_tokens: Some(5),
                completion_tokens: None
            }
        );
    }

    /// Scenario: Streaming inyecta include_usage — un body con `stream: true`
    /// sale con `stream_options.include_usage: true`.
    #[test]
    fn inject_include_usage_adds_include_usage_true() {
        let mut body = json!({"model": "m", "stream": true, "messages": []});

        inject_include_usage(&mut body);

        assert_eq!(
            body.pointer("/stream_options/include_usage"),
            Some(&json!(true)),
            "debe inyectarse stream_options.include_usage: true"
        );
        // El resto del body se conserva intacto.
        assert_eq!(body.get("model"), Some(&json!("m")));
        assert_eq!(body.get("messages"), Some(&json!([])));
    }

    /// La inyección preserva el resto de `stream_options` del cliente y fuerza
    /// `include_usage` a `true` aunque viniera a `false`.
    #[test]
    fn inject_include_usage_forces_true_preserving_client_options() {
        let mut body =
            json!({"stream": true, "stream_options": {"include_usage": false, "foo": 1}});

        inject_include_usage(&mut body);

        assert_eq!(
            body.pointer("/stream_options/include_usage"),
            Some(&json!(true))
        );
        assert_eq!(body.pointer("/stream_options/foo"), Some(&json!(1)));
    }

    /// Scenario: Streaming lee el `usage` del chunk final (los intermedios con
    /// `usage: null` no tapan el valor final).
    #[test]
    fn stream_usage_reads_usage_from_final_chunk() {
        let mut extractor = StreamUsage::default();
        extractor.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}],\"usage\":null}\n\n");

        extractor.feed(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9}}\n\n",
        );
        extractor.feed(b"data: [DONE]\n\n");

        assert_eq!(
            extractor.usage,
            TokenUsage {
                prompt_tokens: Some(7),
                completion_tokens: Some(9)
            }
        );
    }

    /// Una línea `data:` partida entre dos chunks del upstream se reensambla
    /// antes de parsearse: el `usage` del chunk final no se pierde.
    #[test]
    fn stream_usage_reassembles_lines_split_across_chunks() {
        let mut extractor = StreamUsage::default();

        extractor.feed(b"data: {\"choices\":[],\"usage\":{\"prompt_tok");
        extractor.feed(b"ens\":100,\"completion_tokens\":8}}\n\ndata: [DONE]\n\n");

        assert_eq!(
            extractor.usage,
            TokenUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(8)
            }
        );
    }

    /// Un chunk posterior con `usage: null` no resetea el `usage` ya conocido.
    #[test]
    fn stream_usage_null_chunk_does_not_reset_known_usage() {
        let mut extractor = StreamUsage::default();

        extractor.feed(b"data: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9}}\n\n");
        extractor.feed(b"data: {\"choices\":[],\"usage\":null}\n\n");

        assert_eq!(extractor.usage.prompt_tokens, Some(7));
        assert_eq!(extractor.usage.completion_tokens, Some(9));
    }

    /// Scenario: una línea parcial sin `\n` que supera el cap de reensamblado
    /// se descarta (el buffer no crece sin límite) y el stream sigue
    /// funcionando: el `usage` posterior se registra con normalidad.
    #[test]
    fn stream_usage_discards_oversized_partial_line_and_keeps_working() {
        let mut extractor = StreamUsage::default();

        // Línea parcial enorme (sin `\n`) que supera el cap: se descarta.
        let big = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
        extractor.feed(&big);
        assert!(
            extractor.buffer.len() <= MAX_SSE_LINE_BYTES,
            "el buffer de reensamblado no debe crecer sin límite: {} bytes",
            extractor.buffer.len()
        );

        // El resto de la línea descartada (hasta su `\n`) no se interpreta.
        extractor.feed(
            b"resto de la linea descartada\ndata: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n",
        );

        assert_eq!(
            extractor.usage,
            TokenUsage {
                prompt_tokens: Some(3),
                completion_tokens: Some(4)
            }
        );
    }

    /// Scenario: el pre-filtro de `usage` no altera el comportamiento: una
    /// línea que menciona `"usage"` dentro de un string (sin usage real) no
    /// toca el valor retenido, y el chunk final con usage sí lo actualiza.
    #[test]
    fn stream_usage_prefilter_keeps_behavior_identical() {
        let mut extractor = StreamUsage::default();

        extractor.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"the word \\\"usage\\\" here\"}}]}\n\n",
        );
        extractor.feed(b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n");

        assert_eq!(
            extractor.usage,
            TokenUsage {
                prompt_tokens: Some(1),
                completion_tokens: Some(2)
            }
        );
    }

    /// Scenario: el stream termina sin `\n` final (EOF con línea parcial
    /// pendiente): `finish` procesa la última línea para no perder el `usage`
    /// de upstreams que no cierran la línea. Es idempotente si se llama de más.
    #[test]
    fn stream_usage_finish_processes_last_partial_line() {
        let mut extractor = StreamUsage::default();
        extractor.feed(b"data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}");
        assert_eq!(
            extractor.usage,
            TokenUsage::default(),
            "sin \\n aún no se procesa"
        );

        extractor.finish();

        assert_eq!(
            extractor.usage,
            TokenUsage {
                prompt_tokens: Some(5),
                completion_tokens: Some(6)
            }
        );

        // Idempotente: llamar de más no altera el resultado.
        extractor.finish();
        assert_eq!(extractor.usage.prompt_tokens, Some(5));
    }

    /// Scenario: `stream_options: null` del cliente se reemplaza por el objeto
    /// con `include_usage: true` (un `null` no debe silenciar la inyección).
    #[test]
    fn inject_include_usage_replaces_null_stream_options() {
        let mut body = json!({"stream": true, "stream_options": null});

        inject_include_usage(&mut body);

        assert_eq!(
            body.pointer("/stream_options/include_usage"),
            Some(&json!(true)),
            "un stream_options null debe reemplazarse por el objeto con include_usage"
        );
    }

    // --- Evento de petición (`gateway_requests`) -----------------------------

    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::subscriber::DefaultGuard;
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    /// Evento de ejemplo con usage y precios conocidos (1000 in / 200 out a
    /// 2.0/8.0 USD/Mtok → coste 0.0036 USD).
    fn sample_event() -> RequestEvent {
        RequestEvent {
            event_type: "request",
            ts: 1_700_000_000_000,
            request_id: "req-test-1".to_owned(),
            route: "code".to_owned(),
            model_id: Some("acme/flash-a".to_owned()),
            streaming: false,
            attempts: 1,
            failover: false,
            outcome: "success",
            finish_reason: Some("stop".to_owned()),
            latency_ms: 42,
            tokens_in: Some(1000),
            tokens_out: Some(200),
            pin: Some(2.0),
            pout: Some(8.0),
            cost_usd: Some(0.0036),
        }
    }

    /// Escenario 4.1: el evento serializa **todos** los campos del contrato,
    /// con `event_type` `request` (singular).
    #[test]
    fn request_event_serializes_contract_fields() {
        let json = serde_json::to_value(sample_event()).expect("evento serializable");

        assert_eq!(json["event_type"], json!("request"));
        assert_eq!(json["ts"], json!(1_700_000_000_000u64));
        assert_eq!(json["request_id"], json!("req-test-1"));
        assert_eq!(json["route"], json!("code"));
        assert_eq!(json["model_id"], json!("acme/flash-a"));
        assert_eq!(json["streaming"], json!(false));
        assert_eq!(json["attempts"], json!(1));
        assert_eq!(json["failover"], json!(false));
        assert_eq!(json["outcome"], json!("success"));
        assert_eq!(json["finish_reason"], json!("stop"));
        assert_eq!(json["latency_ms"], json!(42));
        assert_eq!(json["tokens_in"], json!(1000));
        assert_eq!(json["tokens_out"], json!(200));
        assert_eq!(json["pin"], json!(2.0));
        assert_eq!(json["pout"], json!(8.0));
        assert_eq!(json["cost_usd"], json!(0.0036));
    }

    /// Escenario «Sin usage el coste queda desconocido»: tokens y coste van
    /// `null` (nunca `0`) y el resto de campos se registran igualmente.
    #[test]
    fn request_event_without_usage_keeps_tokens_and_cost_null() {
        let event = RequestEvent {
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            finish_reason: Some("stop".to_owned()),
            ..sample_event()
        };
        let json = serde_json::to_value(&event).expect("evento serializable");

        assert!(json["tokens_in"].is_null(), "{json}");
        assert!(json["tokens_out"].is_null(), "{json}");
        assert!(json["cost_usd"].is_null(), "{json}");
        assert_eq!(json["route"], json!("code"), "route se registra igualmente");
        assert_eq!(json["model_id"], json!("acme/flash-a"));
        assert_eq!(json["latency_ms"], json!(42));
        assert_eq!(json["outcome"], json!("success"));
        assert_eq!(json["finish_reason"], json!("stop"));
    }

    /// Escenario 4.2: el coste es `tokens_in·pin + tokens_out·pout` en USD
    /// (precios de entrada en USD/Mtok, divididos entre un millón).
    #[test]
    fn cost_usd_combines_tokens_and_prices_in_usd() {
        let cost = cost_usd(Some(1000), Some(200), Some(2.0), Some(8.0))
            .expect("coste con usage y precios completos");
        assert!(
            (cost - 0.0036).abs() < 1e-12,
            "1000·2/1e6 + 200·8/1e6 = 0.0036 USD, coste: {cost}"
        );
        assert_eq!(
            cost_usd(Some(1_000_000), Some(0), Some(2.0), Some(8.0)),
            Some(2.0),
            "un millón de tokens de entrada a 2 USD/Mtok cuesta 2 USD"
        );
    }

    /// Datos parciales no inventan coste: falta cualquier pieza → desconocido.
    #[test]
    fn cost_usd_is_unknown_without_full_usage_or_prices() {
        assert_eq!(cost_usd(None, Some(200), Some(2.0), Some(8.0)), None);
        assert_eq!(cost_usd(Some(1000), None, Some(2.0), Some(8.0)), None);
        assert_eq!(cost_usd(Some(1000), Some(200), None, Some(8.0)), None);
        assert_eq!(cost_usd(Some(1000), Some(200), Some(2.0), None), None);
    }

    /// Un coste `0.0` con precios y tokens conocidos es un **dato** (0 tokens
    /// consumidos), no una ausencia: no se confunde con desconocido.
    #[test]
    fn cost_usd_zero_tokens_with_known_prices_is_zero() {
        assert_eq!(
            cost_usd(Some(0), Some(0), Some(2.0), Some(8.0)),
            Some(0.0),
            "0 tokens con precios conocidos es coste 0, no desconocido"
        );
    }

    /// Escenario 4.1 (no-stream): un único parseo del cuerpo extrae `usage` y
    /// `finish_reason`.
    #[test]
    fn parse_body_ledger_reads_usage_and_finish_reason() {
        let body = br#"{
            "choices": [{"message": {"role": "assistant"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1000, "completion_tokens": 200}
        }"#;

        let (usage, finish_reason) = parse_body_ledger(body);

        assert_eq!(
            usage,
            TokenUsage {
                prompt_tokens: Some(1000),
                completion_tokens: Some(200)
            }
        );
        assert_eq!(finish_reason.as_deref(), Some("stop"));
    }

    /// Escenario «Sin usage»: los tokens quedan desconocidos (nunca `0`) y el
    /// `finish_reason` del upstream se registra igualmente.
    #[test]
    fn parse_body_ledger_without_usage_keeps_tokens_unknown() {
        let body =
            br#"{"choices": [{"message": {"role": "assistant"}, "finish_reason": "stop"}]}"#;

        let (usage, finish_reason) = parse_body_ledger(body);

        assert_eq!(usage, TokenUsage::default(), "sin usage: desconocidos, no 0");
        assert_eq!(finish_reason.as_deref(), Some("stop"));
    }

    /// Un cuerpo que no es JSON válido produce todo desconocido (sin panic).
    #[test]
    fn parse_body_ledger_invalid_json_is_all_unknown() {
        let (usage, finish_reason) = parse_body_ledger(b"<html>gateway error</html>");

        assert_eq!(usage, TokenUsage::default());
        assert_eq!(finish_reason, None);
    }

    /// Escenario 4.1 (streaming): el extractor del stream retiene el
    /// `finish_reason` de los chunks (el chunk final con `usage` no lo trae).
    #[test]
    fn stream_usage_captures_finish_reason_from_chunks() {
        let mut extractor = StreamUsage::default();

        extractor
            .feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"h\"},\"finish_reason\":null}]}\n\n");
        extractor
            .feed(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
        extractor.feed(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":5}}\n\n");
        extractor.feed(b"data: [DONE]\n\n");

        assert_eq!(
            extractor.finish_reason.as_deref(),
            Some("stop"),
            "el finish_reason del chunk de contenido se retiene"
        );
        assert_eq!(extractor.usage.prompt_tokens, Some(3));
        assert_eq!(extractor.usage.completion_tokens, Some(5));
    }

    /// Un `finish_reason: null` intermedio no sobrescribe el último conocido.
    #[test]
    fn stream_usage_finish_reason_null_does_not_overwrite() {
        let mut extractor = StreamUsage::default();

        extractor
            .feed(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
        extractor
            .feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"mas\"},\"finish_reason\":null}]}\n\n");

        assert_eq!(extractor.finish_reason.as_deref(), Some("stop"));
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

    /// Capa que **entra en panic** al registrar el evento `request`: simula un
    /// exportador caído para el aislamiento del emisor (4.3).
    #[derive(Clone)]
    struct PanickingLedgerLayer;

    impl<S: Subscriber> Layer<S> for PanickingLedgerLayer {
        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            let mut visitor = EventFieldsVisitor::default();
            event.record(&mut visitor);
            if visitor.fields.iter().any(|field| field == "event_type=request") {
                panic!("exportador caído");
            }
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

    /// Captura los logs `INFO` solo para el hilo actual.
    fn capture_ledger_logs() -> (Arc<Mutex<Vec<String>>>, DefaultGuard) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(LedgerLayer {
            events: Arc::clone(&events),
        });
        (events, tracing::subscriber::set_default(subscriber))
    }

    /// Suscriptor que entra en panic con el evento `request` (exportador
    /// caído), solo para el hilo actual.
    fn panicking_ledger_subscriber() -> impl Subscriber + Send + Sync {
        tracing_subscriber::registry().with(PanickingLedgerLayer)
    }

    /// El emisor registra el evento como log estructurado con `event_type`
    /// `request` y los campos del contrato (D5).
    #[test]
    fn emit_request_event_logs_contract_fields() {
        let (captured, _guard) = capture_ledger_logs();

        emit_request_event(&sample_event());

        let logs = captured.lock().expect("lock captured ledger logs").clone();
        let request = logs
            .iter()
            .find(|log| log.contains("event_type=request"))
            .expect("el evento de petición se emite como log estructurado");
        assert!(request.contains("request_id=req-test-1"), "{request}");
        assert!(request.contains("route=code"), "{request}");
        assert!(request.contains("model_id=acme/flash-a"), "{request}");
        assert!(request.contains("streaming=false"), "{request}");
        assert!(request.contains("attempts=1"), "{request}");
        assert!(request.contains("failover=false"), "{request}");
        assert!(request.contains("outcome=success"), "{request}");
        assert!(request.contains("finish_reason=stop"), "{request}");
        assert!(request.contains("latency_ms=42"), "{request}");
        assert!(request.contains("tokens_in=1000"), "{request}");
        assert!(request.contains("tokens_out=200"), "{request}");
        assert!(request.contains("pin=2"), "{request}");
        assert!(request.contains("pout=8"), "{request}");
        assert!(request.contains("cost_usd=0.0036"), "{request}");
    }

    /// Escenario 4.3 (emisor): un fallo del exportador —aquí, un panic en el
    /// subscriber al registrar el evento— queda aislado y no se propaga a la
    /// petición.
    #[test]
    fn emit_request_event_survives_panicking_exporter() {
        let event = sample_event();

        let survived = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracing::subscriber::with_default(panicking_ledger_subscriber(), || {
                emit_request_event(&event);
            });
        }));

        assert!(
            survived.is_ok(),
            "el fallo del exportador debe quedar aislado en el emisor"
        );
    }
}
